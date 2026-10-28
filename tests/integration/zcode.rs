use crate::repos::test_file::{ExpectedLine, ExpectedLineExt};
use crate::repos::test_repo::TestRepo;
use chrono::Utc;
use git_ai::metrics::db::MetricsDatabase;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct ZCodeFixture {
    _dir: std::sync::Arc<tempfile::TempDir>,
    path: PathBuf,
    conn: Connection,
    session: String,
}

impl ZCodeFixture {
    fn new(directory: &Path) -> Self {
        Self::new_session(directory, "zcode-session")
    }

    fn new_session(directory: &Path, session: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let conn = git_ai::sqlite::open_with_memory_limits(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE session(id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT);
            CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT);
            CREATE TABLE part(id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, data TEXT);
            CREATE TABLE tool_usage(session_id TEXT, tool_call_id TEXT, trace_id TEXT, turn_id TEXT,
                tool_name TEXT, status TEXT, error_type TEXT, started_at INTEGER, completed_at INTEGER, cancelled_by_user INTEGER DEFAULT 0);
            CREATE TABLE model_usage(id TEXT PRIMARY KEY, logical_request_id TEXT, session_id TEXT,
                turn_id TEXT, trace_id TEXT, assistant_message_id TEXT, parent_user_message_id TEXT,
                provider_id TEXT, model_id TEXT, status TEXT, started_at INTEGER, completed_at INTEGER,
                input_tokens INTEGER, output_tokens INTEGER, cache_read_input_tokens INTEGER,
                cache_creation_input_tokens INTEGER, raw_usage_json TEXT);").unwrap();
        conn.execute(
            "INSERT INTO session VALUES(?1, NULL, ?2)",
            params![session, directory.display().to_string()],
        )
        .unwrap();
        Self {
            _dir: std::sync::Arc::new(dir),
            path,
            conn,
            session: session.into(),
        }
    }

    fn additional_session(&self, directory: &Path, session: &str) -> Self {
        self.conn
            .execute(
                "INSERT INTO session VALUES(?1, NULL, ?2)",
                params![session, directory.display().to_string()],
            )
            .unwrap();
        Self {
            _dir: self._dir.clone(),
            path: self.path.clone(),
            conn: git_ai::sqlite::open_with_memory_limits(&self.path).unwrap(),
            session: session.into(),
        }
    }

    fn usage(&self, request: &str, model: &str, started_at: i64) {
        self.conn
            .execute(
                "INSERT INTO model_usage VALUES(?1, ?1, ?5, 'turn', 'trace',
            ?1, 'user', 'zai-plan', ?2, 'completed', ?3, ?3+1, 100, 20, 30, 10, ?4)",
                params![
                    request,
                    model,
                    started_at,
                    "private usage diagnostics must not be copied",
                    self.session
                ],
            )
            .unwrap();
    }

    fn tool(&self, call: &str, request: &str, model: &str) {
        self.usage(request, model, Utc::now().timestamp_millis() - 2_000);
        self.conn.execute("INSERT INTO message VALUES(?1, ?3, ?2)", params![request,
            json!({"role":"assistant", "modelId":model, "providerId":"zai-plan", "anchor":{"turnId":"turn"}}).to_string(), self.session]).unwrap();
        self.conn.execute("INSERT INTO part VALUES(?1, ?4, ?2, ?3)", params![format!("part-{call}"), request,
            json!({"type":"tool", "callID":call, "state":{"input":{"private":"tool input"}}}).to_string(), self.session]).unwrap();
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
        let mut input = json!({"hook_event_name":event, "sessionId":self.session, "cwd":repo.canonical_path(),
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
        let mut input = json!({"hook_event_name":event, "sessionId":self.session, "cwd":repo.canonical_path(),
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

    fn paired_edit(
        &self,
        repo: &TestRepo,
        call: &str,
        request: &str,
        model: &str,
        file: &Path,
        contents: &str,
    ) {
        self.tool(call, request, model);
        self.hook(repo, "PreToolUse", Some(call), Some(file));
        fs::write(file, contents).unwrap();
        self.hook(repo, "PostToolUse", Some(call), Some(file));
    }

    fn send_receipt_only(&self, repo: &TestRepo, input: Value) {
        let output = repo
            .git_ai_command_without_pre_sync_for_test(
                &[
                    "checkpoint",
                    "zcode",
                    "--strict-errors",
                    "--hook-input",
                    &input.to_string(),
                ],
                &[("GIT_AI_ZCODE_STORAGE_PATH", self.path.to_str().unwrap())],
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        repo.sync_daemon();
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
            expected_started_at_ns: None,
            expected_start_trace_id: None,
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

/// ExpectedLine checks content/source; JSON blame checks the exact conversation
/// and model for every AI line, including lines inherited across commits.
fn assert_zcode_lines(
    repo: &TestRepo,
    name: &str,
    expected: Vec<ExpectedLine>,
    identities: &[Option<(&str, &str)>],
) {
    assert_eq!(expected.len(), identities.len());
    repo.filename(name).assert_committed_lines(expected.clone());
    let committed = repo.git(&["show", &format!("HEAD:{name}")]).unwrap();
    let exact = expected
        .iter()
        .map(|line| line.contents.as_str())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(committed, exact, "committed content must match exactly");
    let output = repo.git_ai(&["blame", "--json", name]).unwrap();
    let blame: Value = serde_json::from_str(&output).unwrap();
    let mut actual = vec![None; expected.len()];
    for (range, author) in blame["lines"].as_object().unwrap() {
        let (start, end) = range.split_once('-').map_or_else(
            || {
                let line = range.parse::<usize>().unwrap();
                (line, line)
            },
            |(start, end)| (start.parse().unwrap(), end.parse().unwrap()),
        );
        let agent = &blame["prompts"][author.as_str().unwrap()]["agent_id"];
        assert_eq!(agent["tool"], "zcode");
        for line in start..=end {
            assert!(
                line > 0 && line <= actual.len(),
                "unexpected attributed line {line}"
            );
            assert!(
                actual[line - 1].is_none(),
                "duplicate attribution on line {line}"
            );
            actual[line - 1] = Some((
                agent["id"].as_str().unwrap(),
                agent["model"].as_str().unwrap(),
            ));
        }
    }
    assert_eq!(
        actual, identities,
        "per-line session/model mismatch: {output}"
    );
}

#[test]
fn test_zcode_complex_human_before_ai_and_human_overwrites_ai() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| patch.exclude_prompts_in_repositories = Some(vec![]));
    let file = initial_file(&repo, "mixed.txt");
    let source = ZCodeFixture::new_session(&repo.canonical_path(), "mixed-session");
    fs::write(&file, "// original\nMANUAL BEACON BEFORE\n").unwrap();
    source.paired_edit(
        &repo,
        "mix-a",
        "mix-request-a",
        "model-a",
        &file,
        "// original\nMANUAL BEACON BEFORE\nAI KEEP ALPHA\nAI REPLACE BETA\n",
    );
    // An explicit editor-origin fact after AI must take ownership of the
    // replacement; the untouched AI line retains its original conversation.
    fs::write(
        &file,
        "// original\nMANUAL BEACON BEFORE\nAI KEEP ALPHA\nHUMAN OMEGA OVERRIDE\n",
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", file.to_str().unwrap()])
        .unwrap();
    repo.stage_all_and_commit("Human overrides one ZCode line")
        .unwrap();
    assert_zcode_lines(
        &repo,
        "mixed.txt",
        crate::lines![
            "// original".unattributed_human(),
            "MANUAL BEACON BEFORE".unattributed_human(),
            "AI KEEP ALPHA".ai(),
            "HUMAN OMEGA OVERRIDE".human(),
        ],
        &[None, None, Some(("mixed-session", "model-a")), None],
    );

    source.paired_edit(
        &repo,
        "mix-b",
        "mix-request-b",
        "model-b",
        &file,
        "// original\nZCODE REPLACEMENT GAMMA\nAI KEEP ALPHA\nHUMAN OMEGA OVERRIDE\n",
    );
    repo.stage_all_and_commit("ZCode replaces earlier manual content")
        .unwrap();
    assert_zcode_lines(
        &repo,
        "mixed.txt",
        crate::lines![
            "// original".unattributed_human(),
            "ZCODE REPLACEMENT GAMMA".ai(),
            "AI KEEP ALPHA".ai(),
            "HUMAN OMEGA OVERRIDE".human(),
        ],
        &[
            None,
            Some(("mixed-session", "model-b")),
            Some(("mixed-session", "model-a")),
            None,
        ],
    );
}

#[test]
fn test_zcode_complex_alternating_sessions_delete_undo_and_rewrite() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| patch.exclude_prompts_in_repositories = Some(vec![]));
    let file = initial_file(&repo, "rounds.txt");
    let a = ZCodeFixture::new_session(&repo.canonical_path(), "conversation-a");
    let b = a.additional_session(&repo.canonical_path(), "conversation-b");
    assert_eq!(
        a.path, b.path,
        "real ZCode conversations share one SQLite source"
    );
    a.paired_edit(
        &repo,
        "round-a",
        "round-request-a",
        "a-first",
        &file,
        "// original\nOMEGA FIRST MODEL\nALPHA DURABLE\nBETA EPHEMERAL\n",
    );
    b.paired_edit(
        &repo,
        "round-b-delete",
        "round-request-b-delete",
        "b-second",
        &file,
        "// original\nOMEGA FIRST MODEL\nALPHA DURABLE\n",
    );
    b.paired_edit(
        &repo,
        "round-b-rewrite",
        "round-request-b-rewrite",
        "b-second",
        &file,
        "// original\nOMEGA FIRST MODEL\nALPHA DURABLE\nDELTA REWRITTEN\n",
    );
    // Human removes an AI line, then a later model recreates the same bytes.
    // The new observation must not resurrect the erased model's identity.
    fs::write(&file, "// original\nOMEGA FIRST MODEL\nDELTA REWRITTEN\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", file.to_str().unwrap()])
        .unwrap();
    a.paired_edit(
        &repo,
        "round-a-undo",
        "round-request-a-undo",
        "a-third",
        &file,
        "// original\nOMEGA FIRST MODEL\nALPHA DURABLE\nDELTA REWRITTEN\n",
    );
    repo.stage_all_and_commit("Alternating conversations with deletion and recreation")
        .unwrap();
    assert_zcode_lines(
        &repo,
        "rounds.txt",
        crate::lines![
            "// original".unattributed_human(),
            "OMEGA FIRST MODEL".ai(),
            "ALPHA DURABLE".ai(),
            "DELTA REWRITTEN".ai(),
        ],
        &[
            None,
            Some(("conversation-a", "a-first")),
            Some(("conversation-a", "a-third")),
            Some(("conversation-b", "b-second")),
        ],
    );
}

#[test]
fn test_zcode_complex_partial_staging_two_commits_and_amend() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| patch.exclude_prompts_in_repositories = Some(vec![]));
    let file = initial_file(&repo, "partial.txt");
    let source = ZCodeFixture::new_session(&repo.canonical_path(), "partial-conversation");
    let full = "// original\nFIRST AI CHUNK\nSECOND AI CHUNK\n";
    source.paired_edit(
        &repo,
        "partial-first",
        "partial-request-first",
        "model-original",
        &file,
        full,
    );
    // Stage a proper subset while keeping the full AI edit in the worktree.
    fs::write(&file, "// original\nFIRST AI CHUNK\n").unwrap();
    repo.git(&["add", "partial.txt"]).unwrap();
    fs::write(&file, full).unwrap();
    repo.commit("First staged chunk").unwrap();
    assert_zcode_lines(
        &repo,
        "partial.txt",
        crate::lines!["// original".unattributed_human(), "FIRST AI CHUNK".ai(),],
        &[None, Some(("partial-conversation", "model-original"))],
    );
    repo.stage_all_and_commit("Remaining staged chunk").unwrap();
    assert_zcode_lines(
        &repo,
        "partial.txt",
        crate::lines![
            "// original".unattributed_human(),
            "FIRST AI CHUNK".ai(),
            "SECOND AI CHUNK".ai(),
        ],
        &[
            None,
            Some(("partial-conversation", "model-original")),
            Some(("partial-conversation", "model-original")),
        ],
    );
    source.paired_edit(
        &repo,
        "partial-amend",
        "partial-request-amend",
        "model-amend",
        &file,
        "// original\nFIRST AI CHUNK\nSECOND AI CHUNK\nAMENDED ZCODE TAIL\n",
    );
    repo.git(&["add", "partial.txt"]).unwrap();
    repo.git(&[
        "commit",
        "--amend",
        "-m",
        "Amend second chunk with a new model",
    ])
    .unwrap();
    assert_zcode_lines(
        &repo,
        "partial.txt",
        crate::lines![
            "// original".unattributed_human(),
            "FIRST AI CHUNK".ai(),
            "SECOND AI CHUNK".ai(),
            "AMENDED ZCODE TAIL".ai(),
        ],
        &[
            None,
            Some(("partial-conversation", "model-original")),
            Some(("partial-conversation", "model-original")),
            Some(("partial-conversation", "model-amend")),
        ],
    );
}

#[test]
fn test_zcode_complex_cancel_restart_and_duplicate_hooks_are_safe() {
    let metrics_dir = tempfile::tempdir().unwrap();
    let metrics_path = metrics_dir.path().join("metrics.sqlite");
    let env = [(
        "GIT_AI_TEST_METRICS_DB_PATH",
        metrics_path.to_str().unwrap(),
    )];
    let mut repo = TestRepo::new_with_daemon_env(&env);
    repo.patch_git_ai_config(|patch| patch.exclude_prompts_in_repositories = Some(vec![]));
    let file = initial_file(&repo, "recovery.txt");
    let source = ZCodeFixture::new_session(&repo.canonical_path(), "recovery-session");
    source.tool("cancelled", "cancel-request", "cancel-model");
    source.hook(&repo, "PreToolUse", Some("cancelled"), Some(&file));
    fs::write(&file, "// original\nUNCERTAIN CANCELLED FRAGMENT\n").unwrap();
    source
        .submit_input(
            &repo,
            json!({"hook_event_name":"PostToolUseFailure", "sessionId":source.session,
        "cwd":repo.canonical_path(), "traceId":"trace", "turnId":"turn", "toolCallId":"cancelled",
        "toolName":"Edit", "toolInput":{"file_path":file},
        "toolOutput":{"status":"cancelled", "isError":true}}),
        )
        .unwrap();
    source.tool("resumed", "resumed-request", "resumed-model");
    source.hook(&repo, "PreToolUse", Some("resumed"), Some(&file));
    repo.sync_daemon();
    repo.restart_dedicated_daemon_with_env_for_test(&env);
    let input = |event: &str| {
        json!({"hook_event_name":event,"sessionId":source.session,
        "cwd":repo.canonical_path(),"traceId":"trace","turnId":"turn","toolCallId":"resumed",
        "toolName":"Edit","toolInput":{"file_path":file}})
    };
    source.send_receipt_only(&repo, input("PreToolUse"));
    fs::write(
        &file,
        "// original\nUNCERTAIN CANCELLED FRAGMENT\nRECOVERED AI SUCCESS\n",
    )
    .unwrap();
    source.hook(&repo, "PostToolUse", Some("resumed"), Some(&file));
    source.send_receipt_only(&repo, input("PostToolUse"));
    source.send_receipt_only(&repo, input("PostToolUse"));
    repo.stage_all_and_commit("Recovery and duplicate receipts preserve exact provenance")
        .unwrap();
    assert_zcode_lines(
        &repo,
        "recovery.txt",
        crate::lines![
            "// original".unattributed_human(),
            "UNCERTAIN CANCELLED FRAGMENT".unattributed_human(),
            "RECOVERED AI SUCCESS".ai(),
        ],
        &[None, None, Some(("recovery-session", "resumed-model"))],
    );
    let conn = git_ai::sqlite::open_with_memory_limits(&metrics_path).unwrap();
    let rows: Vec<(String, i64)> = conn
        .prepare(
            "SELECT phase,COUNT(*) FROM deferred_checkpoint_jobs
        WHERE integration='zcode' AND external_session_id='recovery-session'
          AND external_tool_use_id='resumed' GROUP BY phase ORDER BY phase",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![("post".into(), 1), ("pre".into(), 1)],
        "retries must reuse original jobs"
    );
}

#[test]
fn test_zcode_permission_denial_without_failure_hook_clears_exact_pre_and_allows_next_bash() {
    let (_history, mut repo) = repo_with_bash_history();
    repo.patch_git_ai_config(|patch| patch.exclude_prompts_in_repositories = Some(vec![]));
    let file = initial_file(&repo, "permission.txt");
    let source = ZCodeFixture::new(&repo.canonical_path());
    let denied_command = "printf should-not-run >> permission.txt";
    fs::write(&file, "// original\nDIRTY BEFORE DENIAL\n").unwrap();
    for (index, boundary) in [(1, Some("Stop")), (2, None)] {
        let call = format!("denied-{index}");
        source.tool(&call, &format!("denied-request-{index}"), "denied-model");
        source.bash_hook(&repo, "PreToolUse", &call, denied_command);
        // Real ZCode emits PreToolUse before permission UI. A denial records
        // this authoritative metadata but returns before PostToolUseFailure.
        let now = Utc::now().timestamp_millis();
        let history =
            git_ai::sqlite::open_with_memory_limits(_history.path().join("bash-history.sqlite"))
                .unwrap();
        let pre_ns: i64 = history.query_row("SELECT start_time_ns FROM bash_checkpoint_calls WHERE session_id='zcode-session' AND tool_use_id=?1 ORDER BY id DESC LIMIT 1",[&call],|row|row.get(0)).unwrap();
        source
            .conn
            .execute(
                "INSERT INTO tool_usage VALUES('zcode-session',?1,'trace','turn',
            'Bash','error','permission_denied',?2,?3,0)",
                params![call, pre_ns / 1_000_000 - 1, now],
            )
            .unwrap();
        if let Some(boundary) = boundary {
            source.hook(&repo, boundary, None, None);
            assert_bash_call_ended(&repo, &call);
            source.hook(&repo, boundary, None, None); // repeated Stop is harmless
            assert_bash_call_ended(&repo, &call);
            fs::write(
                &file,
                "// original\nDIRTY BEFORE DENIAL\nKNOWN HUMAN AFTER STOP\n",
            )
            .unwrap();
            repo.git_ai(&["checkpoint", "mock_known_human", file.to_str().unwrap()])
                .unwrap();
        }
    }
    source.tool("next-success", "next-success-request", "successful-model");
    source.bash_hook(
        &repo,
        "PreToolUse",
        "next-success",
        "printf AI >> permission.txt",
    );
    fs::write(
        &file,
        "// original\nDIRTY BEFORE DENIAL\nKNOWN HUMAN AFTER STOP\nNEXT BASH AI SUCCESS\n",
    )
    .unwrap();
    source.bash_hook(
        &repo,
        "PostToolUse",
        "next-success",
        "printf AI >> permission.txt",
    );
    // The next genuine PreToolUse must also clear a denied call when Stop was
    // absent, while preserving the fresh call's own snapshot and identity.
    assert_bash_call_ended(&repo, "denied-2");
    assert_bash_call_ended(&repo, "next-success");
    repo.stage_all_and_commit("Permission denial adds no AI and leaves no pending scope")
        .unwrap();
    assert_zcode_lines(
        &repo,
        "permission.txt",
        crate::lines![
            "// original".unattributed_human(),
            "DIRTY BEFORE DENIAL".unattributed_human(),
            "KNOWN HUMAN AFTER STOP".human(),
            "NEXT BASH AI SUCCESS".ai(),
        ],
        &[
            None,
            None,
            None,
            Some(("zcode-session", "successful-model")),
        ],
    );
}

#[test]
fn test_zcode_cancelled_without_failure_hook_live_and_restart_never_infers_ai_from_tokens() {
    for restart in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let metrics = dir.path().join("metrics.sqlite");
        let history = dir.path().join("bash.sqlite");
        let env = [
            ("GIT_AI_TEST_METRICS_DB_PATH", metrics.to_str().unwrap()),
            (
                "GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH",
                history.to_str().unwrap(),
            ),
        ];
        let mut repo = TestRepo::new_with_daemon_env(&env);
        repo.patch_git_ai_config(|patch| patch.exclude_prompts_in_repositories = Some(vec![]));
        let file = initial_file(&repo, "cancel.txt");
        let source = ZCodeFixture::new(&repo.canonical_path());
        source.tool("cancelled-no-hook", "cancelled-request", "cancel-model");
        source.bash_hook(
            &repo,
            "PreToolUse",
            "cancelled-no-hook",
            "printf partial >> cancel.txt; sleep 60",
        );
        let conn = git_ai::sqlite::open_with_memory_limits(&history).unwrap();
        let pre_ns: i64 = conn.query_row("SELECT start_time_ns FROM bash_checkpoint_calls WHERE session_id='zcode-session' AND tool_use_id='cancelled-no-hook' ORDER BY id DESC LIMIT 1",[],|row|row.get(0)).unwrap();
        fs::write(&file, "// original\nCANCELLED PARTIAL MUST STAY UNKNOWN\n").unwrap();
        let now = Utc::now().timestamp_millis();
        source
            .conn
            .execute(
                "INSERT INTO tool_usage VALUES('zcode-session','cancelled-no-hook','trace','turn',
            'Bash','error','tool_cancelled',?1,?2,1)",
                params![pre_ns / 1_000_000 - 1, now],
            )
            .unwrap();
        if restart {
            repo.sync_daemon();
            repo.restart_dedicated_daemon_with_env_for_test(&env);
        }
        source.usage(
            "nearby-token-request",
            "adjacent-model",
            Utc::now().timestamp_millis(),
        );
        if !restart {
            conn.execute_batch("CREATE TRIGGER reject_terminal BEFORE UPDATE OF end_time_ns ON bash_checkpoint_calls
                WHEN NEW.tool_use_id='cancelled-no-hook' AND NEW.end_time_ns IS NOT NULL
                BEGIN SELECT RAISE(ABORT,'fixture terminal persistence unavailable'); END;").unwrap();
            assert!(
                source
                    .try_hook(&repo, "UserPromptSubmit", None, None)
                    .is_err()
            );
            use git_ai::daemon::control_api::{BashSnapshotQueryResponse, ControlRequest};
            let response = git_ai::daemon::send_control_request(
                &repo.daemon_control_socket_path(),
                &ControlRequest::BashSnapshotQuery {
                    session_id: "zcode-session".into(),
                    tool_use_id: "cancelled-no-hook".into(),
                    expected_started_at_ns: Some(pre_ns as u128),
                    expected_start_trace_id: None,
                },
            )
            .unwrap();
            let snapshot: BashSnapshotQueryResponse =
                serde_json::from_value(response.data.unwrap()).unwrap();
            assert!(
                snapshot.found,
                "failed terminal persistence must retain the frozen pre for retry"
            );
            conn.execute_batch("DROP TRIGGER reject_terminal;").unwrap();
        }
        source.hook(&repo, "UserPromptSubmit", None, None);
        assert_bash_call_ended(&repo, "cancelled-no-hook");
        source.hook(&repo, "Stop", None, None);
        // Ensure the adjacent token metadata is actually in the recovery store,
        // so a false AI result cannot be hidden by a stream-worker race.
        wait_for_usage_sources(&metrics, 1);
        repo.sync_daemon();
        let evidence = git_ai::sqlite::open_with_memory_limits(&metrics).unwrap();
        let failed_jobs: i64 = evidence
            .query_row(
                "SELECT COUNT(*) FROM deferred_checkpoint_jobs
            WHERE external_session_id='zcode-session' AND external_tool_use_id='cancelled-no-hook'
              AND phase='failed' AND state='done'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let unknown_deltas: i64 = evidence
            .query_row(
                "SELECT COUNT(*) FROM metrics WHERE event_kind=4
            AND json_extract(event_json,'$.v.\"1\"')='human'
            AND json_extract(event_json,'$.v.\"7\"')='cancelled-no-hook'
            AND json_extract(event_json,'$.v.\"3\"')=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let diagnostic: Vec<(String,String)> = evidence.prepare("SELECT phase,state FROM deferred_checkpoint_jobs WHERE external_session_id='zcode-session' AND external_tool_use_id='cancelled-no-hook' ORDER BY id").unwrap().query_map([],|row|Ok((row.get(0)?,row.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(
            failed_jobs,
            if restart { 0 } else { 1 },
            "only a live frozen snapshot may create a failed delta job: {diagnostic:?}"
        );
        assert_eq!(
            unknown_deltas,
            if restart { 0 } else { 1 },
            "live failure captures exactly one Unknown line; restart must not invent a delta"
        );
        repo.stage_all_and_commit("Cancelled bytes remain unknown beside a genuine model request")
            .unwrap();
        assert_zcode_lines(
            &repo,
            "cancel.txt",
            crate::lines![
                "// original".unattributed_human(),
                "CANCELLED PARTIAL MUST STAY UNKNOWN".unattributed_human(),
            ],
            &[None, None],
        );
        let closed: i64 = conn.query_row("SELECT COUNT(*) FROM bash_checkpoint_calls WHERE session_id='zcode-session'
            AND tool_use_id='cancelled-no-hook' AND end_time_ns IS NOT NULL AND json_extract(metadata_json,'$.tool_outcome')='failed'",[],|row|row.get(0)).unwrap();
        assert_eq!(
            closed, 1,
            "one persistent terminal across repeated boundaries, restart={restart}"
        );
        let metrics_conn = git_ai::sqlite::open_with_memory_limits(&metrics).unwrap();
        let ai_facts: i64 = metrics_conn.query_row("SELECT COUNT(*) FROM metrics WHERE event_kind=4 AND json_extract(event_json,'$.v.\"1\"')='ai_agent'",[],|row|row.get(0)).unwrap();
        assert_eq!(
            ai_facts, 0,
            "cancelled calls must never emit successful AI facts"
        );
    }
}
