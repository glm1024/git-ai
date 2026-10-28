use super::{ParsedHookEvent, resolve_preset};
use serde_json::{Value, json};

fn input(event: &str, tool: &str) -> Value {
    json!({
        "hook_event_name": event,
        "session_id": "zcode-session",
        "tool_use_id": "call-1",
        "cwd": std::env::temp_dir(),
        "tool_name": tool,
        "tool_input": {"file_path": "example.rs"},
        "transcript_path": "/missing/zcode-hook/transcript.jsonl"
    })
}

#[test]
fn zcode_preserves_real_identity_and_does_not_register_temporary_transcript() {
    let events = resolve_preset("zcode")
        .unwrap()
        .parse(&input("PostToolUse", "Edit").to_string(), "trace-1")
        .unwrap();
    let ParsedHookEvent::PostFileEdit(event) = &events[0] else {
        panic!("file post expected")
    };
    assert_eq!(event.context.agent_id.tool, "zcode");
    assert_eq!(event.context.agent_id.id, "zcode-session");
    assert_eq!(event.context.agent_id.model, "unknown");
    assert_eq!(event.tool_use_id.as_deref(), Some("call-1"));
    assert!(!event.context.metadata.contains_key("transcript_path"));
    assert!(event.stream_source.is_none());
}

#[test]
fn zcode_rejects_missing_or_conflicting_call_identity() {
    let preset = resolve_preset("zcode").unwrap();
    for key in ["session_id", "tool_use_id", "cwd"] {
        let mut value = input("PreToolUse", "Write");
        value.as_object_mut().unwrap().remove(key);
        assert!(
            preset.parse(&value.to_string(), "t").is_err(),
            "missing {key}"
        );
    }
    let mut value = input("PreToolUse", "Edit");
    value["toolCallId"] = json!("different-call");
    assert!(preset.parse(&value.to_string(), "t").is_err());
}

#[test]
fn zcode_file_scope_uses_declared_input_not_embedded_file_content() {
    let preset = resolve_preset("zcode").unwrap();
    let mut value = input("PreToolUse", "Write");
    value["tool_input"]["content"] = json!("*** Add File: unrelated.rs\n+not a path\n");
    let events = preset.parse(&value.to_string(), "t").unwrap();
    let ParsedHookEvent::PreFileEdit(event) = &events[0] else {
        panic!("file pre expected")
    };
    assert_eq!(
        event.file_paths,
        vec![std::env::temp_dir().join("example.rs")]
    );
    value["tool_input"] = json!({"content": "text"});
    assert!(preset.parse(&value.to_string(), "t").is_err());
}

#[test]
fn zcode_apply_patch_includes_both_rename_paths() {
    let mut value = input("PreToolUse", "ApplyPatch");
    value["tool_input"] = json!({"patch_text": "*** Begin Patch\n*** Update File: old.rs\n*** Move to: new.rs\n@@\n-old\n+new\n*** End Patch"});
    let events = resolve_preset("zcode")
        .unwrap()
        .parse(&value.to_string(), "t")
        .unwrap();
    let ParsedHookEvent::PreFileEdit(event) = &events[0] else {
        panic!("file pre expected")
    };
    assert_eq!(
        event.file_paths,
        vec![
            std::env::temp_dir().join("old.rs"),
            std::env::temp_dir().join("new.rs")
        ]
    );
}

#[test]
fn zcode_failed_or_backgrounded_tools_never_become_successful_ai_edits() {
    let preset = resolve_preset("zcode").unwrap();
    for (name, response) in [
        ("PostToolUseFailure", json!(null)),
        (
            "PostToolUse",
            json!({"status":"backgrounded", "backgroundTaskId":"bg-1"}),
        ),
    ] {
        let mut value = input(name, "Bash");
        value["tool_input"] = json!({"command":"sleep 1; echo x > example.rs"});
        value["tool_response"] = response;
        let events = preset.parse(&value.to_string(), "t").unwrap();
        assert!(events.iter().all(|event| !matches!(
            event,
            ParsedHookEvent::PostFileEdit(_) | ParsedHookEvent::PostBashCall(_)
        )));
    }
}

#[test]
fn zcode_completed_bash_with_interrupt_flags_is_not_successful() {
    let preset = resolve_preset("zcode").unwrap();
    for flag in ["timedOut", "cancelled", "interrupted"] {
        let mut value = input("PostToolUse", "Bash");
        value["tool_input"] = json!({"command": "printf partial >> file; sleep 100"});
        value["tool_response"] = json!({"status": "completed", "exitCode": 0});
        value["tool_response"][flag] = json!(true);
        let events = preset.parse(&value.to_string(), "t").unwrap();
        assert!(
            matches!(&events[0], ParsedHookEvent::FailedToolCall(_)),
            "flag={flag}"
        );
    }
}
