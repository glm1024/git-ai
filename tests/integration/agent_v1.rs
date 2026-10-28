use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

fn parse_agent_v1(hook_input: &str) -> Result<Vec<ParsedHookEvent>, git_ai::error::GitAiError> {
    resolve_preset("agent-v1")?.parse(hook_input, "t_test")
}

#[test]
fn test_agent_v1_human_checkpoint_with_dirty_files() {
    let hook_input = json!({
        "type": "human",
        "repo_working_dir": "/Users/test/project",
        "will_edit_filepaths": ["/Users/test/project/file.ts"],
        "dirty_files": {
            "/Users/test/project/file.ts": "console.log('hello');"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "human");
            assert_eq!(e.context.agent_id.id, "human");
            assert_eq!(e.context.agent_id.model, "human");
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/project"));
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert_eq!(dirty_files.len(), 1);
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file.ts"))
                    .unwrap(),
                "console.log('hello');"
            );
        }
        _ => panic!("Expected PreFileEdit for human checkpoint"),
    }
}

#[test]
fn test_agent_v1_ai_agent_checkpoint_with_dirty_files() {
    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["/Users/test/project/file.ts"],
        "transcript": {"messages": []},
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": "test-123",
        "dirty_files": {
            "/Users/test/project/file.ts": "console.log('hello');"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(e.context.agent_id.tool, "test-agent");
            assert_eq!(e.context.agent_id.id, "test-123");
            assert_eq!(e.context.agent_id.model, "test-model");
            assert_eq!(e.context.cwd, PathBuf::from("/Users/test/project"));
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert_eq!(dirty_files.len(), 1);
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file.ts"))
                    .unwrap(),
                "console.log('hello');"
            );
            // Inline transcripts removed - should now be None
            assert!(e.stream_source.is_none());
        }
        _ => panic!("Expected PostFileEdit for ai_agent checkpoint"),
    }
}

#[test]
fn test_agent_v1_human_checkpoint_without_dirty_files() {
    let hook_input = json!({
        "type": "human",
        "repo_working_dir": "/Users/test/project",
        "will_edit_filepaths": ["/Users/test/project/file.ts"]
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert!(e.dirty_files.is_none());
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
        }
        _ => panic!("Expected PreFileEdit"),
    }
}

#[test]
fn test_agent_v1_ai_agent_checkpoint_without_dirty_files() {
    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["/Users/test/project/file.ts"],
        "transcript": {"messages": []},
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": "test-123"
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert!(e.dirty_files.is_none());
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/file.ts")]
            );
            assert_eq!(e.context.agent_id.tool, "test-agent");
            assert_eq!(e.context.agent_id.id, "test-123");
            assert_eq!(e.context.agent_id.model, "test-model");
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_agent_v1_dirty_files_multiple_files() {
    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["/Users/test/project/file1.ts", "/Users/test/project/file2.ts"],
        "transcript": {"messages": []},
        "agent_name": "test-agent",
        "model": "test-model",
        "conversation_id": "test-123",
        "dirty_files": {
            "/Users/test/project/file1.ts": "content1",
            "/Users/test/project/file2.ts": "content2"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert_eq!(dirty_files.len(), 2);
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file1.ts"))
                    .unwrap(),
                "content1"
            );
            assert_eq!(
                dirty_files
                    .get(&PathBuf::from("/Users/test/project/file2.ts"))
                    .unwrap(),
                "content2"
            );
            assert_eq!(e.file_paths.len(), 2);
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_agent_v1_dirty_files_relative_paths_resolved_to_absolute() {
    let hook_input = json!({
        "type": "human",
        "repo_working_dir": "/Users/test/project",
        "will_edit_filepaths": ["src/main.rs"],
        "dirty_files": {
            "src/main.rs": "fn main() {}"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    match &events[0] {
        ParsedHookEvent::PreFileEdit(e) => {
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/src/main.rs")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert!(
                dirty_files.contains_key(&PathBuf::from("/Users/test/project/src/main.rs")),
                "dirty_files keys should be resolved to absolute paths"
            );
        }
        _ => panic!("Expected PreFileEdit"),
    }

    let hook_input = json!({
        "type": "ai_agent",
        "repo_working_dir": "/Users/test/project",
        "edited_filepaths": ["src/main.rs"],
        "agent_name": "github-copilot-jetbrains",
        "model": "unknown",
        "conversation_id": "session-1",
        "dirty_files": {
            "src/main.rs": "fn main() { println!(\"hello\"); }"
        }
    })
    .to_string();

    let events = parse_agent_v1(&hook_input).unwrap();
    match &events[0] {
        ParsedHookEvent::PostFileEdit(e) => {
            assert_eq!(
                e.file_paths,
                vec![PathBuf::from("/Users/test/project/src/main.rs")]
            );
            let dirty_files = e.dirty_files.as_ref().unwrap();
            assert!(
                dirty_files.contains_key(&PathBuf::from("/Users/test/project/src/main.rs")),
                "dirty_files keys should be resolved to absolute paths"
            );
        }
        _ => panic!("Expected PostFileEdit"),
    }
}

#[test]
fn test_agent_v1_relative_dirty_files_e2e_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("test.txt");

    fs::write(&file_path, "original line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("test.txt");
    file.assert_committed_lines(crate::lines!["original line".unattributed_human(),]);

    let repo_dir = repo.path().to_string_lossy().to_string();

    let pre_edit_content = "original line\n";
    let human_payload = json!({
        "type": "human",
        "repo_working_dir": repo_dir,
        "will_edit_filepaths": ["test.txt"],
        "dirty_files": {
            "test.txt": pre_edit_content
        }
    })
    .to_string();
    repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &human_payload])
        .unwrap();

    let post_edit_content = "original line\nAI added line\n";
    fs::write(&file_path, post_edit_content).unwrap();

    let ai_payload = json!({
        "type": "ai_agent",
        "repo_working_dir": repo_dir,
        "edited_filepaths": ["test.txt"],
        "agent_name": "github-copilot-jetbrains",
        "model": "unknown",
        "conversation_id": "session-123",
        "dirty_files": {
            "test.txt": post_edit_content
        }
    })
    .to_string();
    repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &ai_payload])
        .unwrap();

    repo.stage_all_and_commit("AI edit").unwrap();
    file.assert_committed_lines(crate::lines![
        "original line".unattributed_human(),
        "AI added line".ai(),
    ]);
}

#[test]
fn test_agent_v1_shell_command_e2e_attribution() {
    let repo = TestRepo::new();
    let file_path = repo.path().join("script-output.txt");

    fs::write(&file_path, "base line\n").unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();
    let mut file = repo.filename("script-output.txt");
    file.assert_committed_lines(crate::lines!["base line".unattributed_human(),]);

    let repo_dir = repo.canonical_path().to_string_lossy().to_string();

    let pre_payload = json!({
        "type": "pre_shell_command",
        "repo_working_dir": repo_dir,
        "agent_name": "agent-v1-test",
        "model": "test-model",
        "conversation_id": "shell-session-123",
        "tool_use_id": "shell-tool-1",
        "command": "printf 'created by shell\\n' >> script-output.txt"
    })
    .to_string();
    repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &pre_payload])
        .unwrap();

    fs::write(&file_path, "base line\ncreated by shell\n").unwrap();

    let post_payload = json!({
        "type": "post_shell_command",
        "repo_working_dir": repo_dir,
        "agent_name": "agent-v1-test",
        "model": "test-model",
        "conversation_id": "shell-session-123",
        "tool_use_id": "shell-tool-1",
        "command": "printf 'created by shell\\n' >> script-output.txt"
    })
    .to_string();
    repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &post_payload])
        .unwrap();

    repo.stage_all_and_commit("Agent v1 shell edit").unwrap();
    file.assert_committed_lines(crate::lines![
        "base line".unattributed_human(),
        "created by shell".ai(),
    ]);
}

/// The external conversation remains one session when its model changes. Each
/// edit must keep the model reported by its own checkpoint, including carryover.
#[test]
fn test_agent_v1_model_switch_preserves_checkpoint_identity() {
    assert_model_switch_identity(false);
}

#[test]
fn test_agent_v1_model_switch_preserves_partial_commit_identity() {
    assert_model_switch_identity(true);
}

fn assert_model_switch_identity(partial: bool) {
    use git_ai::authorship::authorship_log_serialization::generate_session_id;
    use git_ai::git::repository::find_repository_in_path;
    use std::collections::HashMap;

    let (_metrics_dir, metrics_db_path) = crate::test_utils::isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&["commit", "--allow-empty", "-m", "Fixture base"])
        .unwrap();
    for (path, model, content) in [
        ("a.txt", "model-A", "from model A\n"),
        ("b.txt", "model-B", "from model B\n"),
    ] {
        let full_path = repo.path().join(path);
        let pre = json!({
            "type": "human", "repo_working_dir": repo.path(),
            "will_edit_filepaths": [full_path],
        });
        repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &pre.to_string()])
            .unwrap();
        fs::write(&full_path, content).unwrap();
        let post = json!({
            "type": "ai_agent", "repo_working_dir": repo.path(),
            "edited_filepaths": [full_path], "agent_name": "codex",
            "conversation_id": "session-A", "model": model,
        });
        repo.git_ai(&["checkpoint", "agent-v1", "--hook-input", &post.to_string()])
            .unwrap();
    }
    if partial {
        repo.git(&["add", "a.txt"]).unwrap();
    } else {
        repo.git(&["add", "."]).unwrap();
    }
    let first = repo.commit("Commit model identities").unwrap();
    repo.filename("a.txt")
        .assert_committed_lines(crate::lines!["from model A".ai()]);
    let repository = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let assert_identity =
        |log: &git_ai::authorship::authorship_log_serialization::AuthorshipLog,
         path: &str,
         model: &str| {
            let (_, hash, prompt) = log
                .get_line_attribution(&repository, path, 1, &mut HashMap::new())
                .expect("checkpointed line should be attributed");
            let agent = prompt.expect("AI identity").agent_id;
            assert_eq!(agent.id, "session-A", "preserve real conversation ID");
            assert_eq!(agent.model, model, "preserve {path} checkpoint model");
            assert!(
                hash.unwrap()
                    .starts_with(&format!("{}::", generate_session_id("session-A", "codex")))
            );
            assert_eq!(log.metadata.sessions.len(), 1, "one real conversation");
        };
    assert_identity(&first.authorship_log, "a.txt", "model-A");
    let session = first
        .authorship_log
        .metadata
        .sessions
        .values()
        .next()
        .unwrap();
    assert_eq!(
        session.trace_models.len(),
        if partial { 1 } else { 2 },
        "new note keeps only landed traces"
    );
    if partial {
        let initial = repo
            .current_working_logs()
            .read_initial_attributions()
            .unwrap();
        let carried = initial.sessions.values().next().unwrap();
        assert_eq!(
            carried.trace_models.len(),
            1,
            "INITIAL keeps only carried traces"
        );
        assert_eq!(carried.trace_models.values().next().unwrap(), "model-B");
    }
    let stats = repo.stats().unwrap();
    assert_eq!(stats.tool_model_breakdown["codex::model-A"].ai_additions, 1);
    let diff: serde_json::Value = serde_json::from_str(
        &repo
            .git_ai(&["diff", &first.commit_sha, "--json", "--include-stats"])
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        diff["commit_stats"]["tool_model_breakdown"]["codex::model-A"]["ai_lines_added"],
        1
    );
    if !partial {
        assert_eq!(
            diff["commit_stats"]["tool_model_breakdown"]["codex::model-B"]["ai_lines_added"],
            1
        );
    }
    repo.git_ai(&["await", "--timeout", "30"]).unwrap();
    let db =
        git_ai::metrics::db::MetricsDatabase::open_at_path(std::path::Path::new(&metrics_db_path))
            .unwrap();
    let events = db
        .get_metric_history(
            0,
            None,
            &[git_ai::metrics::types::MetricEventId::Committed as u16],
        )
        .unwrap();
    let event = events
        .iter()
        .find(|record| {
            record
                .event
                .attrs
                .get(&git_ai::metrics::attrs::attr_pos::COMMIT_SHA.to_string())
                .and_then(|value| value.as_str())
                == Some(first.commit_sha.as_str())
        })
        .expect("formal committed event");
    let values = &event.event.values;
    let pairs = values[&git_ai::metrics::events::committed_pos::TOOL_MODEL_PAIRS.to_string()]
        .as_array()
        .unwrap();
    let additions = values[&git_ai::metrics::events::committed_pos::AI_ADDITIONS.to_string()]
        .as_array()
        .unwrap();
    for model in if partial {
        vec!["model-A"]
    } else {
        vec!["model-A", "model-B"]
    } {
        let pair = format!("codex::{model}");
        let index = pairs
            .iter()
            .position(|value| value.as_str() == Some(pair.as_str()))
            .expect("checkpoint model in formal Event 1");
        assert_eq!(additions[index], 1, "Event 1 counts {model} separately");
    }
    if partial {
        repo.git(&["add", "b.txt"]).unwrap();
        let second = repo.commit("Commit carryover model identity").unwrap();
        repo.filename("a.txt")
            .assert_committed_lines(crate::lines!["from model A".ai()]);
        repo.filename("b.txt")
            .assert_committed_lines(crate::lines!["from model B".ai()]);
        assert_identity(&second.authorship_log, "b.txt", "model-B");
        // A later valid note may only carry its own model/trace. Looking up an
        // older full trace must continue to the note that actually contains it.
        let old_author = first
            .authorship_log
            .attestations
            .iter()
            .find(|file| file.file_path == "a.txt")
            .unwrap()
            .entries[0]
            .hash
            .clone();
        let mut later_note = second.authorship_log.clone();
        let session = later_note.metadata.sessions.values_mut().next().unwrap();
        session.agent_id.model = "model-B".into();
        session.trace_models.retain(|_, model| model == "model-B");
        git_ai::git::notes_api::write_note(
            &repository,
            &second.commit_sha,
            &later_note.serialize_to_string().unwrap(),
        )
        .unwrap();
        let (source_commit, prompt) =
            git_ai::authorship::prompt_utils::find_prompt_in_history(&repository, &old_author, 0)
                .unwrap();
        assert_eq!(source_commit, first.commit_sha);
        assert_eq!(prompt.agent_id.model, "model-A");
        assert_eq!(
            repo.stats().unwrap().tool_model_breakdown["codex::model-B"].ai_additions,
            1
        );
    } else {
        repo.filename("b.txt")
            .assert_committed_lines(crate::lines!["from model B".ai()]);
        assert_identity(&first.authorship_log, "b.txt", "model-B");
        assert_eq!(stats.tool_model_breakdown["codex::model-B"].ai_additions, 1);
        let mut original_note = first.authorship_log.clone();
        original_note
            .metadata
            .sessions
            .values_mut()
            .next()
            .unwrap()
            .record_trace_model("t_unreferenced_history", "unused-model");
        git_ai::git::notes_api::write_note(
            &repository,
            &first.commit_sha,
            &original_note.serialize_to_string().unwrap(),
        )
        .unwrap();
        repo.git(&["commit", "--amend", "-m", "Amended model identities"])
            .unwrap();
        repo.filename("a.txt")
            .assert_committed_lines(crate::lines!["from model A".ai()]);
        repo.filename("b.txt")
            .assert_committed_lines(crate::lines!["from model B".ai()]);
        let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap();
        let amended = git_ai::authorship::authorship_log_serialization::AuthorshipLog::deserialize_from_string(
            &repo.read_authorship_note(amended_sha.trim()).unwrap()
        ).unwrap();
        assert_identity(&amended, "a.txt", "model-A");
        assert_identity(&amended, "b.txt", "model-B");
        assert_eq!(
            amended
                .metadata
                .sessions
                .values()
                .next()
                .unwrap()
                .trace_models
                .len(),
            2,
            "amend keeps only final attestation traces"
        );
        assert!(
            repo.read_authorship_note(&first.commit_sha)
                .unwrap()
                .contains("t_unreferenced_history"),
            "amend does not prune the historical source note"
        );
    }
}
