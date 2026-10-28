use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use crate::test_utils::{extract_json_object, isolated_metrics_db_path};
use git_ai::authorship::authorship_log_serialization::generate_session_id;
use git_ai::metrics::attrs::attr_pos;
use git_ai::metrics::db::MetricsDatabase;
use git_ai::metrics::events::{checkpoint_pos, committed_pos};
use git_ai::metrics::types::MetricEvent;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

const TOOL: &str = "deepseek-harness";
const SESSION: &str = "dsh-adaptation-fixture-session";

// These are the plugin's actual default payloads. In particular, file pre has
// no invented agent identity, and no payload carries a transcript or usage.
fn checkpoint(repo: &TestRepo, payload: Value) {
    repo.git_ai_with_stdin(
        &[
            "checkpoint",
            "agent-v1",
            "--strict-errors",
            "--hook-input",
            "stdin",
        ],
        payload.to_string().as_bytes(),
    )
    .expect("the plugin requires an applied strict checkpoint acknowledgment");
}

fn metrics(repo: &TestRepo, db_path: &str) -> Vec<MetricEvent> {
    repo.git_ai(&["await", "--timeout", "30"]).unwrap();
    let db = MetricsDatabase::open_at_path(Path::new(db_path)).unwrap();
    db.get_metric_history(0, None, &[1, 2, 4, 5, 6, 9, 11])
        .unwrap()
        .into_iter()
        .map(|record| record.event)
        .collect()
}

fn sparse_str(values: &git_ai::metrics::types::SparseArray, position: usize) -> Option<&str> {
    values.get(&position.to_string()).and_then(Value::as_str)
}

fn usage(repo: &TestRepo, db_path: &str) -> Value {
    let output = repo
        .git_ai_with_env(
            &["usage", "--json"],
            &[("GIT_AI_TEST_METRICS_DB_PATH", db_path)],
        )
        .unwrap();
    serde_json::from_str(&extract_json_object(&output)).unwrap()
}

fn assert_no_transcript_or_token_events(events: &[MetricEvent]) {
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.event_id, 5 | 6 | 9 | 11)),
        "hook-only DSH must not acquire a transcript/token/OTEL stream"
    );
}

#[test]
fn test_dsh_file_checkpoint_reports_two_models_without_native_session_stream() {
    let (_metrics_dir, db_path) = isolated_metrics_db_path();
    let repo = TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", db_path.as_str())]);
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/dsh-adaptation-fixture.git",
    ])
    .unwrap();
    fs::write(repo.path().join("base.txt"), "manual baseline\n").unwrap();
    repo.stage_all_and_commit("Manual baseline").unwrap();
    repo.filename("base.txt")
        .assert_committed_lines(crate::lines!["manual baseline".unattributed_human()]);

    for (path, model, content) in [
        ("first.txt", "model-A", "DSH_MODEL_A\n"),
        ("second.txt", "model-B", "DSH_MODEL_B\n"),
    ] {
        let full = repo.path().join(path);
        checkpoint(
            &repo,
            json!({"type":"human", "repo_working_dir":repo.path(), "will_edit_filepaths":[full]}),
        );
        fs::write(&full, content).unwrap();
        checkpoint(
            &repo,
            json!({"type":"ai_agent", "repo_working_dir":repo.path(), "agent_name":TOOL,
            "model":model, "conversation_id":SESSION, "edited_filepaths":[full]}),
        );
    }
    let commit = repo.stage_all_and_commit("DSH model checkpoints").unwrap();
    repo.filename("base.txt")
        .assert_committed_lines(crate::lines!["manual baseline".unattributed_human()]);
    repo.filename("first.txt")
        .assert_committed_lines(crate::lines!["DSH_MODEL_A".ai()]);
    repo.filename("second.txt")
        .assert_committed_lines(crate::lines!["DSH_MODEL_B".ai()]);
    let repository =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    for (path, model) in [("first.txt", "model-A"), ("second.txt", "model-B")] {
        let (_, author, prompt) = commit
            .authorship_log
            .get_line_attribution(&repository, path, 1, &mut std::collections::HashMap::new())
            .unwrap();
        let agent = prompt.unwrap().agent_id;
        assert_eq!(agent.tool, TOOL);
        assert_eq!(agent.id, SESSION);
        assert_eq!(agent.model, model);
        assert!(
            author
                .unwrap()
                .starts_with(&format!("{}::", generate_session_id(SESSION, TOOL)))
        );
    }
    let events = metrics(&repo, &db_path);
    assert_no_transcript_or_token_events(&events);
    let emitted: Vec<_> = events
        .iter()
        .filter(|event| {
            (event.event_id == 4 && sparse_str(&event.attrs, attr_pos::TOOL) == Some(TOOL))
                || (event.event_id == 1
                    && sparse_str(&event.attrs, attr_pos::COMMIT_SHA)
                        == Some(commit.commit_sha.as_str()))
        })
        .cloned()
        .collect();
    assert_eq!(
        emitted.iter().filter(|event| event.event_id == 4).count(),
        2
    );
    for event in emitted.iter().filter(|event| event.event_id == 4) {
        assert_eq!(
            sparse_str(&event.attrs, attr_pos::SESSION_ID),
            Some(generate_session_id(SESSION, TOOL).as_str())
        );
        assert_eq!(
            sparse_str(&event.attrs, attr_pos::EXTERNAL_SESSION_ID),
            Some(SESSION)
        );
        assert_eq!(event.values[&checkpoint_pos::LINES_ADDED.to_string()], 1);
    }
    let committed = emitted.iter().find(|event| event.event_id == 1).unwrap();
    assert_eq!(
        committed.values[&committed_pos::TOOL_MODEL_PAIRS.to_string()],
        json!([
            "all",
            "deepseek-harness::model-A",
            "deepseek-harness::model-B"
        ])
    );
    assert_eq!(
        committed.values[&committed_pos::AI_ADDITIONS.to_string()],
        json!([2, 1, 1])
    );

    // Opt-in export lets the backend contract test consume real engine wire
    // events without persisting machine-specific fixture paths in the repo.
    if let Some(path) = std::env::var_os("GIT_AI_DSH_METRICS_FIXTURE_PATH") {
        fs::write(path, serde_json::to_vec_pretty(&json!({"fixture":"dsh-agent-v1-file-checkpoints",
            "agent_name":TOOL, "external_session_id":SESSION, "wire_batch":{"v":1,"events":emitted}})).unwrap()).unwrap();
    }
    let activity = usage(&repo, &db_path);
    assert_eq!(
        activity["sessions"]["total"], 1,
        "two models are one real DSH conversation"
    );
    assert_eq!(activity["sessions"]["by_tool"], json!([[TOOL, 1]]));
    assert_eq!(activity["checkpoints"]["ai_lines_added"], 2);
    assert_eq!(activity["tokens"]["input"], 0);
    assert_eq!(activity["tokens"]["output"], 0);
    assert_eq!(activity["tokens"]["estimated_cost_usd"], 0.0);
    assert_eq!(activity["tokens"]["by_model"], json!([]));
    assert!(activity["summary"]["favorite_model"].is_null());
}

#[test]
fn test_dsh_shell_checkpoint_preserves_identity_without_command_text() {
    let (_metrics_dir, db_path) = isolated_metrics_db_path();
    let repo = TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", db_path.as_str())]);
    fs::write(repo.path().join("base.txt"), "manual baseline\n").unwrap();
    repo.stage_all_and_commit("Manual baseline").unwrap();
    repo.filename("base.txt")
        .assert_committed_lines(crate::lines!["manual baseline".unattributed_human()]);
    for phase in ["pre_shell_command", "post_shell_command"] {
        if phase == "post_shell_command" {
            fs::write(repo.path().join("shell.txt"), "DSH_SHELL_LINE\n").unwrap();
        }
        checkpoint(
            &repo,
            json!({"type":phase,"repo_working_dir":repo.path(),"agent_name":TOOL,
            "model":"model-shell","conversation_id":SESSION,"tool_use_id":"dsh-shell-fixture-call"}),
        );
    }
    let commit = repo.stage_all_and_commit("DSH shell checkpoint").unwrap();
    repo.filename("base.txt")
        .assert_committed_lines(crate::lines!["manual baseline".unattributed_human()]);
    repo.filename("shell.txt")
        .assert_committed_lines(crate::lines!["DSH_SHELL_LINE".ai()]);
    let session = &commit.authorship_log.metadata.sessions[&generate_session_id(SESSION, TOOL)];
    assert_eq!(session.agent_id.tool, TOOL);
    assert_eq!(session.agent_id.id, SESSION);
    assert_eq!(session.agent_id.model, "model-shell");
    let events = metrics(&repo, &db_path);
    assert_no_transcript_or_token_events(&events);
    assert!(events.iter().any(|event| event.event_id == 4
        && sparse_str(&event.attrs, attr_pos::TOOL) == Some(TOOL)
        && sparse_str(&event.values, checkpoint_pos::EDIT_KIND) == Some("bash")));
}
