use super::*;
use crate::request_logging::history::row::body_columns;

pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        Self(std::env::temp_dir().join(format!("cpa-history-{}", uuid::Uuid::new_v4())))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn at(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339).unwrap().with_timezone(&Utc)
}

fn full_row() -> Row {
    Row {
        exchange_id: "ex-1".into(),
        turn: 2,
        send: 1,
        started_at_us: 1_762_000_000_123_456,
        ended_at_us: Some(1_762_000_001_000_000),
        source: Source::Proxy,
        endpoint: Some("POST /v1/messages".into()),
        transport: Some("websocket".into()),
        provider: Some("claude".into()),
        model_requested: Some("sonnet".into()),
        model: Some("claude-sonnet-4-6".into()),
        response_model: Some("claude-sonnet-4-6".into()),
        account: Some("user@example.invalid".into()),
        auth_index: Some("abc123".into()),
        session_id: Some("00000000-0000-4000-8000-000000000001".into()),
        user_agent: Some("synthetic/1.0".into()),
        stream: true,
        status: Some(200),
        outcome: Outcome::Complete,
        error: None,
        input_tokens: Some(10),
        output_tokens: Some(20),
        reasoning_tokens: Some(0),
        cache_read_tokens: Some(30),
        cache_write_tokens: Some(40),
        cache_write_5m_tokens: Some(5),
        cache_write_1h_tokens: Some(35),
        total_tokens: Some(100),
        latency_ms: Some(900),
        ttft_ms: Some(120),
        upstream_request_id: Some("req_1".into()),
        request_body: None,
        request_body_raw: Some(vec![0xff, 0xfe]),
        response_body: Some("data: {\"a\":1}\ndata: {\"b\":2}".into()),
        response_body_raw: None,
        request_frames: Some(vec!["a\nb".into(), "c".into()]),
        response_frames: Some(Vec::new()),
        request_bytes: 2,
        response_bytes: 26,
        request_truncated: false,
        response_truncated: true,
    }
}

fn sparse_row() -> Row {
    Row {
        exchange_id: "ex-2".into(),
        started_at_us: 1_762_000_002_000_000,
        source: Source::Native,
        outcome: Outcome::TransportLost,
        ..Row::default()
    }
}

fn files(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for partition in std::fs::read_dir(dir).unwrap() {
        for file in std::fs::read_dir(partition.unwrap().path()).unwrap() {
            let path = file.unwrap().path();
            if path.to_string_lossy().ends_with(suffix) {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn rows_round_trip_through_a_file() {
    let dir = TempDir::new();
    let rows = vec![full_row(), sparse_row()];
    let mut file = ParquetFile::create(&dir.0, at("2026-11-09T08:15:30Z"), 7).unwrap();
    file.write_group(&rows).unwrap();
    let path = file.finish().unwrap();
    assert_eq!(read_rows(&path).unwrap(), rows);
}

#[test]
fn rows_written_in_two_groups_read_back_in_order() {
    let dir = TempDir::new();
    let mut file = ParquetFile::create(&dir.0, at("2026-11-09T08:15:30Z"), 0).unwrap();
    file.write_group(&[full_row()]).unwrap();
    file.write_group(&[sparse_row()]).unwrap();
    let path = file.finish().unwrap();
    assert_eq!(read_rows(&path).unwrap(), vec![full_row(), sparse_row()]);
}

#[test]
fn a_file_is_invisible_until_finished() {
    let dir = TempDir::new();
    let mut file = ParquetFile::create(&dir.0, at("2026-11-09T23:59:59Z"), 3).unwrap();
    file.write_group(&[sparse_row()]).unwrap();
    assert!(file.bytes_written() > 0);
    assert_eq!(files(&dir.0, ".partial").len(), 1);
    assert!(files(&dir.0, ".parquet").is_empty());
    let path = file.finish().unwrap();
    assert!(files(&dir.0, ".partial").is_empty());
    assert_eq!(files(&dir.0, ".parquet"), vec![path.clone()]);
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(name, format!("235959-{}-0003.parquet", std::process::id()));
    assert_eq!(path.parent().unwrap().file_name().unwrap(), "date=2026-11-09");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&dir.0), 0o700);
    }
}

#[test]
fn a_finished_name_already_taken_is_never_overwritten() {
    let dir = TempDir::new();
    let first = ParquetFile::create(&dir.0, at("2026-11-09T08:00:00Z"), 0).unwrap();
    let first = first.finish().unwrap();
    let before = std::fs::read(&first).unwrap();
    let mut second = ParquetFile::create(&dir.0, at("2026-11-09T08:00:00Z"), 0).unwrap();
    second.write_group(&[sparse_row()]).unwrap();
    let second = second.finish().unwrap();
    assert_ne!(first, second);
    assert_eq!(std::fs::read(&first).unwrap(), before);
    assert_eq!(read_rows(&second).unwrap(), vec![sparse_row()]);
}

#[test]
fn body_columns_have_no_statistics_or_dictionary() {
    use parquet::file::reader::FileReader;
    let dir = TempDir::new();
    let mut file = ParquetFile::create(&dir.0, at("2026-11-09T08:15:30Z"), 0).unwrap();
    file.write_group(&[full_row(), full_row()]).unwrap();
    let path = file.finish().unwrap();
    let reader =
        parquet::file::serialized_reader::SerializedFileReader::new(std::fs::File::open(path).unwrap()).unwrap();
    let group = reader.metadata().row_group(0);
    let column = |name: &str| {
        group
            .columns()
            .iter()
            .find(|c| c.column_path().string() == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    };
    for name in [
        "request_body",
        "response_body",
        "request_body_raw",
        "response_body_raw",
        "request_frames.list.element",
        "response_frames.list.element",
    ] {
        assert!(column(name).statistics().is_none(), "{name} has statistics");
        assert!(
            column(name).dictionary_page_offset().is_none(),
            "{name} is dictionary encoded"
        );
    }
    assert!(column("model").dictionary_page_offset().is_some());
    assert!(column("started_at").statistics().is_some());
}

#[test]
fn body_columns_keep_utf8_as_text_and_the_rest_as_bytes() {
    assert_eq!(
        body_columns(b"{\"a\":1}".to_vec(), false),
        (Some("{\"a\":1}".into()), None)
    );
    assert_eq!(body_columns(vec![0x7b, 0xff], false), (None, Some(vec![0x7b, 0xff])));
    assert_eq!(body_columns(Vec::new(), false), (Some(String::new()), None));
    // A cut character is dropped only from a truncated body.
    assert_eq!(body_columns(vec![b'a', 0xc3], true), (Some("a".into()), None));
    assert_eq!(body_columns(vec![b'a', 0xc3], false), (None, Some(vec![b'a', 0xc3])));
}

#[test]
fn a_failed_finish_leaves_no_partial() {
    let dir = TempDir::new();
    let target = write_dir_target(&dir.0);
    let mut file = ParquetFile::replacing(&target).unwrap();
    file.write_group(&[sparse_row()]).unwrap();
    // The target is a directory, so the rename fails after the footer is written.
    assert!(file.finish().is_err());
    assert!(files(&dir.0, ".partial").is_empty());
}

fn write_dir_target(dir: &Path) -> PathBuf {
    let target = dir.join("date=2026-11-09").join("080000-1-0000.parquet");
    std::fs::create_dir_all(target.join("occupied")).unwrap();
    target
}
