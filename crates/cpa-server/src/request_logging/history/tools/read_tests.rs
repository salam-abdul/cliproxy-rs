use serde_json::json;

use super::*;
use crate::request_logging::history::test_support::TempDir;
use crate::request_logging::history::tools::scan::{Call, Conv, Def, Outcome, SendFacts};
use crate::request_logging::history::tools::store::{Fingerprint, Index};

const NOW: i64 = 1_764_000_000_000;
const DAY: i64 = 86_400_000;

/// A tool index built file by file with [`Index::commit`].
struct Store {
    dir: TempDir,
    index: Index,
    files: usize,
}

impl Store {
    fn new() -> Self {
        let dir = TempDir::new();
        std::fs::create_dir_all(&dir.0).unwrap();
        let index = Index::open(&dir.0).unwrap();
        Self { dir, index, files: 0 }
    }

    /// Commits `sends` as the facts of a new stand-in history file.
    fn file(&mut self, sends: &[SendFacts]) -> &mut Self {
        self.commit(sends, false)
    }

    fn commit(&mut self, sends: &[SendFacts], unreadable: bool) -> &mut Self {
        self.files += 1;
        let partition = self.dir.0.join("date=2025-11-24");
        std::fs::create_dir_all(&partition).unwrap();
        let name = format!("{:06}-1-0000.parquet", self.files);
        let path = partition.join(&name);
        std::fs::write(&path, b"synthetic").unwrap();
        let read_as = Fingerprint::from_path(&path).unwrap();
        let rel = format!("date=2025-11-24/{name}");
        assert!(self.index.commit(&rel, &read_as, &path, sends, unreadable).unwrap());
        self
    }

    fn read(&self, range: Range, provider: Option<&str>) -> Value {
        read(&self.dir.0, range, provider, NOW).unwrap()
    }
}

fn send(at_ms: i64, conv: Conv) -> SendFacts {
    SendFacts {
        at_ms,
        provider: Some("claude".into()),
        conv,
        message: None,
        defs: Vec::new(),
        calls: Vec::new(),
        results: Vec::new(),
        skipped_format: false,
        cut: false,
    }
}

trait Build {
    fn message(self, id: i64) -> Self;
    fn def(self, tool: &str, bytes: i64) -> Self;
    fn call(self, id: i64, tool: &str) -> Self;
    fn skill(self, id: i64, skill: &str) -> Self;
    fn result(self, id: i64, is_error: bool, bytes: i64) -> Self;
    fn loaded(self, id: i64, bytes: i64, loaded: i64) -> Self;
}

impl Build for SendFacts {
    fn message(mut self, id: i64) -> Self {
        self.message = Some(id);
        self
    }
    fn def(mut self, tool: &str, bytes: i64) -> Self {
        self.defs.push(Def {
            tool: tool.into(),
            bytes,
        });
        self
    }
    fn call(mut self, id: i64, tool: &str) -> Self {
        self.calls.push(Call {
            id,
            tool: tool.into(),
            skill: None,
        });
        self
    }
    fn skill(mut self, id: i64, skill: &str) -> Self {
        self.calls.push(Call {
            id,
            tool: "Skill".into(),
            skill: Some(skill.into()),
        });
        self
    }
    fn result(mut self, id: i64, is_error: bool, bytes: i64) -> Self {
        self.results.push(Outcome {
            id,
            is_error,
            bytes,
            loaded: 0,
        });
        self
    }
    fn loaded(mut self, id: i64, bytes: i64, loaded: i64) -> Self {
        self.results.push(Outcome {
            id,
            is_error: false,
            bytes,
            loaded,
        });
        self
    }
}

fn skipped(at_ms: i64, provider: &str) -> SendFacts {
    let mut facts = send(at_ms, Conv::None);
    facts.provider = Some(provider.into());
    facts.skipped_format = true;
    facts
}

fn tool<'a>(answer: &'a Value, name: &str) -> &'a Value {
    answer["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no tool {name} in {answer}"))
}

fn names(answer: &Value, list: &str) -> Vec<String> {
    answer[list]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn every_measure_on_a_small_fixture() {
    let t = NOW - 3_600_000;
    let mut cut = send(t + 5, Conv::Start(4));
    cut.cut = true;
    let mut store = Store::new();
    store
        .file(&[
            send(t, Conv::Start(1))
                .message(100)
                .def("Read", 100)
                .def("mcp__docs__search", 300)
                .call(1, "Read"),
            send(t + 1, Conv::Continue(100))
                .message(200)
                .def("Read", 120)
                .def("mcp__docs__search", 300)
                .call(2, "mcp__docs__search")
                .result(1, false, 10),
            send(t + 2, Conv::Start(2))
                .def("Read", 80)
                .call(3, "Read")
                .skill(4, "pdf")
                .result(2, true, 50),
            send(t + 3, Conv::Start(2)).loaded(4, 30, 400),
            send(t + 4, Conv::Start(3)),
            cut,
            skipped(t + 6, "codex"),
        ])
        .commit(&[], true);
    let answer = store.read(Range::Day, None);
    assert_eq!(answer["range"], "24h");
    assert_eq!(answer["covered"], json!({"fromMs": t, "toMs": t + 6}));
    assert_eq!(answer["sends"], 7);
    assert_eq!(answer["skippedFormat"], 1);
    assert_eq!(answer["skippedCut"], 1);
    assert_eq!(answer["unreadableFiles"], 1);
    // Conversations 1 and 2 defined tools; 3 and 4 did not.
    assert_eq!(answer["conversations"], 2);
    assert_eq!(names(&answer, "tools"), ["Read", "mcp__docs__search", "Skill"]);
    assert_eq!(
        *tool(&answer, "Read"),
        json!({
            "name": "Read", "server": null, "carried": 2, "carriedShare": 1.0, "used": 2,
            "weight": 100.0, "calls": 2, "callsPerConversation": 1.0, "results": 1, "errors": 0,
            "failureRate": 0.0, "resultMean": 10.0, "resultP95": 10, "resultTotal": 10, "labels": []
        })
    );
    assert_eq!(
        *tool(&answer, "mcp__docs__search"),
        json!({
            "name": "mcp__docs__search", "server": "docs", "carried": 1, "carriedShare": 0.5, "used": 1,
            "weight": 300.0, "calls": 1, "callsPerConversation": 1.0, "results": 1, "errors": 1,
            "failureRate": 1.0, "resultMean": 50.0, "resultP95": 50, "resultTotal": 50, "labels": ["heavy"]
        })
    );
    assert_eq!(
        *tool(&answer, "Skill"),
        json!({
            "name": "Skill", "server": null, "carried": 0, "carriedShare": 0.0, "used": 0,
            "weight": null, "calls": 1, "callsPerConversation": null, "results": 1, "errors": 0,
            "failureRate": 0.0, "resultMean": 30.0, "resultP95": 30, "resultTotal": 30, "labels": []
        })
    );
    assert_eq!(
        answer["servers"],
        json!([{"name": "docs", "tools": 1, "carried": 1, "calls": 1, "errors": 1}])
    );
    assert_eq!(
        answer["skills"],
        json!([{"name": "pdf", "calls": 1, "conversations": 1, "errors": 0, "loadedMean": 400.0, "loadedP95": 400}])
    );
}

#[test]
fn a_conversation_started_before_the_range_counts_for_its_calls_inside_it() {
    let before = NOW - 2 * DAY;
    let mut store = Store::new();
    store
        .file(&[
            send(before, Conv::Start(1)).message(10).def("Read", 40).call(1, "Read"),
            send(before, Conv::Start(2)).message(20).def("Read", 40).call(2, "Read"),
        ])
        .file(&[send(NOW - 1000, Conv::Continue(10)).def("Read", 40).call(3, "Read")]);
    let answer = store.read(Range::Day, None);
    assert_eq!(answer["sends"], 1);
    // Conversation 2 had no send in range.
    assert_eq!(answer["conversations"], 1);
    let read = tool(&answer, "Read");
    assert_eq!(read["carried"], 1);
    assert_eq!(read["used"], 1);
    assert_eq!(read["calls"], 1);
    assert!(read["used"].as_i64() <= read["carried"].as_i64());
}

#[test]
fn a_file_with_only_skipped_sends_counts_them_and_nothing_else() {
    let mut store = Store::new();
    store.file(&[skipped(NOW - 2000, "codex"), skipped(NOW - 1000, "gemini")]);
    let answer = store.read(Range::Week, None);
    assert_eq!(answer["range"], "7d");
    assert_eq!(answer["sends"], 2);
    assert_eq!(answer["skippedFormat"], 2);
    assert_eq!(answer["skippedCut"], 0);
    assert_eq!(answer["conversations"], 0);
    assert_eq!(answer["covered"], json!({"fromMs": NOW - 2000, "toMs": NOW - 1000}));
    assert_eq!(answer["tools"], json!([]));
    assert_eq!(answer["servers"], json!([]));
    assert_eq!(answer["skills"], json!([]));
}

#[test]
fn the_range_boundary_can_fall_inside_one_file() {
    let from = NOW - DAY;
    let mut store = Store::new();
    store.file(&[
        send(from - 1, Conv::Start(1)).def("Read", 10).call(1, "Read"),
        send(from, Conv::Start(2)).def("Read", 10).call(2, "Read"),
        send(NOW, Conv::Start(3)).def("Read", 10).call(3, "Read"),
        send(NOW + 1, Conv::Start(4)).def("Read", 10).call(4, "Read"),
    ]);
    let answer = store.read(Range::Day, None);
    assert_eq!(answer["sends"], 2);
    assert_eq!(answer["covered"], json!({"fromMs": from, "toMs": NOW}));
    assert_eq!(answer["conversations"], 2);
    assert_eq!(tool(&answer, "Read")["carried"], 2);
    assert_eq!(tool(&answer, "Read")["calls"], 2);
}

#[test]
fn a_call_repeated_in_two_files_counts_once_from_its_first_copy() {
    let t = NOW - 3_600_000;
    let mut store = Store::new();
    store
        .file(&[
            send(t, Conv::Start(1)).def("Read", 10).call(1, "Read"),
            send(t + 1, Conv::Start(1)).def("Read", 10).result(1, true, 70),
            // First seen before the range: its later copy in range does not count.
            send(NOW - 2 * DAY, Conv::Start(9)).def("Grep", 10).call(9, "Grep"),
        ])
        .file(&[
            send(t + 2, Conv::Start(1))
                .def("Read", 10)
                .call(1, "Read")
                .result(1, false, 5),
            send(t + 3, Conv::Start(1)).def("Grep", 10).call(9, "Grep"),
        ]);
    let answer = store.read(Range::Day, None);
    let read = tool(&answer, "Read");
    assert_eq!(read["calls"], 1);
    assert_eq!(read["results"], 1);
    // The first result copy is the one counted.
    assert_eq!(read["errors"], 1);
    assert_eq!(read["resultTotal"], 70);
    let grep = tool(&answer, "Grep");
    assert_eq!(grep["calls"], 0);
    assert_eq!(grep["carried"], 1);
}

/// Conversations 1..=`n`, each defining and calling `Base`; conversation 1 also
/// defines `Idle` and never calls it.
fn dead_weight(n: i64) -> Value {
    let mut store = Store::new();
    let sends: Vec<SendFacts> = (1..=n)
        .map(|conv| {
            let facts = send(NOW - 1000, Conv::Start(conv)).def("Base", 10).call(conv, "Base");
            if conv == 1 { facts.def("Idle", 10) } else { facts }
        })
        .collect();
    store.file(&sends);
    store.read(Range::Day, None)
}

fn labels(answer: &Value, name: &str) -> Vec<String> {
    tool(answer, name)["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn dead_weight_needs_a_fifth_of_conversations_and_no_use() {
    let at = dead_weight(5);
    assert!(labels(&at, "Idle").contains(&"dead".to_owned()));
    assert!(!labels(&at, "Base").contains(&"dead".to_owned()));
    let below = dead_weight(6);
    assert!(!labels(&below, "Idle").contains(&"dead".to_owned()));
}

#[test]
fn failing_needs_ten_calls_and_a_fifth_of_results_in_error() {
    let mut store = Store::new();
    let mut sends = Vec::new();
    let mut id = 0;
    // (tool, calls, errors): every call has a result.
    for (name, calls, errors) in [("AtRate", 10, 2), ("BelowRate", 11, 2), ("FewCalls", 9, 9)] {
        for n in 0..calls {
            id += 1;
            sends.push(
                send(NOW - 1000, Conv::Start(1))
                    .def(name, 10)
                    .call(id, name)
                    .result(id, n < errors, 1),
            );
        }
    }
    store.file(&sends);
    let answer = store.read(Range::Day, None);
    assert!(labels(&answer, "AtRate").contains(&"failing".to_owned()));
    assert!(!labels(&answer, "BelowRate").contains(&"failing".to_owned()));
    assert!(!labels(&answer, "FewCalls").contains(&"failing".to_owned()));
}

#[test]
fn bloating_needs_a_p95_result_of_100_kib() {
    let mut store = Store::new();
    store.file(&[send(NOW - 1000, Conv::Start(1))
        .def("At", 10)
        .def("Below", 10)
        .call(1, "At")
        .call(2, "Below")
        .result(1, false, 102_400)
        .result(2, false, 102_399)]);
    let answer = store.read(Range::Day, None);
    // At also wins the heavy tie by name.
    assert_eq!(labels(&answer, "At"), ["heavy", "bloating"]);
    assert_eq!(labels(&answer, "Below"), Vec::<String>::new());
}

#[test]
fn heavy_is_the_top_tenth_by_weight_times_carried_with_ties_by_name() {
    let mut first = send(NOW - 1000, Conv::Start(1));
    for n in 0..8 {
        first = first.def(&format!("Small{n}"), 10);
    }
    let first = first.def("Tc", 100).def("Tb", 100).def("Ta", 100).def("Wide", 60);
    let second = send(NOW - 1000, Conv::Start(2)).def("Wide", 60);
    let mut store = Store::new();
    store.file(&[first, second]);
    let answer = store.read(Range::Day, None);
    // Twelve carried tools: the top two by score are heavy. Wide scores 120, the
    // T tools 100 each, and Ta wins their tie by name.
    assert_eq!(labels(&answer, "Wide"), ["dead", "heavy"]);
    assert_eq!(labels(&answer, "Ta"), ["dead", "heavy"]);
    assert_eq!(labels(&answer, "Tb"), ["dead"]);
    assert_eq!(labels(&answer, "Tc"), ["dead"]);
    assert_eq!(labels(&answer, "Small0"), ["dead"]);
}

#[test]
fn p95_is_the_nearest_rank() {
    let mut facts = send(NOW - 1000, Conv::Start(1)).def("Read", 10);
    for id in 1..=20 {
        facts = facts.call(id, "Read").result(id, false, id);
    }
    let mut store = Store::new();
    store.file(&[facts]);
    let answer = store.read(Range::Day, None);
    let read = tool(&answer, "Read");
    // Rank ceil(0.95 * 20) = 19; interpolation would give 19.05, the maximum 20.
    assert_eq!(read["resultP95"], 19);
    assert_eq!(read["resultMean"], 10.5);
    assert_eq!(read["resultTotal"], 210);
}

#[test]
fn the_provider_filter_applies_to_sends_definitions_and_calls() {
    let mut store = Store::new();
    store.file(&[
        send(NOW - 1000, Conv::Start(1)).def("Read", 10).call(1, "Read"),
        skipped(NOW - 500, "codex"),
    ]);
    let codex = store.read(Range::Day, Some("codex"));
    assert_eq!(codex["sends"], 1);
    assert_eq!(codex["skippedFormat"], 1);
    assert_eq!(codex["conversations"], 0);
    assert_eq!(codex["tools"], json!([]));
    let claude = store.read(Range::Day, Some("claude"));
    assert_eq!(claude["sends"], 1);
    assert_eq!(claude["skippedFormat"], 0);
    assert_eq!(claude["conversations"], 1);
    assert_eq!(names(&claude, "tools"), ["Read"]);
    assert_eq!(tool(&claude, "Read")["calls"], 1);
}

#[test]
fn no_index_file_is_the_empty_answer_and_creates_nothing() {
    let dir = TempDir::new();
    std::fs::create_dir_all(&dir.0).unwrap();
    let answer = read(&dir.0, Range::Month, None, NOW).unwrap();
    assert_eq!(
        answer,
        json!({
            "range": "30d", "covered": null, "conversations": 0, "sends": 0, "skippedFormat": 0,
            "skippedCut": 0, "unreadableFiles": 0, "tools": [], "servers": [], "skills": []
        })
    );
    assert!(!dir.0.join(INDEX_FILE).exists());
}

#[test]
fn an_index_file_still_being_created_is_the_empty_answer() {
    let dir = TempDir::new();
    std::fs::create_dir_all(&dir.0).unwrap();
    // The indexer creates the file empty before it writes the schema.
    std::fs::write(dir.0.join(INDEX_FILE), b"").unwrap();
    let answer = read(&dir.0, Range::Day, None, NOW).unwrap();
    assert_eq!(answer["sends"], 0);
    assert_eq!(answer["covered"], Value::Null);
    assert_eq!(answer["tools"], json!([]));
}

#[test]
fn the_index_reads_after_the_indexer_closed_it() {
    let mut store = Store::new();
    store.file(&[send(NOW - 1000, Conv::Start(1)).def("Read", 10).call(1, "Read")]);
    let Store { dir, index, .. } = store;
    drop(index);
    let path = dir.0.join(INDEX_FILE);
    let side = |suffix: &str| dir.0.join(format!("{INDEX_FILE}{suffix}"));
    // Closing the last connection checkpoints and removes the side files; where it
    // does not, remove them after a full checkpoint, as SQLite itself would.
    if side("-wal").exists() || side("-shm").exists() {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
        drop(conn);
        let _ = std::fs::remove_file(side("-wal"));
        let _ = std::fs::remove_file(side("-shm"));
    }
    assert!(!side("-wal").exists() && !side("-shm").exists());
    let listing = || {
        let mut names: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before = listing();
    let answer = read(&dir.0, Range::Day, None, NOW).unwrap();
    assert_eq!(answer["sends"], 1);
    assert_eq!(tool(&answer, "Read")["calls"], 1);
    assert_eq!(listing(), before, "the read leaves no file behind");
}

#[test]
fn a_repeated_call_counts_under_the_conversation_a_later_copy_holds() {
    let t = NOW - 3_600_000;
    let mut store = Store::new();
    store
        .file(&[send(t, Conv::Continue(999)).call(1, "Read")])
        .file(&[send(t + 1, Conv::Start(1)).def("Read", 10).call(1, "Read")]);
    let answer = store.read(Range::Day, None);
    let read = tool(&answer, "Read");
    assert_eq!(read["calls"], 1);
    assert_eq!(read["used"], 1);
    assert_eq!(read["callsPerConversation"], 1.0);
    assert!(!labels(&answer, "Read").contains(&"dead".to_owned()));
}

#[test]
fn a_tool_called_only_outside_known_conversations_is_not_dead_weight() {
    let mut store = Store::new();
    store.file(&[
        send(NOW - 2000, Conv::Start(1)).def("Read", 10),
        send(NOW - 1000, Conv::Continue(999)).call(1, "Read"),
    ]);
    let answer = store.read(Range::Day, None);
    let read = tool(&answer, "Read");
    assert_eq!(read["calls"], 1);
    assert_eq!(read["used"], 0);
    assert!(!labels(&answer, "Read").contains(&"dead".to_owned()));
}

#[test]
fn server_and_versioned_tools_are_carried_and_used() {
    use crate::request_logging::history::row::Row;
    let request = r#"{"tools":[{"type":"bash_20250124","name":"bash"},{"type":"web_search_20250305","name":"web_search"}],"messages":[{"role":"user","content":"go"},{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"bash","input":{}},{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}},{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":{"type":"web_search_tool_result_error","error_code":"unavailable"}}]}]}"#;
    let row = Row {
        started_at_us: (NOW - 1000) * 1000,
        provider: Some("claude".into()),
        session_id: Some("s".into()),
        request_body: Some(request.into()),
        response_body: Some(r#"{"id":"msg_1","content":[]}"#.into()),
        ..Row::default()
    };
    let mut store = Store::new();
    store.file(&[crate::request_logging::history::tools::scan::scan(&row, &[])]);
    let answer = store.read(Range::Day, None);
    assert_eq!(names(&answer, "tools"), ["bash", "web_search"]);
    for name in ["bash", "web_search"] {
        assert_eq!(tool(&answer, name)["carried"], 1, "{name}");
        assert_eq!(tool(&answer, name)["used"], 1, "{name}");
        assert!(!labels(&answer, name).contains(&"dead".to_owned()), "{name}");
    }
    assert_eq!(tool(&answer, "web_search")["errors"], 1);
}
