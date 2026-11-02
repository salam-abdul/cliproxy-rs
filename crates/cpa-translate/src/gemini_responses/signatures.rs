use std::collections::{BTreeMap, HashSet};

use base64::Engine as _;
use cpa_common::json::{self as gj, Res};
use cpa_common::signature::{self as sig, BlockKind, Provider};
use cpa_core::config::go_trim_space;
use sha2::{Digest, Sha256};

use super::{
    ANY, CARRIER_PREFIX, DIRECTION_FIELD, FUNCTION, NEXT, PREVIOUS, SIGNATURE_FIELD, STANDALONE, SUMMARY_FIELD,
    TARGET_FIELD, TEXT, assistant_visible_text, field, is, item_type, trimmed,
};

// ---------------------------------------------------------------------------------------
// Go base64 (encoding/base64 skips CR and LF and accepts non-zero trailing bits)

pub(crate) fn go_base64(padded: bool) -> base64::engine::GeneralPurpose {
    use base64::engine::{DecodePaddingMode, GeneralPurposeConfig};
    base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_encode_padding(padded)
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(if padded {
                DecodePaddingMode::RequireCanonical
            } else {
                DecodePaddingMode::RequireNone
            }),
    )
}

pub(crate) fn go_base64_decode(input: &[u8], padded: bool) -> Option<Vec<u8>> {
    let input: Vec<u8> = input.iter().copied().filter(|&c| c != b'\r' && c != b'\n').collect();
    go_base64(padded).decode(input).ok()
}

// ---------------------------------------------------------------------------------------
// Carriers (signature_carrier.go)

/// encodeGeminiResponsesCarrier.
pub(crate) fn encode_carrier(raw_signature: &[u8], direction: &str, target: &str) -> Vec<u8> {
    let raw = go_trim_space(raw_signature);
    if raw.is_empty() {
        return vec![];
    }
    let mut out = CARRIER_PREFIX.to_vec();
    out.extend_from_slice(direction.as_bytes());
    out.push(b':');
    out.extend_from_slice(target.as_bytes());
    out.push(b':');
    out.extend_from_slice(go_base64(false).encode(raw).as_bytes());
    out
}

struct Decoded {
    signature: Vec<u8>,
    direction: Vec<u8>,
    target: Vec<u8>,
    marked: bool,
    ok: bool,
}

/// decodeGeminiResponsesCarrier.
fn decode_carrier(raw_signature: &[u8]) -> Decoded {
    let raw = go_trim_space(raw_signature);
    let fail = Decoded {
        signature: vec![],
        direction: vec![],
        target: vec![],
        marked: true,
        ok: false,
    };
    let Some(rest) = raw.strip_prefix(CARRIER_PREFIX) else {
        return Decoded {
            signature: raw.to_vec(),
            direction: vec![],
            target: vec![],
            marked: false,
            ok: true,
        };
    };
    if raw.len() > sig::MAX_GEMINI_THOUGHT_SIGNATURE_LEN * 4 / 3 + 1024 {
        return fail;
    }
    let mut fields = rest.splitn(3, |&c| c == b':');
    let (Some(direction), Some(target), Some(payload)) = (fields.next(), fields.next(), fields.next()) else {
        return fail;
    };
    if ![NEXT, PREVIOUS, STANDALONE].iter().any(|d| d.as_bytes() == direction)
        || ![TEXT, FUNCTION, ANY].iter().any(|t| t.as_bytes() == target)
    {
        return fail;
    }
    match go_base64_decode(payload, false) {
        Some(decoded) if !decoded.is_empty() && !decoded.starts_with(CARRIER_PREFIX) => Decoded {
            signature: decoded,
            direction: direction.to_vec(),
            target: target.to_vec(),
            marked: true,
            ok: true,
        },
        _ => fail,
    }
}

/// compatibleGeminiResponsesCarrierSignature: a Gemini-compatible, non-bypass signature.
pub(crate) fn compatible_carrier_signature(raw: &[u8], target: &[u8]) -> Option<Vec<u8>> {
    let kind = if target == FUNCTION.as_bytes() {
        BlockKind::GeminiFunctionCall
    } else {
        BlockKind::GeminiModelPart
    };
    let normalized = sig::compatible_signature_for_provider_block(Provider::Gemini, raw, kind)?;
    if sig::is_gemini_bypass(sig::payload_without_provider_prefix(normalized.as_bytes())) {
        return None;
    }
    Some(normalized.into_bytes())
}

/// isOpenAIResponsesDetachedCarrier: a signature-only reasoning item.
pub(super) fn is_detached_carrier(item: &Res<'_>) -> bool {
    item_type(item) == b"reasoning"
        && !trimmed(item, "encrypted_content").is_empty()
        && trimmed(item, "summary.0.text").is_empty()
}

fn semantic_target(item: &Res<'_>) -> &'static str {
    match item_type(item).as_slice() {
        b"function_call" | b"custom_tool_call" => return FUNCTION,
        b"reasoning" if !trimmed(item, "summary.0.text").is_empty() => return TEXT,
        _ => {}
    }
    if assistant_visible_text(item).is_some() {
        TEXT
    } else {
        ""
    }
}

/// geminiResponsesCarrierMatchesAdjacent: the nearest non-carrier item in the carrier's
/// direction has the carrier's target kind.
fn matches_adjacent(items: &[Res<'_>], index: usize, direction: &[u8], target: &[u8]) -> bool {
    let step: isize = if is(direction, PREVIOUS) { -1 } else { 1 };
    let mut adjacent = index as isize + step;
    while adjacent >= 0 && (adjacent as usize) < items.len() {
        let item = &items[adjacent as usize];
        let kind = semantic_target(item);
        if !kind.is_empty() {
            return is(target, ANY) || is(target, kind);
        }
        if !is_detached_carrier(item) {
            return false;
        }
        adjacent += step;
    }
    false
}

fn has_internal_carrier_fields(item: &Res<'_>) -> bool {
    [DIRECTION_FIELD, TARGET_FIELD, SIGNATURE_FIELD, SUMMARY_FIELD]
        .iter()
        .any(|f| item.get(*f).exists())
}

/// stripGeminiResponsesCarrierMetadata: `json.Unmarshal` into `map[string]json.RawMessage`,
/// drop the internal fields, `json.Marshal` (sorted keys, compacted HTML-escaped values;
/// the last duplicate key wins).
fn strip_carrier_metadata(raw: &[u8]) -> Option<Vec<u8>> {
    if !gj::std_valid(raw) {
        return None;
    }
    let root = gj::parse(raw);
    if !root.is_object() {
        return None;
    }
    let mut fields: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    root.each(|key, value| {
        let key = gj::go_unquote(&key.raw).map(String::into_bytes).unwrap_or_default();
        fields.insert(key, value.raw.to_vec());
        true
    });
    for f in [DIRECTION_FIELD, TARGET_FIELD, SIGNATURE_FIELD, SUMMARY_FIELD] {
        fields.remove(f.as_bytes());
    }
    let mut out = vec![b'{'];
    for (i, (key, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        gj::marshal_str(&mut out, key, true);
        out.push(b':');
        out.extend_from_slice(&gj::compact(value, true));
    }
    out.push(b'}');
    Some(out)
}

pub(super) fn owned(raw: &[u8]) -> Res<'static> {
    gj::parse(raw).into_owned()
}

/// normalizeGeminiResponsesCarriers: decodes carriers, keeps those whose signature is
/// Gemini-compatible and whose neighbour matches, and reports whether any valid carrier
/// (or compatible raw signature) is present.
pub(super) fn normalize_carriers<'a>(items: &[Res<'a>]) -> (Vec<Res<'a>>, bool) {
    let mut normalized = Vec::with_capacity(items.len());
    let mut has_valid = false;
    for (index, original) in items.iter().enumerate() {
        let mut item = original.clone();
        let mut item_json: Option<Vec<u8>> = None;
        if has_internal_carrier_fields(original)
            && let Some(stripped) = strip_carrier_metadata(&original.raw)
        {
            item = owned(&stripped);
            item_json = Some(stripped);
        }
        if item_type(&item) != b"reasoning" {
            normalized.push(item);
            continue;
        }
        let mut item_json = item_json.unwrap_or_else(|| item.raw.to_vec());
        let raw_signature = trimmed(&item, "encrypted_content");
        let decoded = decode_carrier(&raw_signature);
        if !decoded.marked {
            if !raw_signature.is_empty() {
                has_valid |= compatible_carrier_signature(&raw_signature, ANY.as_bytes()).is_some();
            }
            normalized.push(item);
            continue;
        }
        let (direction, target) = (decoded.direction, decoded.target);
        let mut signature = None;
        if decoded.ok {
            signature = compatible_carrier_signature(&decoded.signature, &target);
        }
        let mut ok = signature.is_some();
        if ok && !is(&direction, STANDALONE) {
            ok = matches_adjacent(items, index, &direction, &target);
        }
        let has_summary = !trimmed(&item, "summary.0.text").is_empty();
        let valid_summary_carrier = has_summary
            && ((is(&direction, STANDALONE) && (is(&target, TEXT) || is(&target, ANY))) || is(&direction, NEXT));
        if !ok || (!is_detached_carrier(&item) && !valid_summary_carrier) {
            if !has_summary {
                continue;
            }
            gj::delete(&mut item_json, "encrypted_content");
            normalized.push(owned(&item_json));
            continue;
        }
        has_valid = true;
        gj::set_str(&mut item_json, "encrypted_content", signature.unwrap_or_default());
        gj::set_str(&mut item_json, DIRECTION_FIELD, &direction);
        gj::set_str(&mut item_json, TARGET_FIELD, &target);
        normalized.push(owned(&item_json));
    }
    (normalized, has_valid)
}

// ---------------------------------------------------------------------------------------
// Trailing text signatures (trailing_signature.go)

fn sha256_hex(text: &[u8]) -> String {
    cpa_core::hex::encode(Sha256::digest(text))
}

fn replay_model(model: &str) -> String {
    cpa_common::thinking::parse_suffix(model).model_name
}

fn replay_session(message_id: &[u8]) -> String {
    format!("gemini-responses-text:{}", String::from_utf8_lossy(message_id))
}

/// cacheGeminiResponsesTextSignatures: remembers signatures that trailed a visible message
/// so the next request can restore them; false when any is not a usable text signature.
pub(crate) fn cache_text_signatures(model: &str, message_id: &[u8], text: &[u8], signatures: &[Vec<u8>]) -> bool {
    if message_id.is_empty() || text.is_empty() {
        return false;
    }
    let hash = sha256_hex(text);
    let mut items = vec![];
    for signature in signatures {
        if compatible_carrier_signature(signature, TEXT.as_bytes()).is_none() {
            return false;
        }
        let mut item = br#"{"type":"thought_signature","targetKind":"text"}"#.to_vec();
        gj::set_str(&mut item, "thoughtSignature", signature);
        gj::set_str(&mut item, "targetHash", &hash);
        items.push(item);
    }
    crate::replay_cache::put(&replay_model(model), &replay_session(message_id), &items)
}

/// restoreGeminiResponsesTextSignatures: re-inserts cached signatures as `previous`
/// carriers after the assistant message whose text they were cached for.
pub(super) fn restore_text_signatures<'a>(model: &str, items: &[Res<'a>]) -> Vec<Res<'a>> {
    let mut restored = Vec::with_capacity(items.len());
    let mut skip: HashSet<usize> = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        if skip.contains(&index) {
            continue;
        }
        restored.push(item.clone());
        let message_id = trimmed(item, "id");
        let Some(text) = assistant_visible_text(item) else {
            continue;
        };
        if message_id.is_empty() {
            continue;
        }
        let Some(cached) = crate::replay_cache::get(&replay_model(model), &replay_session(&message_id)) else {
            continue;
        };
        let mut replayed: HashSet<Vec<u8>> = HashSet::new();
        let hash = sha256_hex(&text);
        for raw in cached {
            let entry = gj::parse(&raw);
            let signature = entry.get("thoughtSignature").bytes().into_owned();
            if entry.get("targetHash").bytes().as_ref() != hash.as_bytes() {
                continue;
            }
            let mut carrier = br#"{"type":"reasoning","summary":[]}"#.to_vec();
            gj::set_str(
                &mut carrier,
                "encrypted_content",
                encode_carrier(&signature, PREVIOUS, TEXT),
            );
            restored.push(owned(&carrier));
            replayed.insert(signature);
        }
        let mut adjacent = index + 1;
        while adjacent < items.len() && is_detached_carrier(&items[adjacent]) {
            let d = decode_carrier(&field(&items[adjacent], "encrypted_content"));
            if d.ok && is(&d.direction, PREVIOUS) && is(&d.target, TEXT) && replayed.contains(&d.signature) {
                skip.insert(adjacent);
            }
            adjacent += 1;
        }
    }
    restored
}
