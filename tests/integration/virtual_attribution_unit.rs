use crate::repos::test_repo::TestRepo;
use git_ai::authorship::virtual_attribution::VirtualAttributions;
use git_ai::git::repository::find_repository_in_path;

/// Exercise both commit serializers against real Git hunks, isolating them from
/// later recovery (which must supply its own provenance and metrics).
fn assert_commit_conversion_preserves_evidence(content: &str, evidenced_lines: &[u32]) {
    use crate::repos::test_file::ExpectedLineExt;
    use git_ai::authorship::attribution_tracker::LineAttribution;
    use std::collections::HashMap;

    let repo = TestRepo::new();
    repo.git_og(&["commit", "--allow-empty", "-m", "Empty fixture base"])
        .unwrap();
    let parent = repo.git_og(&["rev-parse", "HEAD"]).unwrap();
    let path = "evidence.md";
    std::fs::write(repo.path().join(path), content).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", path])
        .unwrap();
    let commit = repo.stage_all_and_commit("Fixture content").unwrap();
    repo.filename(path)
        .assert_committed_lines(content.lines().map(|line| line.human()).collect::<Vec<_>>());

    let author = "s_evidence::t_edit";
    let repository = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();
    let attribution = VirtualAttributions::new(
        repository.clone(),
        parent.trim().to_string(),
        HashMap::from([(
            path.to_string(),
            (
                vec![],
                evidenced_lines
                    .iter()
                    .map(|&line| LineAttribution::new(line, line, author.to_string(), None))
                    .collect(),
            ),
        )]),
        HashMap::from([(path.to_string(), content.to_string())]),
        0,
    );
    let (normal, carryover, _) = attribution
        .to_authorship_log_and_initial_working_log(
            &repository,
            parent.trim(),
            &commit.commit_sha,
            None,
            None,
        )
        .unwrap();
    let index_only = attribution
        .to_authorship_log_index_only(&repository, parent.trim(), &commit.commit_sha, None)
        .unwrap();
    assert!(carryover.files.is_empty());
    for (mode, note) in [("normal", normal), ("index-only", index_only)] {
        let mut actual: Vec<u32> = note
            .attestations
            .iter()
            .flat_map(|file| &file.entries)
            .filter(|entry| entry.hash == author)
            .flat_map(|entry| &entry.line_ranges)
            .flat_map(|range| range.expand())
            .collect();
        actual.sort_unstable();
        assert_eq!(
            actual.len(),
            evidenced_lines.len(),
            "{mode}: must not mint AI evidence"
        );
        assert_eq!(
            actual, evidenced_lines,
            "{mode}: preserve exact line identity"
        );
    }
}

#[test]
fn test_commit_conversion_does_not_expand_88_lines_to_1317() {
    let content: String = (1..=1317)
        .map(|line| format!("| field_{line} | String |\n"))
        .collect();
    let evidence: Vec<u32> = (1..=44).chain(1274..=1317).collect();
    assert_commit_conversion_preserves_evidence(&content, &evidence);
}

#[test]
fn test_commit_conversion_does_not_claim_duplicate_content() {
    assert_commit_conversion_preserves_evidence(
        "| type | String |\nother\n| type | String |\n",
        &[1],
    );
}

#[test]
fn test_commit_conversion_does_not_infer_blank_line_provenance() {
    assert_commit_conversion_preserves_evidence("AI before\n\nAI after\n", &[1, 3]);
}

#[test]
fn test_commit_conversion_preserves_evidenced_blank_and_duplicate_lines() {
    assert_commit_conversion_preserves_evidence("same\n\nsame\n", &[1, 2, 3]);
}

#[test]
fn test_virtual_attributions() {
    // Create a temporary repo with an initial commit
    let repo = TestRepo::new();

    // Write a test file with some content
    std::fs::write(
        repo.path().join("test_file.rs"),
        "fn main() {\n    println!(\"Hello\");\n}\n",
    )
    .unwrap();
    repo.git_og(&["add", "test_file.rs"]).unwrap();

    // Trigger checkpoint and commit to create proper authorship data
    repo.git_ai(&["checkpoint", "mock_known_human", "test_file.rs"])
        .unwrap();
    repo.stage_all_and_commit("Initial commit").unwrap();

    // Get the commit SHA
    let commit_sha = repo
        .git_og(&["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_string();

    // Get gitai repo handle
    let gitai_repo = find_repository_in_path(repo.path().to_str().unwrap()).unwrap();

    // Create VirtualAttributions using the temp repo
    let virtual_attributions = git_ai::tokio_runtime::block_on(async {
        VirtualAttributions::new_for_base_commit(
            gitai_repo.clone(),
            commit_sha.clone(),
            &["test_file.rs".to_string()],
            None,
        )
        .await
    })
    .unwrap();

    // Verify files were tracked
    println!(
        "virtual_attributions files: {:?}",
        virtual_attributions.files()
    );
    println!("base_commit: {}", virtual_attributions.base_commit());
    println!("timestamp: {}", virtual_attributions.timestamp());

    // Print attribution details if available (for debugging)
    if let Some((char_attrs, line_attrs)) = virtual_attributions.get_attributions("test_file.rs") {
        println!("\n=== test_file.rs Attribution Info ===");
        println!("Character-level attributions: {} ranges", char_attrs.len());
        println!("Line-level attributions: {} ranges", line_attrs.len());
    }

    assert!(!virtual_attributions.files().is_empty());
}

crate::reuse_tests_in_worktree!(test_virtual_attributions,);
