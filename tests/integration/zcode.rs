use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use chrono::Utc;
use git_ai::metrics::db::MetricsDatabase;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct ZCodeFixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    conn: Connection,
}

impl ZCodeFixture {
    fn new(directory: &Path) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let conn = git_ai::sqlite::open_with_memory_limits(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE session(id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT);
            CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT);
            CREATE TABLE part(id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, data TEXT);
            CREATE TABLE tool_usage(session_id TEXT, tool_call_id TEXT, trace_id TEXT, turn_id TEXT);
            CREATE TABLE model_usage(id TEXT PRIMARY KEY, logical_request_id TEXT, session_id TEXT,
                turn_id TEXT, trace_id TEXT, assistant_message_id TEXT, parent_user_message_id TEXT,
                provider_id TEXT, model_id TEXT, status TEXT, started_at INTEGER, completed_at INTEGER,
                input_tokens INTEGER, output_tokens INTEGER, cache_read_input_tokens INTEGER,
                cache_creation_input_tokens INTEGER, raw_usage_json TEXT);").unwrap();
        conn.execute(
            "INSERT INTO session VALUES('zcode-session', NULL, ?1)",
            [directory.display().to_string()],
        )
        .unwrap();
        Self {
            _dir: dir,
            path,
            conn,
        }
    }

    fn usage(&self, request: &str, model: &str, started_at: i64) {
        self.conn
            .execute(
                "INSERT INTO model_usage VALUES(?1, ?1, 'zcode-session', 'turn', 'trace',
            ?1, 'user', 'zai-plan', ?2, 'completed', ?3, ?3+1, 100, 20, 30, 10, ?4)",
                params![
                    request,
                    model,
                    started_at,
                    "private usage diagnostics must not be copied"
                ],
            )
            .unwrap();
    }

    fn tool(&self, call: &str, request: &str, model: &str) {
        self.usage(request, model, Utc::now().timestamp_millis() - 2_000);
        self.conn.execute("INSERT INTO message VALUES(?1, 'zcode-session', ?2)", params![request,
            json!({"role":"assistant", "modelId":model, "providerId":"zai-plan", "anchor":{"turnId":"turn"}}).to_string()]).unwrap();
        self.conn.execute("INSERT INTO part VALUES(?1, 'zcode-session', ?2, ?3)", params![format!("part-{call}"), request,
            json!({"type":"tool", "callID":call, "state":{"input":{"private":"tool input"}}}).to_string()]).unwrap();
    }

    fn hook(&self, repo: &TestRepo, event: &str, call: Option<&str>, file: Option<&Path>) {
        self.try_hook(repo, event, call, file).unwrap();
    }

    fn try_hook(
        &self,
        repo: &TestRepo,
        event: &str,
        call: Option<&str>,
        file: Option<&Path>,
    ) -> Result<String, String> {
        let mut input = json!({"hook_event_name":event, "sessionId":"zcode-session", "cwd":repo.canonical_path(),
            "traceId":"trace", "turnId":"turn"});
        if let Some(call) = call {
            input["toolCallId"] = json!(call);
            input["toolName"] = json!("Edit");
            input["toolInput"] = json!({"file_path":file.unwrap()});
        }
        if event == "PostToolUseFailure" {
            input["toolOutput"] = json!({"status":"failed", "isError":true});
        }
        self.submit_input(repo, input)
    }

    fn bash_hook(&self, repo: &TestRepo, event: &str, call: &str, command: &str) {
        let mut input = json!({"hook_event_name":event, "sessionId":"zcode-session", "cwd":repo.canonical_path(),
            "traceId":"trace", "turnId":"turn", "toolCallId":call, "toolName":"Bash", "toolInput":{"command":command}});
        if event != "PreToolUse" {
            input["toolOutput"] = if event == "PostToolUseFailure" {
                json!({"status":"failed", "exitCode":1, "isError":true})
            } else {
                json!({"status":"completed", "exitCode":0})
            };
        }
        self.submit_input(repo, input).unwrap();
    }

    fn submit_input(&self, repo: &TestRepo, input: Value) -> Result<String, String> {
        repo.git_ai_with_env(
            &[
                "checkpoint",
                "zcode",
                "--strict-errors",
                "--hook-input",
                &input.to_string(),
            ],
            &[("GIT_AI_ZCODE_STORAGE_PATH", self.path.to_str().unwrap())],
        )
    }
}

fn initial_file(repo: &TestRepo, name: &str) -> PathBuf {
    let file = repo.canonical_path().join(name);
    fs::write(&file, "// original\n").unwrap();
    repo.stage_all_and_commit("Initial human content").unwrap();
    repo.filename(name)
        .assert_committed_lines(crate::lines!["// original".unattributed_human()]);
    file
}

#[test]
fn test_zcode_e2e_exact_tool_model_and_human_preexisting_changes() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });
    let file = initial_file(&repo, "exact.ts");
    let source = ZCodeFixture::new(&repo.canonical_path());
    source.tool("call-a", "request-a", "glm-a");
    source.tool("call-b", "request-b", "glm-b"); // A later model must not relabel call-a.
    fs::write(&file, "// original\n// human before pre\n").unwrap();
    source.hook(&repo, "PreToolUse", Some("call-a"), Some(&file));
    fs::write(
        &file,
        "// original\n// human before pre\n// ZCode AI edit\n",
    )
    .unwrap();
    source.hook(&repo, "PostToolUse", Some("call-a"), Some(&file));
    let commit = repo.stage_all_and_commit("Add ZCode AI edit").unwrap();
    repo.filename("exact.ts")
        .assert_committed_lines(crate::lines![
            "// original".unattributed_human(),
            "// human before pre".unattributed_human(),
            "// ZCode AI edit".ai()
        ]);
    let session = commit
        .authorship_log
        .metadata
        .sessions
        .values()
        .find(|session| session.agent_id.tool == "zcode")
        .expect("ZCode session");
    assert_eq!(session.agent_id.model, "glm-a");

    source.hook(&repo, "PreToolUse", Some("call-b"), Some(&file));
    fs::write(
        &file,
        "// original\n// human before pre\n// ZCode AI edit\n// switched model edit\n",
    )
    .unwrap();
    source.hook(&repo, "PostToolUse", Some("call-b"), Some(&file));
    let commit = repo
        .stage_all_and_commit("Use next model in the same ZCode session")
        .unwrap();
    repo.filename("exact.ts")
        .assert_committed_lines(crate::lines![
            "// original".unattributed_human(),
            "// human before pre".unattributed_human(),
            "// ZCode AI edit".ai(),
            "// switched model edit".ai()
        ]);
    assert!(
        commit
            .authorship_log
            .metadata
            .sessions
            .values()
            .any(|session| session.agent_id.tool == "zcode" && session.agent_id.model == "glm-b")
    );
}

#[test]
fn test_zcode_e2e_failed_partial_write_and_unpaired_post_remain_unknown() {
    let metrics_dir = tempfile::tempdir().unwrap();
    let metrics_path = metrics_dir.path().join("metrics.sqlite");
    MetricsDatabase::open_at_path(&metrics_path).unwrap();
    let metrics_string = metrics_path.display().to_string();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_string.as_str())]);
    let file = initial_file(&repo, "failed.ts");
    let source = ZCodeFixture::new(&repo.canonical_path());
    source.tool("failed-call", "failed-request", "glm-a");
    source.hook(&repo, "PreToolUse", Some("failed-call"), Some(&file));
    fs::write(&file, "// original\n// partial failed write\n").unwrap();
    source.hook(
        &repo,
        "PostToolUseFailure",
        Some("failed-call"),
        Some(&file),
    );
    // The CLI acknowledges receipt before asynchronous admission. Send these
    // deliberately rejected requests without registering TestRepo completion
    // expectations: no checkpoint will be executed for either receipt.
    let send_rejected_post = |call: &str| {
        let input = json!({"hook_event_name":"PostToolUse", "sessionId":"zcode-session",
            "cwd":repo.canonical_path(), "traceId":"trace", "turnId":"turn",
            "toolCallId":call, "toolName":"Edit", "toolInput":{"file_path":file}})
        .to_string();
        let output = repo
            .git_ai_command_without_pre_sync_for_test(
                &[
                    "checkpoint",
                    "zcode",
                    "--strict-errors",
                    "--hook-input",
                    &input,
                ],
                &[("GIT_AI_ZCODE_STORAGE_PATH", source.path.to_str().unwrap())],
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "CLI receipt failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("checkpoint_requests=1"));
    };
    send_rejected_post("failed-call");
    fs::write(
        &file,
        "// original\n// partial failed write\n// post without pre\n",
    )
    .unwrap();
    send_rejected_post("unpaired-call");
    // Family sync waits through received checkpoint admissions, including the
    // rejected receipts, before the durable queue is inspected.
    repo.sync_daemon();
    let conn = git_ai::sqlite::open_with_memory_limits(&metrics_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT external_tool_use_id, phase, state
        FROM deferred_checkpoint_jobs WHERE integration='zcode'
          AND external_session_id='zcode-session' ORDER BY id",
        )
        .unwrap();
    let jobs = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        jobs.iter().any(|(call, phase, state)| call == "failed-call"
            && phase == "failed"
            && state == "done"),
        "failure terminal checkpoint must be durable: {jobs:?}"
    );
    assert!(
        !jobs.iter().any(|(call, phase, _)| phase == "post"
            && matches!(call.as_str(), "failed-call" | "unpaired-call")),
        "neither rejected success receipt may admit an AI post: {jobs:?}"
    );
    repo.stage_all_and_commit("Keep uncertain file changes unknown")
        .unwrap();
    repo.filename("failed.ts")
        .assert_committed_lines(crate::lines![
            "// original".unattributed_human(),
            "// partial failed write".unattributed_human(),
            "// post without pre".unattributed_human()
        ]);
}

fn wait_for_usage_sources(metrics_path: &Path, expected: i64) -> Connection {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let conn = git_ai::sqlite::open_with_memory_limits(metrics_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_token_sources WHERE tool='zcode'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if count == expected {
            return conn;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for ZCode source count {expected}, got {count}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_zcode_e2e_prompt_only_usage_excludes_history_and_reads_late_same_millisecond() {
    let metrics_dir = tempfile::tempdir().unwrap();
    let metrics_path = metrics_dir.path().join("metrics.sqlite");
    MetricsDatabase::open_at_path(&metrics_path).unwrap();
    let metrics_string = metrics_path.display().to_string();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_string.as_str())]);
    let source = ZCodeFixture::new(&repo.canonical_path());
    source.usage(
        "history",
        "old-model",
        Utc::now().timestamp_millis() - 10_000,
    );
    source.hook(&repo, "SessionStart", None, None);
    let request_time = Utc::now().timestamp_millis();
    source.usage("z-request", "glm-a", request_time);
    source.hook(&repo, "UserPromptSubmit", None, None);
    let conn = wait_for_usage_sources(&metrics_path, 1);
    assert_eq!(
        conn.query_row(
            "SELECT model FROM session_token_sources WHERE tool='zcode'",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "glm-a"
    );
    drop(conn);
    source.usage("a-late-request", "glm-b", request_time);
    source.hook(&repo, "Stop", None, None);
    let conn = wait_for_usage_sources(&metrics_path, 2);
    let counts: (i64, i64, i64, i64) = conn.query_row("SELECT SUM(input_tokens),SUM(output_tokens),SUM(cache_read_tokens),SUM(cache_write_tokens)
        FROM session_token_sources WHERE tool='zcode'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
    assert_eq!(counts, (120, 40, 60, 20));
    let checkpoints: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM metrics WHERE event_kind=4",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        checkpoints, 0,
        "prompt-only usage must not require a code checkpoint"
    );
    let mut stmt = conn
        .prepare("SELECT event_json FROM metrics WHERE event_kind=9")
        .unwrap();
    let uploads = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        !uploads.is_empty(),
        "enterprise daily snapshots must be queued"
    );
    for upload in uploads {
        assert!(!upload.contains("private usage diagnostics"));
        let event: Value = serde_json::from_str(&upload).unwrap();
        assert_eq!(
            event.pointer("/a/20").and_then(Value::as_str),
            Some("zcode")
        );
    }
}

fn assert_bash_call_ended(repo: &TestRepo, call: &str) {
    use git_ai::daemon::control_api::{
        BashSessionQueryResponse, BashSnapshotQueryResponse, ControlRequest,
    };
    use git_ai::daemon::send_control_request;
    let socket = repo.daemon_control_socket_path();
    let response = send_control_request(
        &socket,
        &ControlRequest::BashSessionQuery {
            repo_work_dir: repo.canonical_path().display().to_string(),
        },
    )
    .unwrap();
    assert!(response.ok, "{:?}", response.error);
    let active: BashSessionQueryResponse = serde_json::from_value(response.data.unwrap()).unwrap();
    assert!(
        !active.active,
        "Bash call must not remain active after its terminal hook"
    );
    let response = send_control_request(
        &socket,
        &ControlRequest::BashSnapshotQuery {
            session_id: "zcode-session".to_string(),
            tool_use_id: call.to_string(),
        },
    )
    .unwrap();
    assert!(response.ok, "{:?}", response.error);
    let snapshot: BashSnapshotQueryResponse =
        serde_json::from_value(response.data.unwrap()).unwrap();
    assert!(
        !snapshot.found,
        "a completed call must not retain a reusable pre snapshot"
    );
}

fn repo_with_bash_history() -> (tempfile::TempDir, TestRepo) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bash-history.sqlite");
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH",
        path.to_str().unwrap(),
    )]);
    (directory, repo)
}

#[test]
fn test_zcode_e2e_bash_success_uses_observed_snapshot_and_exact_model() {
    let (_bash_history, mut repo) = repo_with_bash_history();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });
    let file = initial_file(&repo, "bash-success.ts");
    let source = ZCodeFixture::new(&repo.canonical_path());
    source.tool("bash-success", "bash-success-request", "glm-bash");
    let command = "printf '// Bash AI edit\n' >> bash-success.ts";
    source.bash_hook(&repo, "PreToolUse", "bash-success", command);
    fs::write(&file, "// original\n// Bash AI edit\n").unwrap();
    source.bash_hook(&repo, "PostToolUse", "bash-success", command);
    assert_bash_call_ended(&repo, "bash-success");
    let commit = repo
        .stage_all_and_commit("Record successful ZCode Bash edit")
        .unwrap();
    repo.filename("bash-success.ts")
        .assert_committed_lines(crate::lines![
            "// original".unattributed_human(),
            "// Bash AI edit".ai()
        ]);
    assert!(commit.authorship_log.metadata.sessions.values().any(|session|
        session.agent_id.tool == "zcode" && session.agent_id.model == "glm-bash"));
}

#[test]
fn test_zcode_e2e_failed_bash_partial_write_stays_unknown_beside_next_success() {
    let (_bash_history, repo) = repo_with_bash_history();
    let file = initial_file(&repo, "bash-mixed.ts");
    let source = ZCodeFixture::new(&repo.canonical_path());
    source.tool("bash-failed", "bash-failed-request", "glm-a");
    source.tool("bash-next", "bash-next-request", "glm-b");
    let failed_command = "printf '// failed partial\n' >> bash-mixed.ts; exit 1";
    source.bash_hook(&repo, "PreToolUse", "bash-failed", failed_command);
    fs::write(&file, "// original\n// failed partial\n").unwrap();
    source.bash_hook(&repo, "PostToolUseFailure", "bash-failed", failed_command);
    assert_bash_call_ended(&repo, "bash-failed");
    let success_command = "printf '// next successful AI edit\n' >> bash-mixed.ts";
    source.bash_hook(&repo, "PreToolUse", "bash-next", success_command);
    fs::write(
        &file,
        "// original\n// failed partial\n// next successful AI edit\n",
    )
    .unwrap();
    source.bash_hook(&repo, "PostToolUse", "bash-next", success_command);
    assert_bash_call_ended(&repo, "bash-next");
    repo.stage_all_and_commit("Keep failed shell output unknown beside successful edit")
        .unwrap();
    repo.filename("bash-mixed.ts")
        .assert_committed_lines(crate::lines![
            "// original".unattributed_human(),
            "// failed partial".unattributed_human(),
            "// next successful AI edit".ai()
        ]);
}

#[test]
fn test_zcode_e2e_no_changes_bash_clears_pending_for_later_known_human_edit() {
    let (_bash_history, repo) = repo_with_bash_history();
    let file = initial_file(&repo, "bash-noop.ts");
    let source = ZCodeFixture::new(&repo.canonical_path());
    source.tool("bash-noop", "bash-noop-request", "glm-a");
    fs::write(&file, "// original\n// dirty before Bash\n").unwrap();
    source.bash_hook(&repo, "PreToolUse", "bash-noop", "true");
    // No write occurs between these hooks. The pre-existing dirty path still
    // exercises the pending scope admitted by the pre-hook's Human checkpoint.
    source.bash_hook(&repo, "PostToolUse", "bash-noop", "true");
    assert_bash_call_ended(&repo, "bash-noop");
    repo.sync_daemon();
    fs::write(
        &file,
        "// original\n// dirty before Bash\n// later known human edit\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", file.to_str().unwrap()])
        .unwrap();
    repo.sync_daemon();
    assert!(
        repo.current_working_logs()
            .read_all_checkpoints()
            .unwrap()
            .iter()
            .any(|checkpoint| checkpoint.kind
                == git_ai::authorship::working_log::CheckpointKind::KnownHuman),
        "NoChanges must clear the call's pending scope so the later KnownHuman checkpoint is admitted"
    );
    repo.stage_all_and_commit("Known human edit after no-op ZCode Bash")
        .unwrap();
    repo.filename("bash-noop.ts")
        .assert_committed_lines(crate::lines![
            "// original".unattributed_human(),
            "// dirty before Bash".unattributed_human(),
            "// later known human edit".human()
        ]);
}
