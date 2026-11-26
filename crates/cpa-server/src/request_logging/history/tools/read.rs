//! Tool and skill measures over a range, read from the tool index on a query-only
//! connection. Measures count conversations, not requests: a conversation is in range
//! when one of its sends is, and the tools it carried are its definitions on any date.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};

use super::store::INDEX_FILE;
use crate::activity::Range;

/// The tables [`super::store::Index::open`] creates. Until all exist, the indexer is
/// still creating the file and nothing was indexed.
const TABLES: [&str; 6] = ["files", "sends", "messages", "defs", "calls", "results"];

/// Dead weight: carried by this share of conversations with tools, with no call of
/// the tool in the range.
const DEAD_SHARE_PERCENT: i64 = 20;
/// Failing: at least this many calls and this share of results in error.
const FAILING_CALLS: i64 = 10;
const FAILING_SHARE_PERCENT: i64 = 20;
/// Bloating: a p95 result of at least 100 KiB.
const BLOATING_BYTES: i64 = 102_400;

/// The measures of the history folder `dir`'s tool index over `range` ending at
/// `now_ms`, for sends of `provider` when one is given. No index file is the empty answer.
pub(crate) fn read(dir: &Path, range: Range, provider: Option<&str>, now_ms: i64) -> rusqlite::Result<Value> {
    let path = dir.join(INDEX_FILE);
    if !path.is_file() {
        return Ok(Answer::default().json(range));
    }
    // Read-write without create, then query-only: the index is in WAL mode, and once
    // the indexer's connection closes its side files are gone, which a read-only
    // connection can neither recreate reliably nor remove when it closes.
    let mut db = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_secs(2))?;
    db.pragma_update(None, "query_only", true)?;
    let tx = db.transaction()?;
    let tables: i64 = tx.query_row(
        &format!(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ({})",
            TABLES.map(|t| format!("'{t}'")).join(",")
        ),
        [],
        |r| r.get(0),
    )?;
    if tables < count(TABLES.len()) {
        return Ok(Answer::default().json(range));
    }
    let from = now_ms.saturating_sub(range.duration());
    let mut answer = Answer::default();
    tx.query_row(
        "SELECT COUNT(*), COALESCE(SUM(skipped_format), 0), COALESCE(SUM(cut), 0), MIN(at_ms), MAX(at_ms)
         FROM sends WHERE at_ms >= ?1 AND at_ms <= ?2 AND (?3 IS NULL OR provider = ?3)",
        params![from, now_ms, provider],
        |r| {
            answer.sends = r.get(0)?;
            answer.skipped_format = r.get(1)?;
            answer.skipped_cut = r.get(2)?;
            answer.covered = r.get::<_, Option<i64>>(3)?.zip(r.get::<_, Option<i64>>(4)?);
            Ok(())
        },
    )?;
    answer.unreadable_files = tx.query_row("SELECT COUNT(*) FROM files WHERE unreadable", [], |r| r.get(0))?;

    // The largest definition of each tool in each conversation in range.
    let mut with_tools: HashSet<i64> = HashSet::new();
    {
        let mut stmt = tx.prepare(
            "SELECT conv, tool, MAX(bytes) FROM defs
             WHERE (?3 IS NULL OR provider = ?3) AND conv IN (
                 SELECT conv FROM sends
                 WHERE at_ms >= ?1 AND at_ms <= ?2 AND (?3 IS NULL OR provider = ?3) AND conv IS NOT NULL)
             GROUP BY conv, tool",
        )?;
        let mut rows = stmt.query(params![from, now_ms, provider])?;
        while let Some(r) = rows.next()? {
            let conv: i64 = r.get(0)?;
            with_tools.insert(conv);
            answer
                .tools
                .entry(r.get(1)?)
                .or_default()
                .carriers
                .insert(conv, r.get(2)?);
        }
    }
    answer.conversations = i64::try_from(with_tools.len()).unwrap_or(i64::MAX);

    // Calls by the first copy of their ID, with the first copy of their result: a retry,
    // failover or resend repeats both. A call's conversation is that of its earliest copy
    // with one, since history that starts mid-conversation leaves early copies unlinked.
    // Only IDs with a copy in the range can have their first copy there.
    let mut stmt = tx.prepare(
        "WITH ids AS (
             SELECT DISTINCT id FROM calls
             WHERE at_ms >= ?1 AND at_ms <= ?2 AND (?3 IS NULL OR provider = ?3)),
         firsts AS (
             SELECT id, at_ms, tool, skill,
                    ROW_NUMBER() OVER (PARTITION BY id ORDER BY at_ms, rel) AS n,
                    FIRST_VALUE(conv) OVER (
                        PARTITION BY id ORDER BY conv IS NULL, at_ms, rel
                        ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) AS conv
             FROM calls WHERE id IN ids AND (?3 IS NULL OR provider = ?3)),
         outcomes AS (
             SELECT id, is_error, bytes, loaded,
                    ROW_NUMBER() OVER (PARTITION BY id ORDER BY at_ms, rel) AS n
             FROM results WHERE id IN ids)
         SELECT f.conv, f.tool, f.skill, o.is_error, o.bytes, o.loaded
         FROM firsts f LEFT JOIN outcomes o ON o.id = f.id AND o.n = 1
         WHERE f.n = 1 AND f.at_ms >= ?1 AND f.at_ms <= ?2",
    )?;
    let mut rows = stmt.query(params![from, now_ms, provider])?;
    while let Some(r) = rows.next()? {
        let conv: Option<i64> = r.get(0)?;
        let skill: Option<String> = r.get(2)?;
        let outcome: Option<(bool, i64, i64)> = match r.get::<_, Option<bool>>(3)? {
            Some(is_error) => Some((is_error, r.get(4)?, r.get(5)?)),
            None => None,
        };
        let tool = answer.tools.entry(r.get(1)?).or_default();
        tool.calls += 1;
        if let Some(conv) = conv.filter(|conv| tool.carriers.contains_key(conv)) {
            tool.carrying_calls += 1;
            tool.used.insert(conv);
        }
        if let Some((is_error, bytes, _)) = outcome {
            tool.errors += i64::from(is_error);
            tool.results.push(bytes);
        }
        if let Some(skill) = skill {
            let skill = answer.skills.entry(skill).or_default();
            skill.calls += 1;
            skill.conversations.extend(conv);
            if let Some((is_error, _, loaded)) = outcome {
                skill.errors += i64::from(is_error);
                skill.loaded.push(loaded);
            }
        }
    }
    Ok(answer.json(range))
}

#[derive(Default)]
struct Tool {
    /// Each carrying conversation in range, with its largest definition of the tool.
    carriers: HashMap<i64, i64>,
    calls: i64,
    /// Calls made in a carrying conversation.
    carrying_calls: i64,
    /// Carrying conversations with a call in range.
    used: HashSet<i64>,
    errors: i64,
    /// Result bytes, one per call with a result.
    results: Vec<i64>,
}

impl Tool {
    fn carried(&self) -> i64 {
        count(self.carriers.len())
    }

    /// Weight times carried: the sum of the carrying conversations' definition bytes.
    fn score(&self) -> i64 {
        self.carriers
            .values()
            .fold(0, |sum: i64, bytes| sum.saturating_add(*bytes))
    }
}

#[derive(Default)]
struct Skill {
    calls: i64,
    conversations: HashSet<i64>,
    errors: i64,
    loaded: Vec<i64>,
}

#[derive(Default)]
struct Server {
    tools: i64,
    /// Conversations in range that defined any of its tools.
    carried: HashSet<i64>,
    calls: i64,
    errors: i64,
}

#[derive(Default)]
struct Answer {
    covered: Option<(i64, i64)>,
    conversations: i64,
    sends: i64,
    skipped_format: i64,
    skipped_cut: i64,
    unreadable_files: i64,
    tools: BTreeMap<String, Tool>,
    skills: BTreeMap<String, Skill>,
}

impl Answer {
    fn json(self, range: Range) -> Value {
        let conversations = self.conversations;
        let heavy = heavy(&self.tools);
        let mut tools: Vec<(&String, &Tool)> = self.tools.iter().collect();
        tools.sort_by(|(a_name, a), (b_name, b)| {
            b.carried()
                .cmp(&a.carried())
                .then(b.calls.cmp(&a.calls))
                .then(a_name.cmp(b_name))
        });
        let mut servers: BTreeMap<&str, Server> = BTreeMap::new();
        let mut tool_rows = Vec::with_capacity(tools.len());
        for (name, tool) in tools {
            let mut sizes = tool.results.clone();
            sizes.sort_unstable();
            let carried = tool.carried();
            let used = count(tool.used.len());
            let results = count(sizes.len());
            let p95 = nearest_rank(&sizes, 95);
            let mut labels = Vec::new();
            if carried > 0 && tool.calls == 0 && carried.saturating_mul(100) >= DEAD_SHARE_PERCENT * conversations {
                labels.push("dead");
            }
            if heavy.contains(name.as_str()) {
                labels.push("heavy");
            }
            if tool.calls >= FAILING_CALLS
                && results > 0
                && tool.errors.saturating_mul(100) >= FAILING_SHARE_PERCENT * results
            {
                labels.push("failing");
            }
            if p95.is_some_and(|p95| p95 >= BLOATING_BYTES) {
                labels.push("bloating");
            }
            let server = server_of(name);
            if let Some(server) = server {
                let entry = servers.entry(server).or_default();
                entry.tools += 1;
                entry.carried.extend(tool.carriers.keys());
                entry.calls += tool.calls;
                entry.errors += tool.errors;
            }
            tool_rows.push(json!({
                "name": name, "server": server, "carried": carried,
                "carriedShare": ratio(carried, conversations), "used": used,
                "weight": ratio(tool.score(), carried), "calls": tool.calls,
                "callsPerConversation": ratio(tool.carrying_calls, carried),
                "results": results, "errors": tool.errors, "failureRate": ratio(tool.errors, results),
                "resultMean": mean(&sizes), "resultP95": p95, "resultTotal": total(&sizes),
                "labels": labels,
            }));
        }
        let mut servers: Vec<(&str, Server)> = servers.into_iter().collect();
        servers.sort_by(|(a_name, a), (b_name, b)| {
            b.carried
                .len()
                .cmp(&a.carried.len())
                .then(b.calls.cmp(&a.calls))
                .then(a_name.cmp(b_name))
        });
        let servers: Vec<Value> = servers
            .into_iter()
            .map(|(name, server)| {
                json!({"name": name, "tools": server.tools, "carried": server.carried.len(),
                       "calls": server.calls, "errors": server.errors})
            })
            .collect();
        let mut skills: Vec<(String, Skill)> = self.skills.into_iter().collect();
        skills.sort_by(|(a_name, a), (b_name, b)| b.calls.cmp(&a.calls).then(a_name.cmp(b_name)));
        let skills: Vec<Value> = skills
            .into_iter()
            .map(|(name, mut skill)| {
                skill.loaded.sort_unstable();
                json!({"name": name, "calls": skill.calls, "conversations": skill.conversations.len(),
                       "errors": skill.errors, "loadedMean": mean(&skill.loaded),
                       "loadedP95": nearest_rank(&skill.loaded, 95)})
            })
            .collect();
        json!({
            "range": range.name(),
            "covered": self.covered.map(|(from, to)| json!({"fromMs": from, "toMs": to})),
            "conversations": conversations, "sends": self.sends, "skippedFormat": self.skipped_format,
            "skippedCut": self.skipped_cut, "unreadableFiles": self.unreadable_files,
            "tools": tool_rows, "servers": servers, "skills": skills,
        })
    }
}

/// The top `ceil(n / 10)` of the `n` carried tools by weight times carried, ties by name.
fn heavy(tools: &BTreeMap<String, Tool>) -> HashSet<&str> {
    let mut carried: Vec<(&str, i64)> = tools
        .iter()
        .filter(|(_, tool)| !tool.carriers.is_empty())
        .map(|(name, tool)| (name.as_str(), tool.score()))
        .collect();
    carried.sort_by(|(a_name, a), (b_name, b)| b.cmp(a).then(a_name.cmp(b_name)));
    let top = carried.len().div_ceil(10);
    carried.into_iter().take(top).map(|(name, _)| name).collect()
}

/// The MCP server of a tool named `mcp__<server>__<tool>`.
fn server_of(name: &str) -> Option<&str> {
    let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some(server)
}

/// The nearest-rank `p`th percentile of ascending `sorted`: `sorted[ceil(p * n / 100) - 1]`.
fn nearest_rank(sorted: &[i64], p: usize) -> Option<i64> {
    let rank = p.saturating_mul(sorted.len()).div_ceil(100);
    sorted.get(rank.checked_sub(1)?).copied()
}

fn mean(values: &[i64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().map(|v| *v as f64).sum::<f64>() / values.len() as f64)
}

fn ratio(part: i64, whole: i64) -> Option<f64> {
    (whole != 0).then(|| part as f64 / whole as f64)
}

fn total(values: &[i64]) -> i64 {
    values.iter().fold(0, |sum: i64, v| sum.saturating_add(*v))
}

fn count(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
