use cpa_common::json::Res;
use cpa_core::config::go_trim_space;

use super::signatures::go_base64_decode;
use super::{field, trimmed};
use crate::common::{file_ext, go_lower, normalize_openai_file_data};

/// firstNonEmpty over the values at `paths`: the first non-blank one, trimmed.
pub(super) fn first_non_empty(block: &Res<'_>, paths: &[&str]) -> Vec<u8> {
    paths
        .iter()
        .map(|p| trimmed(block, p))
        .find(|v| !v.is_empty())
        .unwrap_or_default()
}

fn has_prefix_fold(s: &[u8], prefixes: &[&[u8]]) -> bool {
    let s = go_trim_space(s);
    prefixes
        .iter()
        .any(|p| s.len() >= p.len() && s[..p.len()].eq_ignore_ascii_case(p))
}

fn is_data_url(raw: &[u8]) -> bool {
    has_prefix_fold(raw, &[b"data:"])
}

pub(super) fn is_remote_url(raw: &[u8]) -> bool {
    has_prefix_fold(raw, &[b"http://", b"https://", b"gs://"])
}

pub(super) fn is_generic_mime(mime: &[u8]) -> bool {
    matches!(
        go_lower(go_trim_space(mime)).as_slice(),
        b"" | b"application/octet-stream" | b"binary/octet-stream"
    )
}

/// firstNonGenericFormat.
pub(super) fn first_non_generic(block: &Res<'_>, paths: &[&str]) -> Vec<u8> {
    paths
        .iter()
        .map(|p| trimmed(block, p))
        .find(|v| !v.is_empty() && !is_generic_mime(v))
        .unwrap_or_default()
}

pub(super) fn ext_lower(filename: &[u8]) -> Vec<u8> {
    let ext = file_ext(filename);
    go_lower(ext.strip_prefix(b".").unwrap_or(ext))
}

/// parseOpenAIResponsesDataURL: MIME type and payload of a valid base64 data URL.
fn parse_data_url(raw: &[u8]) -> (Vec<u8>, Vec<u8>) {
    use cpa_common::gostr::GoStr;
    let raw = go_trim_space(raw);
    if raw.len() < 5 || !raw[..5].eq_ignore_ascii_case(b"data:") {
        return (vec![], vec![]);
    }
    let rest = &raw[5..];
    let Some(comma) = rest.iter().position(|&c| c == b',') else {
        return (vec![], vec![]);
    };
    let (metadata, payload) = (&rest[..comma], go_trim_space(&rest[comma + 1..]));
    if payload.is_empty() {
        return (vec![], vec![]);
    }
    let mut fields = metadata.split(|&c| c == b';');
    let mime = go_trim_space(fields.next().unwrap_or_default()).to_vec();
    if !fields.any(|f| String::from_utf8_lossy(go_trim_space(f)).go_eq_fold("base64")) {
        return (vec![], vec![]);
    }
    if go_base64_decode(payload, true).is_none() && go_base64_decode(payload, false).is_none() {
        return (vec![], vec![]);
    }
    (mime, payload.to_vec())
}

fn mapped_mime(lower: &[u8], prefix: &str) -> Option<&'static str> {
    crate::mime::mime_type(lower).filter(|m| !m.is_empty() && m.starts_with(prefix))
}

/// openAIResponsesAudioMimeType.
pub(super) fn audio_mime(format: &[u8]) -> Vec<u8> {
    let format = go_trim_space(format);
    if is_generic_mime(format) {
        return b"audio/wav".to_vec();
    }
    if format.contains(&b'/') {
        return format.to_vec();
    }
    let lower = go_lower(format);
    let mime: &str = match lower.as_slice() {
        b"wav" => "audio/wav",
        b"mp3" | b"mpeg" => "audio/mpeg",
        b"ogg" => "audio/ogg",
        b"flac" => "audio/flac",
        b"aac" => "audio/aac",
        b"webm" => "audio/webm",
        b"pcm16" | b"pcm" => "audio/pcm",
        b"g711_ulaw" | b"g711_alaw" => "audio/basic",
        b"opus" => "audio/opus",
        b"m4a" => "audio/mp4",
        b"wma" => "audio/x-ms-wma",
        _ => mapped_mime(&lower, "audio/").unwrap_or("audio/wav"),
    };
    mime.as_bytes().to_vec()
}

/// openAIResponsesVideoMimeType.
pub(super) fn video_mime(format: &[u8]) -> Vec<u8> {
    let format = go_trim_space(format);
    if is_generic_mime(format) {
        return b"video/mp4".to_vec();
    }
    if format.contains(&b'/') {
        return format.to_vec();
    }
    let lower = go_lower(format);
    let mime: &str = match lower.as_slice() {
        b"mp4" => "video/mp4",
        b"webm" => "video/webm",
        b"mov" | b"quicktime" => "video/quicktime",
        b"avi" | b"x-msvideo" => "video/x-msvideo",
        b"mpeg" => "video/mpeg",
        b"ogg" => "video/ogg",
        b"mkv" | b"x-matroska" => "video/x-matroska",
        b"flv" | b"x-flv" => "video/x-flv",
        b"3gpp" => "video/3gpp",
        _ => mapped_mime(&lower, "video/").unwrap_or("video/mp4"),
    };
    mime.as_bytes().to_vec()
}

/// The data-URL branch shared by audio and video: the URL's MIME type, else the format,
/// the filename's extension, then `default`.
fn media_data_url(
    url: &[u8],
    format: &[u8],
    filename: &[u8],
    mime_of: fn(&[u8]) -> Vec<u8>,
    default: &[u8],
) -> Option<(Vec<u8>, Vec<u8>)> {
    let (mut mime, data) = parse_data_url(url);
    if data.is_empty() {
        return None;
    }
    if is_generic_mime(&mime) {
        mime = if !format.is_empty() && !is_generic_mime(format) {
            mime_of(format)
        } else if !filename.is_empty() {
            mime_of(&ext_lower_raw(filename))
        } else {
            default.to_vec()
        };
    }
    Some((mime, data))
}

/// `strings.TrimPrefix(filepath.Ext(filename), ".")` without lower-casing.
fn ext_lower_raw(filename: &[u8]) -> Vec<u8> {
    let ext = file_ext(filename);
    ext.strip_prefix(b".").unwrap_or(ext).to_vec()
}

fn media_mime(format: &[u8], filename: &[u8], mime_of: fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    if is_generic_mime(format) && !filename.is_empty() {
        mime_of(&ext_lower_raw(filename))
    } else {
        mime_of(format)
    }
}

type Media = Option<(Vec<u8>, Vec<u8>)>;

/// openAIResponsesAudioFromBlock.
fn audio_from_block(block: &Res<'_>) -> Media {
    let kind = go_lower(go_trim_space(&field(block, "type")));
    if kind != b"input_audio" && kind != b"audio" {
        return None;
    }
    let filename = first_non_empty(block, &["filename", "file.filename"]);
    let mut audio = block.get("input_audio");
    if !audio.exists() {
        audio = block.get("audio");
    }
    let mut format = [
        audio.get("format"),
        audio.get("mime_type"),
        block.get("format"),
        block.get("mime_type"),
    ]
    .iter()
    .map(|v| go_trim_space(&v.bytes()).to_vec())
    .find(|v| !v.is_empty() && !is_generic_mime(v))
    .unwrap_or_default();
    let mut data = audio.get("data").bytes().into_owned();
    if data.is_empty() {
        data = field(block, "data").into_owned();
    }
    if data.is_empty() {
        let url = first_non_empty(block, &["audio_url.url", "audio_url", "url"]);
        if !url.is_empty() {
            if is_data_url(&url) {
                return media_data_url(&url, &format, &filename, audio_mime, b"audio/wav");
            } else if !is_remote_url(&url) {
                return Some((media_mime(&format, &filename, audio_mime), url));
            }
        }
    }
    if data.is_empty() && field(block, "source.type").as_ref() == b"base64" {
        data = field(block, "source.data").into_owned();
        if format.is_empty() {
            format = field(block, "source.media_type").into_owned();
        }
    }
    if data.is_empty() {
        return None;
    }
    if is_data_url(&data) {
        return media_data_url(&data, &format, &filename, audio_mime, b"audio/wav");
    }
    Some((media_mime(&format, &filename, audio_mime), data))
}

/// openAIResponsesVideoFromBlock.
fn video_from_block(block: &Res<'_>) -> Media {
    let kind = go_lower(go_trim_space(&field(block, "type")));
    if kind != b"input_video" && kind != b"video_url" && kind != b"video" {
        return None;
    }
    let filename = first_non_empty(block, &["filename", "file.filename"]);
    let mut video = block.get("input_video");
    if !video.exists() {
        video = block.get("video");
    }
    let mut format = [
        video.get("format"),
        video.get("mime_type"),
        block.get("format"),
        block.get("mime_type"),
    ]
    .iter()
    .map(|v| go_trim_space(&v.bytes()).to_vec())
    .find(|v| !v.is_empty() && !is_generic_mime(v))
    .unwrap_or_default();
    let url = first_non_empty(block, &["video_url.url", "video_url", "url"]);
    if !url.is_empty() {
        if is_data_url(&url) {
            return media_data_url(&url, &format, &filename, video_mime, b"video/mp4");
        } else if !is_remote_url(&url) {
            return Some((media_mime(&format, &filename, video_mime), url));
        }
    }
    let mut data = video.get("data").bytes().into_owned();
    if data.is_empty() {
        data = field(block, "data").into_owned();
    }
    if data.is_empty() && field(block, "source.type").as_ref() == b"base64" {
        data = field(block, "source.data").into_owned();
        if format.is_empty() {
            format = field(block, "source.media_type").into_owned();
        }
    }
    if data.is_empty() {
        return None;
    }
    if is_data_url(&data) {
        return media_data_url(&data, &format, &filename, video_mime, b"video/mp4");
    }
    Some((media_mime(&format, &filename, video_mime), data))
}

/// normalizeFormatToMIME.
pub(super) fn format_to_mime(format: &[u8]) -> Vec<u8> {
    let format = go_trim_space(format);
    if format.is_empty() || is_generic_mime(format) {
        return vec![];
    }
    if format.contains(&b'/') {
        return format.to_vec();
    }
    let lower = go_lower(format);
    let mime = match lower.as_slice() {
        b"jpg" | b"jpeg" => "image/jpeg",
        b"wav" => "audio/wav",
        b"mp3" => "audio/mpeg",
        b"mp4" => "video/mp4",
        b"webm" => "video/webm",
        b"pdf" => "application/pdf",
        _ => crate::mime::mime_type(&lower).unwrap_or_default(),
    };
    mime.as_bytes().to_vec()
}

/// openAIResponsesFileFromBlock.
fn file_from_block(block: &Res<'_>) -> Media {
    let kind = go_lower(go_trim_space(&field(block, "type")));
    if kind != b"input_file" && kind != b"file" {
        return None;
    }
    let filename = first_non_empty(block, &["filename", "file.filename"]);
    let mut data = first_non_empty(block, &["file_data", "file.file_data", "data"]);
    if data.is_empty() {
        let url = first_non_empty(block, &["file_url.url", "file_url", "file.file_url", "url"]);
        if is_data_url(&url) {
            data = url;
        }
    }
    let mut fallback = first_non_generic(block, &["mime_type", "file.mime_type", "format", "file.format"]);
    if !fallback.is_empty() {
        fallback = format_to_mime(&fallback);
    }
    if is_generic_mime(&fallback) && !filename.is_empty() {
        let ext = ext_lower(&filename);
        if !ext.is_empty() {
            fallback = format_to_mime(&ext);
        }
    }
    if is_data_url(&data) {
        let (mut mime, payload) = parse_data_url(&data);
        if payload.is_empty() {
            return None;
        }
        if is_generic_mime(&mime) && !fallback.is_empty() {
            mime = fallback;
        }
        if is_generic_mime(&mime) && !filename.is_empty() {
            let ext = ext_lower(&filename);
            if !ext.is_empty() {
                let norm = format_to_mime(&ext);
                if !norm.is_empty() {
                    mime = norm;
                }
            }
        }
        if is_generic_mime(&mime) {
            mime = b"application/octet-stream".to_vec();
        }
        return Some((mime, payload));
    }
    normalize_openai_file_data(&filename, &fallback, &data)
}

/// openAIResponsesImageMimeType.
pub(super) fn image_mime(format: &[u8], filename: &[u8]) -> Vec<u8> {
    let format = go_trim_space(format);
    if !format.is_empty() && !is_generic_mime(format) {
        if format.contains(&b'/') {
            return format.to_vec();
        }
        let lower = go_lower(format);
        if lower == b"jpg" || lower == b"jpeg" {
            return b"image/jpeg".to_vec();
        }
        if let Some(mapped) = crate::mime::mime_type(&lower).filter(|m| !m.is_empty()) {
            return mapped.as_bytes().to_vec();
        }
        return [b"image/", format].concat();
    }
    if !filename.is_empty() {
        let ext = ext_lower(filename);
        if ext == b"jpg" || ext == b"jpeg" {
            return b"image/jpeg".to_vec();
        }
        if !ext.is_empty()
            && let Some(mapped) = crate::mime::mime_type(&ext).filter(|m| !m.is_empty())
        {
            return mapped.as_bytes().to_vec();
        }
    }
    b"image/png".to_vec()
}

const IMAGE_FORMAT_PATHS: [&str; 6] = [
    "format",
    "mime_type",
    "input_image.format",
    "input_image.mime_type",
    "image.format",
    "image.mime_type",
];

/// openAIResponsesImageFromBlock.
fn image_from_block(block: &Res<'_>) -> Media {
    let kind = go_lower(go_trim_space(&field(block, "type")));
    if !matches!(kind.as_slice(), b"input_image" | b"image_url" | b"image") {
        return None;
    }
    let mut format = first_non_generic(block, &IMAGE_FORMAT_PATHS);
    let filename = first_non_empty(block, &["filename", "file.filename"]);
    let url = first_non_empty(block, &["image_url.url", "image_url", "url"]);
    if !url.is_empty() {
        if is_data_url(&url) {
            let (mut mime, data) = parse_data_url(&url);
            if data.is_empty() {
                return None;
            }
            if is_generic_mime(&mime) {
                mime = image_mime(&format, &filename);
            }
            return Some((mime, data));
        } else if !is_remote_url(&url) {
            return Some((image_mime(&format, &filename), url));
        }
    }
    let mut data = vec![];
    if field(block, "source.type").as_ref() == b"base64" {
        data = field(block, "source.data").into_owned();
        if format.is_empty() {
            format = field(block, "source.media_type").into_owned();
        }
    }
    if data.is_empty() && block.get("data").exists() {
        data = field(block, "data").into_owned();
    }
    if data.is_empty() {
        return None;
    }
    if is_data_url(&data) {
        let (mut mime, payload) = parse_data_url(&data);
        if payload.is_empty() {
            return None;
        }
        if is_generic_mime(&mime) {
            mime = image_mime(&format, &filename);
        }
        return Some((mime, payload));
    }
    Some((image_mime(&format, &filename), data))
}

/// openAIResponsesMediaFromBlock.
pub(super) fn media_from_block(block: &Res<'_>) -> Media {
    image_from_block(block)
        .or_else(|| audio_from_block(block))
        .or_else(|| video_from_block(block))
        .or_else(|| file_from_block(block))
}

/// `filepath.Base(url.Parse(raw).Path)`, or empty when Go's url.Parse fails.
// ponytail: url.Parse is reduced to what decides this path: control bytes, invalid
// percent escapes in the authority, path or fragment, and a non-numeric port fail;
// Go's full host-character validation is not ported.
pub(super) fn url_path_base(raw: &[u8]) -> Vec<u8> {
    if raw.iter().any(|&c| c < 0x20 || c == 0x7f) {
        return vec![];
    }
    fn unescape(s: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(s.len());
        let mut i = 0;
        while i < s.len() {
            if s[i] == b'%' {
                let hex = |c: u8| (c as char).to_digit(16);
                let (h, l) = (hex(*s.get(i + 1)?)?, hex(*s.get(i + 2)?)?);
                out.push((h * 16 + l) as u8);
                i += 3;
            } else {
                out.push(s[i]);
                i += 1;
            }
        }
        Some(out)
    }
    let (rest, fragment) = match raw.iter().position(|&c| c == b'#') {
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (raw, &b""[..]),
    };
    if unescape(fragment).is_none() {
        return vec![];
    }
    let rest = match rest.iter().position(|&c| c == b'?') {
        Some(i) => &rest[..i],
        None => rest,
    };
    let Some(colon) = rest.iter().position(|&c| c == b':') else {
        return vec![];
    };
    let mut rest = &rest[colon + 1..];
    if let Some(after) = rest.strip_prefix(b"//") {
        let end = after.iter().position(|&c| c == b'/').unwrap_or(after.len());
        let authority = &after[..end];
        let host = match authority.iter().rposition(|&c| c == b'@') {
            Some(at) => &authority[at + 1..],
            None => authority,
        };
        if unescape(host).is_none() {
            return vec![];
        }
        if !host.starts_with(b"[")
            && let Some(port) = host.iter().rposition(|&c| c == b':').map(|i| &host[i + 1..])
            && !port.iter().all(u8::is_ascii_digit)
        {
            return vec![];
        }
        rest = &after[end..];
    }
    let Some(path) = unescape(rest) else {
        return vec![];
    };
    // filepath.Base
    let mut path = &path[..];
    if path.is_empty() {
        return b".".to_vec();
    }
    while let Some(stripped) = path.strip_suffix(b"/") {
        path = stripped;
    }
    if let Some(slash) = path.iter().rposition(|&c| c == b'/') {
        path = &path[slash + 1..];
    }
    if path.is_empty() { b"/".to_vec() } else { path.to_vec() }
}
