use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log::LineRange;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use std::fs;

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

    fs::write(&file_path, "base\nknown human\nnot observed by a checkpoint\n").unwrap();
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
