use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::metrics::attrs::attr_pos;
use git_ai::metrics::db::MetricsDatabase;
use git_ai::metrics::events::committed_pos;
use git_ai::metrics::types::{MetricEvent, MetricEventId, SparseArray};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

fn isolated_metrics_db_path() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("failed to create isolated metrics db dir");
    let path = dir.path().join("metrics.db");
    (dir, path.to_string_lossy().to_string())
}

fn codex_checkpoint(
    repo: &TestRepo,
    file_path: &Path,
    session_id: &str,
    hook_event_name: &str,
    tool_use_id: &str,
) {
    let hook_input = json!({
        "session_id": session_id,
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": hook_event_name,
        "tool_name": "apply_patch",
        "tool_use_id": tool_use_id,
        "model": "gpt-5",
        "tool_input": {
            "patch": format!("*** Update File: {}\n", file_path.to_string_lossy())
        },
    })
    .to_string();

    repo.git_ai(&["checkpoint", "codex", "--hook-input", &hook_input])
        .expect("codex checkpoint should succeed");
}

fn sparse_str(values: &SparseArray, pos: usize) -> Option<&str> {
    values
        .get(&pos.to_string())
        .and_then(|value| value.as_str())
}

fn sparse_u64(values: &SparseArray, pos: usize) -> Option<u64> {
    values
        .get(&pos.to_string())
        .and_then(|value| value.as_u64())
}

fn committed_metrics_for_commit(db_path: &str, commit_sha: &str) -> Vec<MetricEvent> {
    let db = MetricsDatabase::open_at_path(Path::new(db_path))
        .expect("metrics db should open at isolated path");
    db.get_metric_history(0, None, &[MetricEventId::Committed as u16])
        .expect("metric history should load")
        .into_iter()
        .filter(|record| sparse_str(&record.event.attrs, attr_pos::COMMIT_SHA) == Some(commit_sha))
        .map(|record| record.event)
        .collect()
}

fn await_deferred_metrics(repo: &TestRepo) {
    repo.git_ai(&["await", "--timeout", "120"])
        .expect("git-ai await should drain deferred commit metrics");
}

#[test]
fn large_single_parent_commit_event_is_computed_after_durable_deferral() {
    let (_metrics_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    let file_path = repo.path().join("large.txt");
    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();

    codex_checkpoint(
        &repo,
        &file_path,
        "large-deferred-session",
        "PreToolUse",
        "large-edit",
    );
    let mut contents = String::from("base\n");
    for line in 0..6_000 {
        contents.push_str(&format!("generated line {line}\n"));
    }
    fs::write(&file_path, contents).unwrap();
    codex_checkpoint(
        &repo,
        &file_path,
        "large-deferred-session",
        "PostToolUse",
        "large-edit",
    );

    let commit = repo
        .stage_all_and_commit("large AI commit")
        .expect("large commit should complete without synchronous stats");
    await_deferred_metrics(&repo);

    let events = committed_metrics_for_commit(&metrics_db_path, &commit.commit_sha);
    assert_eq!(events.len(), 1, "deferred Event 1 must be idempotent");
    let event = &events[0];
    assert_eq!(
        sparse_u64(&event.values, committed_pos::GIT_DIFF_ADDED_LINES),
        Some(6_000)
    );
    assert_eq!(
        sparse_u64(&event.values, committed_pos::AI_ACCEPTED),
        None,
        "AI accepted is encoded as a parallel array, not a scalar"
    );
    let hunks = sparse_str(&event.values, committed_pos::HUNKS)
        .expect("formal Event 1 must contain hunk evidence");
    assert_ne!(hunks, "null");
    assert!(!hunks.is_empty());

    let mut file = repo.filename("large.txt");
    file.assert_committed_lines(
        std::iter::once("base".unattributed_human())
            .chain((0..6_000).map(|line| format!("generated line {line}").ai()))
            .collect(),
    );
}

#[test]
fn large_root_commit_uses_empty_tree_diff_and_null_base_commit() {
    let (_metrics_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    let file_path = repo.path().join("large-root.txt");
    fs::write(&file_path, "").unwrap();

    codex_checkpoint(
        &repo,
        &file_path,
        "large-root-session",
        "PreToolUse",
        "large-root-edit",
    );
    let contents = (0..6_000)
        .map(|line| format!("root generated line {line}\n"))
        .collect::<String>();
    fs::write(&file_path, contents).unwrap();
    codex_checkpoint(
        &repo,
        &file_path,
        "large-root-session",
        "PostToolUse",
        "large-root-edit",
    );

    let commit = repo
        .stage_all_and_commit("large AI root commit")
        .expect("large root commit should be durably deferred");
    await_deferred_metrics(&repo);

    let events = committed_metrics_for_commit(&metrics_db_path, &commit.commit_sha);
    assert_eq!(events.len(), 1, "root Event 1 must be emitted exactly once");
    assert_eq!(
        events[0].attrs.get(&attr_pos::BASE_COMMIT_SHA.to_string()),
        Some(&Value::Null),
        "root commits have no base commit object id"
    );
    assert_eq!(
        sparse_u64(&events[0].values, committed_pos::GIT_DIFF_ADDED_LINES),
        Some(6_000)
    );
    let hunks = sparse_str(&events[0].values, committed_pos::HUNKS)
        .expect("root Event 1 must retain formal hunk evidence");
    assert_ne!(hunks, "null");
    assert!(!hunks.is_empty());
}

#[test]
fn single_parent_ai_deletion_uses_parent_note_provenance() {
    let (_metrics_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    let file_path = repo.path().join("parent-ai.txt");
    fs::write(&file_path, "").unwrap();
    repo.stage_all_and_commit("empty base").unwrap();

    codex_checkpoint(
        &repo,
        &file_path,
        "parent-ai-session",
        "PreToolUse",
        "create-parent-lines",
    );
    fs::write(&file_path, "kept one\nAI line to delete\nkept two\n").unwrap();
    codex_checkpoint(
        &repo,
        &file_path,
        "parent-ai-session",
        "PostToolUse",
        "create-parent-lines",
    );
    let parent = repo
        .stage_all_and_commit("AI-authored parent")
        .expect("parent commit should succeed");

    codex_checkpoint(
        &repo,
        &file_path,
        "deletion-session",
        "PreToolUse",
        "delete-parent-line",
    );
    fs::write(&file_path, "kept one\nkept two\n").unwrap();
    codex_checkpoint(
        &repo,
        &file_path,
        "deletion-session",
        "PostToolUse",
        "delete-parent-line",
    );
    let child = repo
        .stage_all_and_commit("delete parent AI line")
        .expect("child commit should succeed");
    await_deferred_metrics(&repo);

    let events = committed_metrics_for_commit(&metrics_db_path, &child.commit_sha);
    assert_eq!(
        events.len(),
        1,
        "single-parent commit should emit Event 1 once"
    );
    let hunks: Vec<Value> = serde_json::from_str(
        sparse_str(&events[0].values, committed_pos::HUNKS)
            .expect("commit event needs formal hunk evidence"),
    )
    .unwrap();
    let deletion = hunks
        .iter()
        .find(|hunk| hunk.get("hunk_kind").and_then(Value::as_str) == Some("deletion"))
        .expect("deleted parent line needs hunk evidence");
    assert_eq!(
        deletion.get("commit_sha").and_then(Value::as_str),
        Some(child.commit_sha.as_str())
    );
    assert_eq!(
        deletion.get("original_commit_sha").and_then(Value::as_str),
        Some(parent.commit_sha.as_str())
    );
    assert_eq!(
        deletion.get("file_path").and_then(Value::as_str),
        Some("parent-ai.txt")
    );
    assert_eq!(deletion.get("start_line").and_then(Value::as_u64), Some(2));
    assert_eq!(deletion.get("end_line").and_then(Value::as_u64), Some(2));
    assert!(
        deletion.get("prompt_id").and_then(Value::as_str).is_some(),
        "AI-owned deletion must retain the parent prompt"
    );
    assert!(
        deletion.get("session_id").and_then(Value::as_str).is_some(),
        "AI-owned deletion must retain the parent session"
    );
    assert!(deletion.get("human_id").is_none());
    let expected_hash = format!("{:x}", Sha256::digest(b"AI line to delete"));
    assert_eq!(
        deletion.get("content_hash").and_then(Value::as_str),
        Some(expected_hash.as_str())
    );
}

#[test]
fn clean_merge_finishes_deferred_job_without_event_one() {
    let (_metrics_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["switch", "-c", "feature-clean"]).unwrap();
    fs::write(repo.path().join("feature.txt"), "feature\n").unwrap();
    repo.stage_all_and_commit("feature change").unwrap();

    repo.git(&["switch", &main_branch]).unwrap();
    fs::write(repo.path().join("main.txt"), "main\n").unwrap();
    repo.stage_all_and_commit("main change").unwrap();
    repo.git(&["merge", "--no-ff", "feature-clean", "-m", "clean merge"])
        .unwrap();
    let merge_sha = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();

    await_deferred_metrics(&repo);
    assert!(
        committed_metrics_for_commit(&metrics_db_path, &merge_sha).is_empty(),
        "clean merge must not replay the first-parent diff as Event 1"
    );
}

#[test]
fn merge_reports_only_ai_attested_novel_resolution_lines() {
    let (_metrics_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&["config", "diff.renames", "true"]).unwrap();
    let file_path = repo.path().join("conflict.txt");
    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let main_branch = repo.current_branch();

    repo.git(&["switch", "-c", "feature-conflict"]).unwrap();
    fs::write(&file_path, "feature value\n").unwrap();
    repo.stage_all_and_commit("feature value").unwrap();

    repo.git(&["switch", &main_branch]).unwrap();
    fs::write(&file_path, "main value\n").unwrap();
    repo.stage_all_and_commit("main value").unwrap();
    assert!(
        repo.git(&["merge", "feature-conflict"]).is_err(),
        "fixture must create a real conflict"
    );

    codex_checkpoint(
        &repo,
        &file_path,
        "merge-resolution-session",
        "PreToolUse",
        "resolve-conflict",
    );
    fs::write(&file_path, "novel AI resolution\n").unwrap();
    codex_checkpoint(
        &repo,
        &file_path,
        "merge-resolution-session",
        "PostToolUse",
        "resolve-conflict",
    );
    let merge = repo
        .stage_all_and_commit("resolve merge")
        .expect("resolved merge should commit");

    await_deferred_metrics(&repo);
    let events = committed_metrics_for_commit(&metrics_db_path, &merge.commit_sha);
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(
        sparse_u64(&event.values, committed_pos::GIT_DIFF_ADDED_LINES),
        Some(1)
    );
    let hunks: Vec<Value> = serde_json::from_str(
        sparse_str(&event.values, committed_pos::HUNKS).expect("merge event needs hunk evidence"),
    )
    .unwrap();
    assert_eq!(hunks.len(), 1);
    assert_eq!(
        hunks[0].get("commit_sha").and_then(Value::as_str),
        Some(merge.commit_sha.as_str())
    );
    assert_eq!(hunks[0].get("start_line").and_then(Value::as_u64), Some(1));
    assert_eq!(hunks[0].get("end_line").and_then(Value::as_u64), Some(1));
    let expected_hash = format!("{:x}", Sha256::digest(b"novel AI resolution"));
    assert_eq!(
        hunks[0].get("content_hash").and_then(Value::as_str),
        Some(expected_hash.as_str())
    );
    assert!(
        hunks[0].get("prompt_id").and_then(Value::as_str).is_some(),
        "novel merge line must also intersect an AI authorship attestation"
    );

    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(lines!["novel AI resolution".ai()]);
}
