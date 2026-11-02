use super::{Account, State, now, policy, quota};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Availability {
    Disabled,
    Signin,
    Qualification,
    Cooling,
    Included,
    Extra,
    Limited,
    Unknown,
}

impl Availability {
    fn for_account(
        account: &Account,
        model: &str,
        included: bool,
        cooling: bool,
        allow_extra: bool,
        qualification: bool,
        at: u64,
    ) -> Self {
        if account.disabled {
            Self::Disabled
        } else if account.expired || !account.enabled {
            Self::Signin
        } else if qualification {
            Self::Qualification
        } else if cooling {
            Self::Cooling
        } else if included {
            Self::Included
        } else if allow_extra && quota::extra_available(&account.quota) {
            Self::Extra
        } else if quota::recovery(&account.quota, model, at) == quota::Recovery::Unknown {
            Self::Unknown
        } else {
            Self::Limited
        }
    }

    fn available(self) -> bool {
        matches!(self, Self::Included | Self::Extra)
    }
}

fn selection_order<'a>(
    accounts: impl Iterator<Item = (&'a Account, Availability)>,
    at: u64,
    headroom_check: Option<(&str, f64)>,
) -> Vec<&'a str> {
    let mut candidates: Vec<_> = accounts.filter(|(_, state)| state.available()).collect();
    candidates.sort_by_key(|(a, state)| {
        (
            *state == Availability::Extra,
            policy::new_session_key(a, at, headroom_check),
        )
    });
    candidates.into_iter().map(|(a, _)| a.id.as_str()).collect()
}

impl State {
    pub(crate) async fn status(self: &Arc<Self>) -> anyhow::Result<Value> {
        self.status_for("claude").await
    }

    pub(crate) async fn status_for(self: &Arc<Self>, model: &str) -> anyhow::Result<Value> {
        let mut all = Vec::new();
        let allow_extra = self.allow_extra_usage();
        let accounts = self.quota_accounts(self.accounts()?).await;
        let at = now();
        let mut valid_until = at + 15000;
        let mut ready = false;
        let mut states = Vec::new();
        {
            let routing = self.routing.lock().unwrap_or_else(PoisonError::into_inner);
            for a in &accounts {
                let included = self.included_available(a, &a.quota, model);
                let cooldown = routing.cooldowns.until(&a.id, model);
                let availability = Availability::for_account(
                    a,
                    model,
                    included,
                    cooldown > at,
                    allow_extra,
                    self.config.qualification,
                    at,
                );
                ready |= availability.available();
                states.push((a, availability));
                if let Some(reset) = quota::recovery(&a.quota, model, at).reset() {
                    valid_until = valid_until.min(reset.at);
                }
                let q = (!a.quota.is_null()).then_some(&a.quota);
                all.push(json!({
                    "id": a.id, "fileId": a.file_id, "name": a.name,
                    "enabled": a.enabled, "disabled": a.disabled, "expired": a.expired,
                    "availability": availability, "retryAt": (cooldown > at).then_some(cooldown),
                    "includedQuota": q.map(|q| json!({
                        "fiveHour": q["five_hour"]["utilization"],
                        "fiveHourResetsAt": q["five_hour"]["resets_at"],
                        "weekly": q["seven_day"]["utilization"],
                        "weeklyResetsAt": q["seven_day"]["resets_at"],
                        "eligible": included,
                        "limits": {
                            "five_hour": q["five_hour"], "seven_day": q["seven_day"],
                            "seven_day_oauth_apps": q["seven_day_oauth_apps"],
                            "seven_day_sonnet": q["seven_day_sonnet"],
                            "seven_day_opus": q["seven_day_opus"]
                        }
                    }))
                }));
            }
        }
        let selection_order = selection_order(
            states.into_iter(),
            at,
            (!self.config.selection_shadow).then_some((model, self.config.new_session_min_remaining)),
        );
        let state = self.clone();
        let counts = tokio::task::spawn_blocking(move || {
            let mut counts = BTreeMap::new();
            for id in state
                .routing
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .affinity
                .accounts()
            {
                *counts.entry(id.clone()).or_insert(0) += 1;
            }
            counts
        })
        .await?;
        let snapshot = self.rt.native_events.snapshot();
        let recent: Vec<_> = snapshot["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["type"] == "inference")
            .take(10)
            .cloned()
            .collect();
        let status = json!({
            "enabled": true, "model": model, "observedAt": at, "validUntil": valid_until,
            "allowExtraUsage": allow_extra, "selectionStrategy": "soonest-reset", "ready": ready,
            "selectionOrder": selection_order,
            "newSessionMinRemaining": if self.config.selection_shadow { 0.0 } else { self.config.new_session_min_remaining },
            "ownerReady": true, "proxyReady": true, "qualification": self.config.qualification,
            "accounts": all,
            "affinity": counts.into_iter().map(|(a,n)| json!({"account":a,"sessions":n})).collect::<Vec<_>>(),
            "recent": recent
        });
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_order_matches_new_session_selection_and_places_paid_tier_last() {
        use super::super::{policy, routing::RoutingState};
        use std::collections::HashSet;
        let at = 2_000_000_000_000;
        let make = |id: &str, used: f64, reset: &str| -> Account {
            serde_json::from_value(json!({"id":id,"token":"synthetic","enabled":true,
                "quota":{"five_hour":{"utilization":used},"seven_day":{"utilization":used,"resets_at":reset}}}))
            .unwrap()
        };
        let pool = vec![
            make("low", 98., "2033-05-18T04:33:20Z"),
            make("later", 20., "2033-05-19T04:33:20Z"),
            make("earlier", 20., "2033-05-18T05:33:20Z"),
            make("unknown", 20., "invalid"),
        ];
        for shadow in [true, false] {
            let check = (!shadow).then_some(("opus", 5.));
            let ordered = selection_order(pool.iter().map(|a| (a, Availability::Included)), at, check);
            assert_eq!(
                ordered,
                if shadow {
                    vec!["low", "earlier", "later", "unknown"]
                } else {
                    vec!["earlier", "later", "unknown", "low"]
                }
            );
            let routing = RoutingState::default();
            let mut tried = HashSet::new();
            for id in ordered {
                let selected = if shadow {
                    policy::choose_account("new", &pool, &routing, &tried, at)
                } else {
                    policy::proposed("new", &pool, &routing, &tried, at, "opus", 5.)
                }
                .unwrap();
                assert_eq!(id, selected.id);
                tried.insert(id.to_owned());
            }
        }
        let states = [
            Availability::Extra,
            Availability::Included,
            Availability::Cooling,
            Availability::Disabled,
        ];
        assert_eq!(selection_order(pool.iter().zip(states), at, None), ["later", "low"]);
        for excluded in [
            Availability::Signin,
            Availability::Qualification,
            Availability::Limited,
            Availability::Unknown,
        ] {
            assert!(selection_order(pool.iter().map(|a| (a, excluded)), at, None).is_empty());
        }
    }

    #[test]
    fn status_preserves_admission_precedence_and_unknowns() {
        let mut a: Account = serde_json::from_value(json!({"id":"a","token":"synthetic","enabled":true,"quota":{"five_hour":{"utilization":10},"seven_day":{"utilization":10},"seven_day_opus":{"utilization":100,"resets_at":"2033-05-18T04:33:20Z"}}})).unwrap();
        let at = 2_000_000_000_000;
        let classify = |a: &Account, model, cooling, allow_extra, qualification| {
            Availability::for_account(
                a,
                model,
                super::super::eligible(&a.quota, model),
                cooling,
                allow_extra,
                qualification,
                at,
            )
        };
        assert_eq!(classify(&a, "sonnet", false, false, false), Availability::Included);
        assert_eq!(classify(&a, "opus", false, false, false), Availability::Limited);
        assert_eq!(classify(&a, "sonnet", true, false, false), Availability::Cooling);
        assert_eq!(classify(&a, "sonnet", false, false, true), Availability::Qualification);
        a.quota = serde_json::Value::Null;
        assert_eq!(classify(&a, "sonnet", false, true, false), Availability::Unknown);
        a.quota = json!({"extra_usage":{"is_enabled":true,"monthly_limit":100,"used_credits":0}});
        assert_eq!(classify(&a, "sonnet", false, true, false), Availability::Extra);
        assert_eq!(classify(&a, "sonnet", false, false, false), Availability::Unknown);
        a.expired = true;
        assert_eq!(classify(&a, "sonnet", false, true, false), Availability::Signin);
        a.disabled = true;
        assert_eq!(classify(&a, "sonnet", false, true, false), Availability::Disabled);
    }
}
