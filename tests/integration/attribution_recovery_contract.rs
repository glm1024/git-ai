use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log::LineRange;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use std::fs;

fn file_checkpoint(repo: &TestRepo, file: &str, kind: &str) {
    file_checkpoint_with_agent(repo, file, kind, "agent-v1");
}

fn file_checkpoint_with_agent(repo: &TestRepo, file: &str, kind: &str, agent: &str) {
    let payload = if kind == "human" {
        serde_json::json!({
            "type": kind,
            "repo_working_dir": repo.path(),
            "will_edit_filepaths": [repo.path().join(file)],
        })
    } else {
        serde_json::json!({
            "type": kind,
            "repo_working_dir": repo.path(),
            "edited_filepaths": [repo.path().join(file)],
            "agent_name": agent,
            "conversation_id": "mixed-authorship-session",
            "model": "model-A",
        })
    };
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
    .unwrap();
}

#[test]
fn test_agent_v1_preserves_manual_line_adjacent_to_ai_edit() {
    let repo = TestRepo::new();
    let path = repo.path().join("mixed.txt");
    fs::write(
        &path,
        "anchor_top\nmanual_slot = original\nai_slot = original\nanchor_bottom\n",
    )
    .unwrap();
    repo.stage_all_and_commit("baseline").unwrap();
    let mut file = repo.filename("mixed.txt");
    file.assert_committed_lines(lines![
        "anchor_top".unattributed_human(),
        "manual_slot = original".unattributed_human(),
        "ai_slot = original".unattributed_human(),
        "anchor_bottom".unattributed_human(),
    ]);

    fs::write(
        &path,
        "anchor_top\nmanual_slot = HUMAN_BEFORE_AI\nai_slot = original\nanchor_bottom\n",
    )
    .unwrap();
    file_checkpoint(&repo, "mixed.txt", "human");
    fs::write(
        &path,
        "anchor_top\nmanual_slot = HUMAN_BEFORE_AI\nai_slot = MODEL_A\nanchor_bottom\n",
    )
    .unwrap();
    file_checkpoint(&repo, "mixed.txt", "ai_agent");
    repo.stage_all_and_commit("mixed edits").unwrap();
    file.assert_committed_lines(lines![
        "anchor_top".unattributed_human(),
        "manual_slot = HUMAN_BEFORE_AI".unattributed_human(),
        "ai_slot = MODEL_A".ai(),
        "anchor_bottom".unattributed_human(),
    ]);
}

#[test]
fn test_agent_v1_preexisting_manual_lines_resist_commit_marker() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("mixed.txt"), "MANUAL_BEFORE\noriginal\n").unwrap();
    fs::write(repo.path().join("manual-only.txt"), "MANUAL_ONLY\n").unwrap();
    file_checkpoint_with_agent(&repo, "manual-only.txt", "human", "claude");
    file_checkpoint_with_agent(&repo, "mixed.txt", "human", "claude");
    fs::write(repo.path().join("mixed.txt"), "MANUAL_BEFORE\nAI_EDIT\n").unwrap();
    file_checkpoint_with_agent(&repo, "mixed.txt", "ai_agent", "claude");
    repo.stage_all_and_commit("mixed edit\n\nCo-authored-by: Claude <noreply@anthropic.com>")
        .unwrap();
    repo.filename("mixed.txt")
        .assert_committed_lines(lines!["MANUAL_BEFORE".unattributed_human(), "AI_EDIT".ai(),]);
    repo.filename("manual-only.txt")
        .assert_committed_lines(lines!["MANUAL_ONLY".unattributed_human(),]);
}

#[test]
fn test_agent_v1_preserves_manual_overwrite_after_ai_checkpoint() {
    let repo = TestRepo::new();
    file_checkpoint(&repo, "overwrite.txt", "human");
    let path = repo.path().join("overwrite.txt");
    fs::write(&path, "AI_LINE\nKEEP_AI\n").unwrap();
    file_checkpoint(&repo, "overwrite.txt", "ai_agent");
    fs::write(&path, "HUMAN_OVERWRITE\nKEEP_AI\n").unwrap();
    repo.stage_all_and_commit("human overwrote first AI line")
        .unwrap();
    repo.filename("overwrite.txt")
        .assert_committed_lines(lines![
            "HUMAN_OVERWRITE".unattributed_human(),
            "KEEP_AI".ai(),
        ]);
}

#[test]
fn test_agent_v1_manual_overwrite_is_not_restored_by_commit_marker() {
    let repo = TestRepo::new();
    file_checkpoint(&repo, "overwrite.txt", "human");
    let path = repo.path().join("overwrite.txt");
    fs::write(&path, "AI_LINE\nKEEP_AI\n").unwrap();
    file_checkpoint(&repo, "overwrite.txt", "ai_agent");
    fs::write(
        &path,
        "manual replacement one\nmanual replacement two\nKEEP_AI\n",
    )
    .unwrap();
    repo.stage_all_and_commit("mixed edit\n\nCo-authored-by: Claude <noreply@anthropic.com>")
        .unwrap();
    repo.filename("overwrite.txt")
        .assert_committed_lines(lines![
            "manual replacement one".unattributed_human(),
            "manual replacement two".unattributed_human(),
            "KEEP_AI".ai(),
        ]);
}

#[test]
fn test_agent_v1_manual_line_stays_unknown_across_partial_commits() {
    let repo = TestRepo::new();
    let path = repo.path().join("partial.txt");
    fs::write(&path, "base manual\nbase ai\n").unwrap();
    repo.stage_all_and_commit("baseline").unwrap();
    repo.filename("partial.txt").assert_committed_lines(lines![
        "base manual".unattributed_human(),
        "base ai".unattributed_human(),
    ]);
    fs::write(&path, "manual edit\nbase ai\n").unwrap();
    file_checkpoint(&repo, "partial.txt", "human");
    fs::write(&path, "manual edit\nAI_EDIT\n").unwrap();
    file_checkpoint(&repo, "partial.txt", "ai_agent");
    fs::write(&path, "base manual\nAI_EDIT\n").unwrap();
    repo.git(&["add", "partial.txt"]).unwrap();
    fs::write(&path, "manual edit\nAI_EDIT\n").unwrap();
    repo.commit("only stage AI edit").unwrap();
    // assert_committed_lines uses working-tree blame; expose the committed
    // snapshot during this assertion, then restore the unstaged manual edit.
    fs::write(&path, "base manual\nAI_EDIT\n").unwrap();
    repo.filename("partial.txt")
        .assert_committed_lines(lines!["base manual".unattributed_human(), "AI_EDIT".ai(),]);
    fs::write(&path, "manual edit\nAI_EDIT\n").unwrap();
    repo.stage_all_and_commit("remaining manual edit").unwrap();
    repo.filename("partial.txt")
        .assert_committed_lines(lines!["manual edit".unattributed_human(), "AI_EDIT".ai(),]);
}

#[test]
fn test_agent_v1_overwrite_with_rename_preserves_only_matching_lines() {
    let repo = TestRepo::new();
    let old = repo.path().join("before.txt");
    fs::write(&old, "old\nkeep one\nkeep two\nkeep three\nkeep four\n").unwrap();
    repo.stage_all_and_commit("baseline").unwrap();
    repo.filename("before.txt").assert_committed_lines(lines![
        "old".unattributed_human(),
        "keep one".unattributed_human(),
        "keep two".unattributed_human(),
        "keep three".unattributed_human(),
        "keep four".unattributed_human(),
    ]);
    file_checkpoint(&repo, "before.txt", "human");
    fs::write(
        &old,
        "AI_ONE\nAI_TWO\nkeep one\nkeep two\nkeep three\nkeep four\n",
    )
    .unwrap();
    file_checkpoint(&repo, "before.txt", "ai_agent");
    fs::rename(&old, repo.path().join("after.txt")).unwrap();
    fs::write(
        repo.path().join("after.txt"),
        "MANUAL\nAI_TWO\nkeep one\nkeep two\nkeep three\nkeep four\n",
    )
    .unwrap();
    repo.stage_all_and_commit("rename with manual override")
        .unwrap();
    repo.filename("after.txt").assert_committed_lines(lines![
        "MANUAL".unattributed_human(),
        "AI_TWO".ai(),
        "keep one".unattributed_human(),
        "keep two".unattributed_human(),
        "keep three".unattributed_human(),
        "keep four".unattributed_human(),
    ]);
}

#[test]
fn test_agent_v1_decomposed_unicode_path_keeps_ai_provenance() {
    let repo = TestRepo::new();
    repo.git(&["config", "core.precomposeunicode", "false"])
        .unwrap();
    let name = "cafe\u{301}.txt";
    file_checkpoint(&repo, name, "human");
    fs::write(repo.path().join(name), "AI_CONTENT\n").unwrap();
    file_checkpoint(&repo, name, "ai_agent");
    let commit = repo
        .stage_all_and_commit("decomposed Unicode path")
        .unwrap();
    assert!(
        commit.authorship_log.attestations.iter().any(|file| {
            file.file_path == "café.txt"
                && file.entries.iter().any(|entry| {
                    entry.hash.starts_with("s_")
                        && entry.line_ranges.iter().any(|range| range.contains(1))
                })
        }),
        "the note must retain the AI line before blame resolves the NFD Git path: {:?}",
        commit.authorship_log,
    );
    repo.filename(name)
        .assert_committed_lines(lines!["AI_CONTENT".ai()]);
}

#[test]
fn test_agent_v1_crlf_conversion_does_not_restore_overwritten_line() {
    let repo = TestRepo::new();
    file_checkpoint(&repo, "eol.txt", "human");
    let path = repo.path().join("eol.txt");
    fs::write(&path, "AI_ONE\nAI_TWO\n").unwrap();
    file_checkpoint(&repo, "eol.txt", "ai_agent");
    fs::write(&path, "MANUAL\r\nAI_TWO\r\n").unwrap();
    repo.stage_all_and_commit("manual replacement and CRLF")
        .unwrap();
    repo.filename("eol.txt")
        .assert_committed_lines(lines!["MANUAL".unattributed_human(), "AI_TWO".ai(),]);
}

#[test]
fn test_agent_v1_amend_does_not_keep_ai_tokens_on_manual_replacement() {
    let repo = TestRepo::new();
    file_checkpoint(&repo, "amend.txt", "human");
    let path = repo.path().join("amend.txt");
    fs::write(&path, "const AI_VALUE = 1;\nKEEP_AI\n").unwrap();
    file_checkpoint(&repo, "amend.txt", "ai_agent");
    repo.stage_all_and_commit("initial AI edit").unwrap();
    repo.filename("amend.txt")
        .assert_committed_lines(lines!["const AI_VALUE = 1;".ai(), "KEEP_AI".ai(),]);
    fs::write(&path, "const HUMAN_VALUE = 2;\nKEEP_AI\n").unwrap();
    repo.git(&["add", "amend.txt"]).unwrap();
    repo.git(&["commit", "--amend", "-m", "manual correction"])
        .unwrap();
    repo.filename("amend.txt").assert_committed_lines(lines![
        "const HUMAN_VALUE = 2;".unattributed_human(),
        "KEEP_AI".ai(),
    ]);
}

#[test]
fn test_agent_v1_does_not_claim_unknown_additions_or_gaps() {
    let repo = TestRepo::new();
    let path = repo.path().join("gaps.txt");
    fs::write(
        &path,
        "manual before\nai slot one\n\nmanual between\nai slot two\nmanual after\n",
    )
    .unwrap();
    file_checkpoint(&repo, "gaps.txt", "human");
    fs::write(
        &path,
        "manual before\nFIRST_AI\n\nmanual between\nSECOND_AI\nmanual after\n",
    )
    .unwrap();
    file_checkpoint(&repo, "gaps.txt", "ai_agent");
    repo.stage_all_and_commit("AI edits surrounded by unknown source")
        .unwrap();
    repo.filename("gaps.txt").assert_committed_lines(lines![
        "manual before".unattributed_human(),
        "FIRST_AI".ai(),
        "".unattributed_human(),
        "manual between".unattributed_human(),
        "SECOND_AI".ai(),
        "manual after".unattributed_human(),
    ]);
}

fn known_human_attested_lines(authorship_log: &AuthorshipLog, file_path: &str) -> Vec<u32> {
    let mut lines = authorship_log
        .attestations
        .iter()
        .filter(|attestation| attestation.file_path == file_path)
        .flat_map(|attestation| &attestation.entries)
        .filter(|entry| entry.hash.starts_with("h_"))
        .flat_map(|entry| entry.line_ranges.iter().flat_map(LineRange::expand))
        .collect::<Vec<_>>();
    lines.sort_unstable();
    lines.dedup();
    lines
}

/// A KnownHuman checkpoint is positive evidence only for the content it
/// observed. Later uncheckpointed additions must remain unknown rather than
/// inheriting the Git committer's human identity.
#[test]
fn test_terminal_recovery_does_not_turn_unknown_lines_into_known_human() {
    let metrics_dir = tempfile::tempdir().unwrap();
    let bash_dir = tempfile::tempdir().unwrap();
    let metrics_path = metrics_dir.path().join("metrics.db");
    let bash_path = bash_dir.path().join("bash-history.db");
    let env = [
        (
            "GIT_AI_TEST_METRICS_DB_PATH",
            metrics_path.to_str().unwrap(),
        ),
        (
            "GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH",
            bash_path.to_str().unwrap(),
        ),
    ];
    let repo = TestRepo::new_with_daemon_env(&env);
    let file_path = repo.path().join("mixed-evidence.txt");

    fs::write(&file_path, "base\n").unwrap();
    let initial = repo.stage_all_and_commit("Initial commit").unwrap();
    assert!(
        known_human_attested_lines(&initial.authorship_log, "mixed-evidence.txt").is_empty(),
        "an entirely unobserved commit must remain unknown"
    );
    let mut file = repo.filename("mixed-evidence.txt");
    file.assert_committed_lines(lines!["base".unattributed_human()]);

    fs::write(&file_path, "base\nknown human\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "mixed-evidence.txt"])
        .unwrap();

    fs::write(
        &file_path,
        "base\nknown human\nnot observed by a checkpoint\n",
    )
    .unwrap();
    let commit = repo.stage_all_and_commit("Commit mixed evidence").unwrap();

    assert_eq!(
        known_human_attested_lines(&commit.authorship_log, "mixed-evidence.txt"),
        vec![2],
        "only the line observed by the KnownHuman checkpoint may receive h_ evidence"
    );
    file.assert_committed_lines(lines![
        "base".unattributed_human(),
        "known human".human(),
        "not observed by a checkpoint".unattributed_human(),
    ]);
}

/// A commit-tree fallback is final content, not evidence that a checkpoint
/// already observed an untracked file. Amend must retain marker recovery for it.
#[test]
fn test_agent_v1_amend_unobserved_file_retains_commit_marker_recovery() {
    let repo = TestRepo::new();
    fs::write(repo.path().join("base.txt"), "known human base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    repo.stage_all_and_commit("Known human base").unwrap();
    repo.filename("base.txt")
        .assert_committed_lines(lines!["known human base".human()]);

    fs::write(
        repo.path().join("unobserved-ai.txt"),
        "generated without a file checkpoint\n",
    )
    .unwrap();
    repo.git(&["add", "unobserved-ai.txt"]).unwrap();
    repo.git(&[
        "commit",
        "--amend",
        "-m",
        "External AI addition\n\nCo-authored-by: Claude <noreply@anthropic.com>",
    ])
    .unwrap();
    repo.filename("base.txt")
        .assert_committed_lines(lines!["known human base".human()]);
    repo.filename("unobserved-ai.txt")
        .assert_committed_lines(lines!["generated without a file checkpoint".ai()]);

    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let note = AuthorshipLog::deserialize_from_string(
        &repo.read_authorship_note(amended_sha.trim()).unwrap(),
    )
    .unwrap();
    assert_eq!(known_human_attested_lines(&note, "base.txt"), vec![1]);
    assert!(
        note.metadata
            .sessions
            .values()
            .any(|session| { session.agent_id.tool == "claude" })
    );
}

/// Checkpoint paths may be NFD while the amend commit's changed-file list is
/// NFC. Both spellings must resolve to the same observed attribution source.
#[test]
fn test_agent_v1_amend_new_decomposed_unicode_file_keeps_ai_provenance() {
    let repo = TestRepo::new();
    repo.git(&["config", "core.precomposeunicode", "false"])
        .unwrap();
    fs::write(repo.path().join("base.txt"), "known human base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    repo.stage_all_and_commit("Known human base").unwrap();
    repo.filename("base.txt")
        .assert_committed_lines(lines!["known human base".human()]);

    let raw_name = "cafe\u{301}.txt";
    file_checkpoint(&repo, raw_name, "human");
    fs::write(repo.path().join(raw_name), "AI_NFD_AMEND\n").unwrap();
    file_checkpoint(&repo, raw_name, "ai_agent");
    repo.git(&["add", raw_name]).unwrap();
    repo.git(&["commit", "--amend", "-m", "Add NFD AI file"])
        .unwrap();
    repo.filename("base.txt")
        .assert_committed_lines(lines!["known human base".human()]);
    repo.filename(raw_name)
        .assert_committed_lines(lines!["AI_NFD_AMEND".ai()]);

    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let note = AuthorshipLog::deserialize_from_string(
        &repo.read_authorship_note(amended_sha.trim()).unwrap(),
    )
    .unwrap();
    assert!(
        note.attestations.iter().any(|file| {
            file.file_path == "café.txt"
                && file.entries.iter().any(|entry| {
                    entry.hash.starts_with("s_")
                        && entry.line_ranges.iter().any(|range| range.contains(1))
                })
        }),
        "the final note retains AI evidence for the logical Unicode path"
    );
    assert_eq!(known_human_attested_lines(&note, "base.txt"), vec![1]);
}

/// An older NFC blame alias can contain an empty attribution vector while the
/// latest NFD checkpoint has AI evidence. Prefer that observed raw source.
#[test]
fn test_agent_v1_amend_existing_decomposed_manual_file_keeps_first_ai_edit() {
    let repo = TestRepo::new();
    repo.git(&["config", "core.precomposeunicode", "false"])
        .unwrap();
    let raw_name = "cafe\u{301}.txt";
    fs::write(repo.path().join("base.txt"), "known human base\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    fs::write(repo.path().join(raw_name), "manual base\n").unwrap();
    repo.stage_all_and_commit("Manual NFD base").unwrap();
    repo.filename("base.txt")
        .assert_committed_lines(lines!["known human base".human()]);
    repo.filename(raw_name)
        .assert_committed_lines(lines!["manual base".unattributed_human()]);

    file_checkpoint(&repo, raw_name, "human");
    fs::write(repo.path().join(raw_name), "manual base\nAI_NFD_APPEND\n").unwrap();
    file_checkpoint(&repo, raw_name, "ai_agent");
    repo.git(&["add", raw_name]).unwrap();
    repo.git(&["commit", "--amend", "-m", "First AI edit to NFD file"])
        .unwrap();
    repo.filename("base.txt")
        .assert_committed_lines(lines!["known human base".human()]);
    repo.filename(raw_name).assert_committed_lines(lines![
        "manual base".unattributed_human(),
        "AI_NFD_APPEND".ai(),
    ]);

    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let note = AuthorshipLog::deserialize_from_string(
        &repo.read_authorship_note(amended_sha.trim()).unwrap(),
    )
    .unwrap();
    let ai_lines = note
        .attestations
        .iter()
        .filter(|file| file.file_path == "café.txt")
        .flat_map(|file| &file.entries)
        .filter(|entry| entry.hash.starts_with("s_"))
        .flat_map(|entry| entry.line_ranges.iter().flat_map(LineRange::expand))
        .collect::<Vec<_>>();
    assert_eq!(
        ai_lines,
        vec![2],
        "the exact checkpoint owns only its appended AI line"
    );
    assert_eq!(known_human_attested_lines(&note, "base.txt"), vec![1]);
}

/// The latest observed worktree differs from the staged amend. A manual
/// replacement at observed line 1 must not protect a different committed line 1.
#[test]
fn test_agent_v1_partial_amend_recovery_protection_uses_commit_coordinates() {
    let repo = TestRepo::new();
    let file = repo.path().join("partial-amend.txt");
    file_checkpoint(&repo, "partial-amend.txt", "human");
    fs::write(&file, "AI_ORIGINAL\nAI_TAIL\n").unwrap();
    file_checkpoint(&repo, "partial-amend.txt", "ai_agent");
    let original = repo.stage_all_and_commit("Original AI commit").unwrap();
    repo.filename("partial-amend.txt")
        .assert_committed_lines(lines!["AI_ORIGINAL".ai(), "AI_TAIL".ai()]);

    let staged = "EXTERNAL_AI_LINE\nAI_ORIGINAL\nAI_TAIL\n";
    fs::write(&file, staged).unwrap();
    repo.git(&["add", "partial-amend.txt"]).unwrap();
    let unstaged = "HUMAN_REPLACEMENT\nAI_TAIL\n";
    fs::write(&file, unstaged).unwrap();
    file_checkpoint(&repo, "partial-amend.txt", "human");
    repo.git(&[
        "commit",
        "--amend",
        "-m",
        "Partial amend\n\nCo-authored-by: Claude <noreply@anthropic.com>",
    ])
    .unwrap();

    // The blame helper includes live edits, so assert the immutable committed
    // version temporarily and then restore the uncommitted manual snapshot.
    fs::write(&file, staged).unwrap();
    repo.filename("partial-amend.txt")
        .assert_committed_lines(lines![
            "EXTERNAL_AI_LINE".ai(),
            "AI_ORIGINAL".ai(),
            "AI_TAIL".ai(),
        ]);
    fs::write(&file, unstaged).unwrap();
    let amended_sha = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let note = AuthorshipLog::deserialize_from_string(
        &repo.read_authorship_note(amended_sha.trim()).unwrap(),
    )
    .unwrap();
    let repository =
        git_ai::git::repository::find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let mut cache = std::collections::HashMap::new();
    let (_, original_hash, original_prompt) = original
        .authorship_log
        .get_line_attribution(&repository, "partial-amend.txt", 1, &mut cache)
        .unwrap();
    for line in [2, 3] {
        let (_, hash, prompt) = note
            .get_line_attribution(&repository, "partial-amend.txt", line, &mut cache)
            .unwrap();
        assert_eq!(
            hash, original_hash,
            "staged original code keeps its original checkpoint trace"
        );
        assert_eq!(
            prompt.unwrap().agent_id,
            original_prompt.as_ref().unwrap().agent_id
        );
    }
    assert_eq!(original_prompt.unwrap().agent_id.model, "model-A");
    assert!(
        note.attestations
            .iter()
            .filter(|entry| entry.file_path == "partial-amend.txt")
            .flat_map(|entry| &entry.entries)
            .any(|entry| {
                entry.hash.starts_with("s_")
                    && entry.line_ranges.iter().any(|range| range.contains(1))
            }),
        "recovery of the new committed line must not be blocked by an unstaged coordinate"
    );
}
