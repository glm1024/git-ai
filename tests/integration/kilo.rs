use crate::repos::test_file::ExpectedLineExt;
use crate::test_utils::fixture_path;
use chrono::{DateTime, Utc};
use git_ai::authorship::authorship_log_serialization::generate_session_id;
use git_ai::commands::checkpoint_agent::presets::{ParsedHookEvent, resolve_preset};
use git_ai::metrics::db::MetricsDatabase;
use git_ai::streams::agent::get_agent;
use git_ai::streams::watermark::TimestampWatermark;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn with_kilo_storage<T>(run: impl FnOnce(&std::path::Path) -> T) -> T {
    let storage = tempfile::tempdir().unwrap();
    fs::copy(
        fixture_path("opencode-sqlite/opencode.db"),
        storage.path().join("kilo.db"),
    )
    .unwrap();

    let previous = std::env::var_os("GIT_AI_KILO_STORAGE_PATH");
    unsafe {
        std::env::set_var("GIT_AI_KILO_STORAGE_PATH", storage.path());
    }
    let result = run(storage.path());
    unsafe {
        match previous {
            Some(value) => std::env::set_var("GIT_AI_KILO_STORAGE_PATH", value),
            None => std::env::remove_var("GIT_AI_KILO_STORAGE_PATH"),
        }
    }
    result
}

#[test]
#[serial_test::serial]
fn test_kilo_preset_uses_kilo_identity_and_sqlite_transcript() {
    with_kilo_storage(|storage| {
        let input = json!({
            "hook_event_name": "PostToolUse",
            "session_id": "test-session-123",
            "cwd": "/Users/test/project",
            "tool_name": "edit",
            "tool_use_id": "call-sql-001",
            "tool_input": {"filePath": "/Users/test/project/index.ts"},
            "platform": "vscode",
            "editor_name": "Visual Studio Code"
        })
        .to_string();

        let events = resolve_preset("kilo")
            .unwrap()
            .parse(&input, "t_test")
            .unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedHookEvent::PostFileEdit(event) => {
                assert_eq!(event.context.agent_id.tool, "kilo");
                assert_eq!(event.context.agent_id.model, "gpt-5");
                assert_eq!(event.context.external_session_id, "test-session-123");
                assert_eq!(event.context.cwd, PathBuf::from("/Users/test/project"));
                assert_eq!(
                    event.context.metadata.get("platform").map(String::as_str),
                    Some("vscode")
                );
                assert_eq!(
                    event
                        .context
                        .metadata
                        .get("editor_name")
                        .map(String::as_str),
                    Some("Visual Studio Code")
                );
                let stream = event
                    .stream_source
                    .as_ref()
                    .expect("Kilo transcript source");
                assert_eq!(stream.path, storage.join("kilo.db"));
                assert_eq!(
                    stream.external_parent_session_id.as_deref(),
                    Some("parent-session-456")
                );
                assert_eq!(stream.external_session_id, "test-session-123");
            }
            _ => panic!("Expected PostFileEdit"),
        }
    });
}

#[test]
#[serial_test::serial]
fn test_kilo_preset_projects_all_supported_client_channels() {
    with_kilo_storage(|_| {
        for (platform, client, editor_name) in [
            ("vscode", "vscode", Some("Visual Studio Code")),
            ("jetbrains", "jetbrains", Some("IntelliJ IDEA")),
            ("cli", "cli", None),
        ] {
            let input = json!({
                "hook_event_name": "PostToolUse",
                "session_id": "test-session-123",
                "cwd": "/Users/test/project",
                "tool_name": "edit",
                "tool_use_id": format!("call-{platform}"),
                "tool_input": {"filePath": "/Users/test/project/index.ts"},
                "platform": platform,
                "client": client,
                "editor_name": editor_name
            })
            .to_string();

            let events = resolve_preset("kilo")
                .unwrap()
                .parse(&input, "t_channel")
                .unwrap();
            let ParsedHookEvent::PostFileEdit(event) = &events[0] else {
                panic!("Expected PostFileEdit");
            };
            assert_eq!(event.context.metadata["platform"], platform);
            assert_eq!(event.context.metadata["client"], client);
            assert_eq!(
                event
                    .context
                    .metadata
                    .get("editor_name")
                    .map(String::as_str),
                editor_name
            );
        }
    });
}

#[test]
fn test_kilo_stream_alias_reads_kilo_sqlite_without_losing_raw_events() {
    let fixture = fixture_path("opencode-sqlite/opencode.db");
    let agent = get_agent("kilo").expect("Kilo stream agent must be registered");
    let result = agent
        .read_incremental(
            &fixture,
            Box::new(TimestampWatermark::new(DateTime::<Utc>::UNIX_EPOCH)),
            "test-session-123",
        )
        .unwrap();

    assert!(!result.events.is_empty());
    assert!(result.events.iter().all(|event| {
        event
            .pointer("/message/session_id")
            .and_then(|value| value.as_str())
            == Some("test-session-123")
    }));
}

#[test]
#[serial_test::serial]
fn test_kilo_e2e_checkpoint_and_commit_marks_ai_authorship() {
    use crate::repos::test_repo::TestRepo;

    with_kilo_storage(|_| {
        let mut repo = TestRepo::new();
        repo.patch_git_ai_config(|patch| {
            patch.exclude_prompts_in_repositories = Some(vec![]);
        });

        let repo_root = repo.canonical_path();
        let file_path = repo_root.join("index.ts");
        fs::write(&file_path, "// initial\n").unwrap();
        repo.stage_all_and_commit("Initial commit").unwrap();

        let pre = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "test-session-123",
            "cwd": repo_root,
            "tool_name": "edit",
            "tool_use_id": "call-sql-001",
            "tool_input": {"filePath": file_path}
        })
        .to_string();
        repo.git_ai(&["checkpoint", "kilo", "--hook-input", &pre])
            .unwrap();

        fs::write(&file_path, "// initial\n// Kilo AI edit\n").unwrap();

        let post = json!({
            "hook_event_name": "PostToolUse",
            "session_id": "test-session-123",
            "cwd": repo_root,
            "tool_name": "edit",
            "tool_use_id": "call-sql-001",
            "tool_input": {"filePath": file_path}
        })
        .to_string();
        repo.git_ai(&["checkpoint", "kilo", "--hook-input", &post])
            .unwrap();

        let commit = repo.stage_all_and_commit("Add Kilo AI line").unwrap();
        let session = commit
            .authorship_log
            .metadata
            .sessions
            .values()
            .next()
            .expect("Kilo session record");
        assert_eq!(session.agent_id.tool, "kilo");
        assert_eq!(session.agent_id.model, "gpt-5");

        let mut committed = repo.filename("index.ts");
        committed.assert_committed_lines(crate::lines!["// initial", "// Kilo AI edit".ai()]);
    });
}

#[test]
#[serial_test::serial]
fn test_kilo_checkpoint_metric_precedes_session_events_for_backend_context() {
    use crate::repos::test_repo::TestRepo;

    with_kilo_storage(|_| {
        let metrics_dir = tempfile::tempdir().unwrap();
        let metrics_path = metrics_dir.path().join("metrics.db");
        MetricsDatabase::open_at_path(&metrics_path).unwrap();
        let metrics_path_string = metrics_path.to_string_lossy().to_string();
        let repo = TestRepo::new_with_daemon_env(&[(
            "GIT_AI_TEST_METRICS_DB_PATH",
            metrics_path_string.as_str(),
        )]);

        let repo_root = repo.canonical_path();
        let file_path = repo_root.join("ordered.ts");
        fs::write(&file_path, "// initial\n").unwrap();
        repo.stage_all_and_commit("Initial commit").unwrap();

        for (event_name, content) in [
            ("PreToolUse", "// initial\n"),
            ("PostToolUse", "// initial\n// Kilo ordered edit\n"),
        ] {
            fs::write(&file_path, content).unwrap();
            let input = json!({
                "hook_event_name": event_name,
                "session_id": "test-session-123",
                "cwd": repo_root,
                "tool_name": "edit",
                "tool_use_id": "call-ordering",
                "tool_input": {"filePath": file_path},
                "platform": "vscode"
            })
            .to_string();
            repo.git_ai(&["checkpoint", "kilo", "--hook-input", &input])
                .unwrap();
        }

        let session_id = generate_session_id("test-session-123", "kilo");
        let deadline = Instant::now() + Duration::from_secs(10);
        let rows = loop {
            let conn = rusqlite::Connection::open(&metrics_path).unwrap();
            let mut statement = conn
                .prepare(
                    "SELECT id, event_kind FROM metrics \
                     WHERE session_id = ? AND event_kind IN (4, 5) ORDER BY id",
                )
                .unwrap();
            let rows = statement
                .query_map([&session_id], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            if rows.iter().any(|(_, kind)| *kind == 4) && rows.iter().any(|(_, kind)| *kind == 5) {
                break rows;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for Kilo checkpoint and session metrics: {rows:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        };

        let checkpoint_id = rows
            .iter()
            .find(|(_, kind)| *kind == 4)
            .map(|(id, _)| *id)
            .unwrap();
        let first_session_id = rows
            .iter()
            .find(|(_, kind)| *kind == 5)
            .map(|(id, _)| *id)
            .unwrap();
        assert!(
            checkpoint_id < first_session_id,
            "checkpoint context must be durable before Kilo session/token events: {rows:?}"
        );
    });
}

#[test]
fn test_kilo_installer_is_registered_separately_from_opencode() {
    let installers = git_ai::mdm::agents::get_all_installers();
    let ids: Vec<&str> = installers.iter().map(|installer| installer.id()).collect();
    assert!(ids.contains(&"kilo"));
    assert!(ids.contains(&"opencode"));
}
