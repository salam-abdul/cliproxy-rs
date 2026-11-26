use super::*;
use crate::request_logging::history::row::Source;
use crate::request_logging::history::tools::hash;

const SESSION: &str = "00000000-0000-4000-8000-000000000001";

fn row(request: Option<&str>, response: Option<&str>) -> Row {
    Row {
        started_at_us: 1_762_000_000_123_456,
        source: Source::Native,
        provider: Some("claude".into()),
        session_id: Some(SESSION.into()),
        request_body: request.map(str::to_owned),
        response_body: response.map(str::to_owned),
        ..Row::default()
    }
}

fn facts(request: &str, response: &str) -> SendFacts {
    scan(&row(Some(request), Some(response)), &[])
}

const DONE: &str = r#"{"id":"msg_done","type":"message","content":[]}"#;

fn def(tool: &str, raw: &str) -> Def {
    Def {
        tool: tool.into(),
        bytes: raw.len() as i64,
    }
}

fn call(id: &str, tool: &str, skill: Option<&str>) -> Call {
    Call {
        id: hash(id.as_bytes()),
        tool: tool.into(),
        skill: skill.map(str::to_owned),
    }
}

#[test]
fn thread_start_defines_tools_and_keys_by_session_and_first_message() {
    let read = r#"{"name":"Read","input_schema":{"type":"object"}}"#;
    let bash = r#"{"name":"Bash","description":"Runs a command","input_schema":{}}"#;
    let first = r#"{"role":"user","content":"hello"}"#;
    let request = format!(r#"{{"model":"m","tools":[{read}, {bash}],"messages":[{first}]}}"#);
    let start = facts(&request, DONE);
    assert_eq!(start.defs, vec![def("Read", read), def("Bash", bash)]);
    assert!(matches!(start.conv, Conv::Start(_)));
    assert_eq!(start.message, Some(hash(b"msg_done")));
    assert_eq!(start.at_ms, 1_762_000_000_123);
    assert!(!start.cut && !start.skipped_format);

    let mut other = row(Some(&request), Some(DONE));
    other.session_id = Some("00000000-0000-4000-8000-000000000002".into());
    let other = scan(&other, &[]);
    assert_ne!(other.conv, start.conv);
    assert!(matches!(other.conv, Conv::Start(_)));

    let longer = format!(
        r#"{{"model":"m","tools":[{read}, {bash}],"messages":[{first},{{"role":"assistant","content":"hi"}},{{"role":"user","content":"more"}}]}}"#
    );
    assert_eq!(facts(&longer, DONE).conv, start.conv);

    let mut no_session = row(Some(&request), Some(DONE));
    no_session.session_id = None;
    let no_session = scan(&no_session, &[]).conv;
    assert!(matches!(no_session, Conv::Start(_)));
    assert_ne!(no_session, start.conv);
}

#[test]
fn first_message_cache_control_keeps_the_key() {
    let plain = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#;
    let cached = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hello","cache_control":{"type":"ephemeral"}}],"cache_control":{"type":"ephemeral"}}]}"#;
    let other = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hellp"}]}]}"#;
    let split = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hel","x":"lo"}]}]}"#;
    let (plain, cached) = (facts(plain, DONE).conv, facts(cached, DONE).conv);
    assert!(matches!(plain, Conv::Start(_)));
    assert_eq!(plain, cached);
    assert_ne!(facts(other, DONE).conv, plain);
    assert_ne!(facts(split, DONE).conv, plain);
    let text = r#"{"messages":[{"role":"user","content":"hello"}]}"#;
    let text_other = r#"{"messages":[{"role":"user","content":"hellp"}]}"#;
    assert_ne!(facts(text, DONE).conv, facts(text_other, DONE).conv);
}

#[test]
fn continuation_points_at_its_parent() {
    let request = r#"{"thread":{"type":"continue","previous_message_id":"msg_parent"},"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"},{"type":"text","text":"<system-reminder>x</system-reminder>"}]},{"role":"system","content":"note"}]}"#;
    let continued = facts(request, DONE);
    assert_eq!(continued.conv, Conv::Continue(hash(b"msg_parent")));
    assert_eq!(continued.results.len(), 1);
    assert_eq!(continued.results[0].id, hash(b"toolu_1"));
    assert!(continued.defs.is_empty());

    // A thread that does not continue, or names no parent, starts by its first message.
    for thread in [
        r#"{"previous_message_id":"msg_parent"}"#,
        r#"{"type":"new","previous_message_id":"msg_parent"}"#,
        r#"{"type":"continue","previous_message_id":""}"#,
    ] {
        let body = format!(r#"{{"thread":{thread},"messages":[{{"role":"user","content":"go"}}]}}"#);
        assert!(matches!(facts(&body, DONE).conv, Conv::Start(_)), "{thread}");
    }
    assert_eq!(facts(r#"{"model":"m"}"#, DONE).conv, Conv::None);
    assert_eq!(facts(r#"{"messages":[]}"#, DONE).conv, Conv::None);
}

#[test]
fn streamed_calls_and_skill_name_from_deltas() {
    let response = [
        r#"event: message_start"#,
        r#"data: {"type":"message_start","message":{"id":"msg_stream","type":"message","role":"assistant","content":[]}}"#,
        "",
        r#"event: content_block_start"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "",
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_read","name":"Read","input":{}}}"#,
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"skill\":\"not-a-skill\"}"}}"#,
        r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_skill","name":"Skill","input":{}}}"#,
        r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"sk"}}"#,
        r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"ill\": \"rev"}}"#,
        r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"iew\"}"}}"#,
        r#"data: {"type":"content_block_stop","index":2}"#,
        r#"data: {"type":"message_stop"}"#,
        "",
    ]
    .join("\n");
    let streamed = facts(r#"{"messages":[{"role":"user","content":"go"}]}"#, &response);
    assert_eq!(streamed.message, Some(hash(b"msg_stream")));
    assert_eq!(
        streamed.calls,
        vec![
            call("toolu_read", "Read", None),
            call("toolu_skill", "Skill", Some("review"))
        ]
    );
    assert!(!streamed.cut);
}

#[test]
fn buffered_response_calls() {
    let response = r#"{"id":"msg_buffered","type":"message","content":[{"type":"text","text":"x"},{"type":"tool_use","id":"toolu_b","name":"Skill","input":{"skill":"plan","args":"now"}},{"type":"tool_use","id":"toolu_c","name":"Grep","input":{"pattern":"a"}}]}"#;
    let buffered = facts(r#"{"messages":[{"role":"user","content":"go"}]}"#, response);
    assert_eq!(buffered.message, Some(hash(b"msg_buffered")));
    assert_eq!(
        buffered.calls,
        vec![call("toolu_b", "Skill", Some("plan")), call("toolu_c", "Grep", None)]
    );
}

#[test]
fn results_carry_error_flag_and_size() {
    let blocks = r#"[{"type":"text","text":"x"}]"#;
    let request = format!(
        r#"{{"messages":[{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"boom"}},{{"type":"tool_result","tool_use_id":"t2","is_error":"true","content":{blocks}}},{{"type":"tool_result","tool_use_id":"t3"}}]}}]}}"#
    );
    let results = facts(&request, DONE).results;
    assert_eq!(
        results,
        vec![
            Outcome {
                id: hash(b"t1"),
                is_error: true,
                bytes: 6,
                loaded: 0
            },
            Outcome {
                id: hash(b"t2"),
                is_error: false,
                bytes: blocks.len() as i64,
                loaded: 0
            },
            Outcome {
                id: hash(b"t3"),
                is_error: false,
                bytes: 0,
                loaded: 0
            },
        ]
    );
}

#[test]
fn the_text_block_after_a_result_is_its_loaded_bytes() {
    let a = r#"{"type":"text","text":"Base directory for this skill: /synthetic"}"#;
    let b = r#"{"type":"text","text":"Skill body"}"#;
    let c = r#"{"type":"text","text":"after the second"}"#;
    let request = format!(
        r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"before"}},{{"type":"tool_result","tool_use_id":"s1","content":"Launching skill: review"}},{a},{b},{{"type":"tool_result","tool_use_id":"s2","content":"ok"}},{c}]}},{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"s3","content":"ok"}}]}}]}}"#
    );
    let loaded: Vec<i64> = facts(&request, DONE).results.iter().map(|r| r.loaded).collect();
    // Only the text block right after a result is what it loaded; a reminder block
    // after the skill text is not.
    assert_eq!(loaded, vec![a.len() as i64, c.len() as i64, 0]);
    let request = r#"{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"s4","content":"ok"},{"type":"image","source":{}},{"type":"text","text":"later"}]}]}"#;
    assert_eq!(facts(request, DONE).results[0].loaded, 0);
}

#[test]
fn tools_are_named_by_name_and_by_type_only_without_one() {
    let web = r#"{"type":"web_search_20250305","name":"web_search","max_uses":5}"#;
    let bash = r#"{"type":"bash_20250124","name":"bash"}"#;
    let custom = r#"{"type":"custom","name":"Lookup","input_schema":{}}"#;
    let plain = r#"{"name":"Edit","input_schema":{}}"#;
    let unnamed = r#"{"type":"web_fetch_20250910"}"#;
    let request = format!(
        r#"{{"tools":[{web},{bash},{custom},{plain},{unnamed}],"messages":[{{"role":"user","content":"go"}}]}}"#
    );
    assert_eq!(
        facts(&request, DONE).defs,
        vec![
            def("web_search", web),
            def("bash", bash),
            def("Lookup", custom),
            def("Edit", plain),
            def("web_fetch_20250910", unnamed)
        ]
    );
}

#[test]
fn server_tool_calls_and_results_count() {
    let failed = r#"{"type":"web_search_tool_result_error","error_code":"max_uses_exceeded"}"#;
    let found = r#"[{"type":"web_search_result","url":"https://example.invalid/","title":"t"}]"#;
    let request = format!(
        r#"{{"tools":[{{"type":"web_search_20250305","name":"web_search"}}],"messages":[{{"role":"user","content":"go"}},{{"role":"assistant","content":[{{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{{"query":"q"}}}},{{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":{failed}}},{{"type":"server_tool_use","id":"srvtoolu_2","name":"web_search","input":{{}}}},{{"type":"web_search_tool_result","tool_use_id":"srvtoolu_2","content":{found}}}]}}]}}"#
    );
    let buffered = r#"{"id":"msg_s","content":[{"type":"server_tool_use","id":"srvtoolu_3","name":"web_search","input":{}},{"type":"web_search_tool_result","tool_use_id":"srvtoolu_3","is_error":true,"content":[]}]}"#;
    let read = facts(&request, buffered);
    assert_eq!(
        read.calls,
        vec![
            call("srvtoolu_1", "web_search", None),
            call("srvtoolu_2", "web_search", None),
            call("srvtoolu_3", "web_search", None)
        ]
    );
    assert_eq!(
        read.results,
        vec![
            Outcome {
                id: hash(b"srvtoolu_1"),
                is_error: true,
                bytes: failed.len() as i64,
                loaded: 0
            },
            Outcome {
                id: hash(b"srvtoolu_2"),
                is_error: false,
                bytes: found.len() as i64,
                loaded: 0
            },
            Outcome {
                id: hash(b"srvtoolu_3"),
                is_error: true,
                bytes: 2,
                loaded: 0
            },
        ]
    );
    let streamed = [
        r#"data: {"type":"message_start","message":{"id":"msg_t"}}"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_4","name":"web_search","input":{}}}"#,
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_4","content":[]}}"#,
    ]
    .join("\n");
    let streamed = facts(r#"{"messages":[{"role":"user","content":"go"}]}"#, &streamed);
    assert_eq!(streamed.calls, vec![call("srvtoolu_4", "web_search", None)]);
    assert_eq!(streamed.results.len(), 1);
    assert!(!streamed.results[0].is_error);
}

#[test]
fn aliases_read_back_and_real_mcp_names_stay() {
    let prefixes = vec![cpa_exec::claude::alias_prefix("k1")];
    let alias = format!("{}abandon_Read", prefixes[0]);
    let real = "mcp__ccd_session__mark_chapter";
    let request = format!(
        r#"{{"tools":[{{"name":"{alias}"}},{{"name":"{real}"}}],"messages":[{{"role":"user","content":"go"}},{{"role":"assistant","content":[{{"type":"tool_use","id":"u1","name":"{alias}","input":{{}}}},{{"type":"tool_use","id":"u2","name":"{real}","input":{{}}}}]}}]}}"#
    );
    let response =
        format!(r#"{{"id":"msg_a","content":[{{"type":"tool_use","id":"u3","name":"{alias}","input":{{}}}}]}}"#);
    let found = scan(&row(Some(&request), Some(&response)), &prefixes);
    let defined: Vec<&str> = found.defs.iter().map(|d| d.tool.as_str()).collect();
    assert_eq!(defined, vec!["Read", real]);
    assert_eq!(
        found.calls,
        vec![
            call("u1", "Read", None),
            call("u2", real, None),
            call("u3", "Read", None)
        ]
    );
    // Without the prefix configured, the alias is an MCP name like any other.
    let unconfigured = scan(&row(Some(&request), Some(&response)), &[]);
    assert_eq!(unconfigured.defs[0].tool, alias);
}

#[test]
fn full_history_resend_reports_every_call() {
    let request = r#"{"fallbacks":[],"tools":[{"name":"Read"}],"messages":[{"role":"user","content":"go"},{"role":"assistant","content":[{"type":"text","text":"t"},{"type":"tool_use","id":"r1","name":"Read","input":{"file_path":"/a"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"r1","content":"A"}]},{"role":"assistant","content":[{"type":"tool_use","id":"r2","name":"Read","input":{"file_path":"/b"}},{"type":"tool_use","id":"r3","name":"Bash","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"r2","content":"B"},{"type":"tool_result","tool_use_id":"r3","content":"C","is_error":true}]}]}"#;
    let resent = facts(request, DONE);
    assert_eq!(
        resent.calls,
        vec![
            call("r1", "Read", None),
            call("r2", "Read", None),
            call("r3", "Bash", None)
        ]
    );
    let ids: Vec<i64> = resent.results.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![hash(b"r1"), hash(b"r2"), hash(b"r3")]);
    assert!(matches!(resent.conv, Conv::Start(_)));
}

#[test]
fn cut_response_keeps_the_request_facts() {
    let request = r#"{"tools":[{"name":"Read"}],"messages":[{"role":"user","content":"go"},{"role":"assistant","content":[{"type":"tool_use","id":"r1","name":"Read","input":{}}]}]}"#;
    let response = r#"{"id":"msg_cut","content":[{"type":"tool_use","id":"r9","name":"Read","input":{}}]}"#;
    let mut truncated = row(Some(request), Some(response));
    truncated.response_truncated = true;
    for cut in [
        scan(&truncated, &[]),
        facts(request, r#"data: {"type":"message_start","message":{"id":"msg_x"}"#),
        facts(request, "not json"),
        scan(&row(Some(request), None), &[]),
    ] {
        assert!(cut.cut);
        assert_eq!(cut.defs, vec![def("Read", r#"{"name":"Read"}"#)]);
        assert_eq!(cut.calls, vec![call("r1", "Read", None)]);
        assert!(matches!(cut.conv, Conv::Start(_)));
        assert_eq!(cut.message, None);
    }
    let mut request_cut = row(Some(request), Some(response));
    request_cut.request_truncated = true;
    let request_cut = scan(&request_cut, &[]);
    assert!(request_cut.cut);
    assert!(request_cut.defs.is_empty());
    assert_eq!(request_cut.conv, Conv::None);
    assert_eq!(request_cut.message, Some(hash(b"msg_cut")));
    assert_eq!(request_cut.calls, vec![call("r9", "Read", None)]);
}

#[test]
fn other_providers_are_skipped() {
    let request = r#"{"tools":[{"name":"Read"}],"messages":[{"role":"user","content":"go"}]}"#;
    let mut codex = row(Some(request), Some(DONE));
    codex.provider = Some("codex".into());
    let skipped = scan(&codex, &[]);
    assert!(skipped.skipped_format);
    assert!(!skipped.cut);
    assert!(skipped.defs.is_empty() && skipped.calls.is_empty() && skipped.results.is_empty());
    assert_eq!(skipped.conv, Conv::None);
    assert_eq!(skipped.message, None);
    let mut none = row(Some(request), Some(DONE));
    none.provider = None;
    assert!(scan(&none, &[]).skipped_format);
}
