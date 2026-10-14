use crate::repos::test_file::{ExpectedLine, ExpectedLineExt};
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use std::fs;

const FILE: &str = "eol-source.txt";

fn contents() -> Vec<String> {
    (1..=40)
        .map(|i| format!("original source line {i}"))
        .collect()
}

fn write_lines(repo: &TestRepo, lines: &[String], eol: &str) {
    fs::write(repo.path().join(FILE), format!("{}{eol}", lines.join(eol))).unwrap();
}

fn assert_lines(repo: &TestRepo, lines: &[String], ai_lines: &[usize]) {
    let expected: Vec<ExpectedLine> = lines
        .iter()
        .enumerate()
        .map(|(i, text)| {
            if ai_lines.contains(&(i + 1)) {
                text.clone().ai()
            } else {
                text.clone().unattributed_human()
            }
        })
        .collect();
    repo.filename(FILE).assert_committed_lines(expected);
}

fn ai_source(log: &AuthorshipLog, line: u32) -> Option<String> {
    ai_source_for(log, FILE, line)
}

fn ai_source_for(log: &AuthorshipLog, path: &str, line: u32) -> Option<String> {
    let file = log
        .attestations
        .iter()
        .find(|file| file.file_path == path)?;
    file.entries
        .iter()
        .find(|entry| {
            (log.metadata.prompts.contains_key(&entry.hash)
                || log
                    .metadata
                    .sessions
                    .contains_key(entry.hash.split("::").next().unwrap()))
                && entry.line_ranges.iter().any(|range| range.contains(line))
        })
        .map(|entry| entry.hash.clone())
}

fn seed(repo: &TestRepo, lines: &[String]) {
    // Keep worktree writes explicit; individual tests opt into Git text normalization.
    repo.git(&["config", "core.autocrlf", "false"]).unwrap();
    repo.git(&["config", "core.safecrlf", "false"]).unwrap();
    write_lines(repo, lines, "\n");
    repo.stage_all_and_commit("Original LF source").unwrap();
    assert_lines(repo, lines, &[]);
}

#[test]
fn eol_first_ai_checkpoint_lf_to_crlf_attributes_only_real_edit() {
    let repo = TestRepo::new();
    let mut lines = contents();
    seed(&repo, &lines);

    // No pre-edit checkpoint for this file: the first AI checkpoint must compare
    // HEAD's raw LF blob with this CRLF worktree without claiming untouched lines.
    lines[19] = "AI changes exactly one source line".to_string();
    write_lines(&repo, &lines, "\r\n");
    repo.git_ai(&["checkpoint", "mock_ai", FILE]).unwrap();
    let commit = repo
        .stage_all_and_commit("One AI edit plus CRLF conversion")
        .unwrap();
    assert_lines(&repo, &lines, &[20]);
    assert!(ai_source(&commit.authorship_log, 20).is_some());
    for line in 1..=40 {
        if line != 20 {
            assert_eq!(ai_source(&commit.authorship_log, line), None);
        }
    }
    assert_eq!(
        fs::read(repo.path().join(FILE)).unwrap(),
        format!("{}\r\n", lines.join("\r\n")).as_bytes()
    );
}

#[test]
fn eol_existing_ai_sources_survive_both_directions_and_amend() {
    let repo = TestRepo::new();
    let mut lines = contents();
    fs::write(repo.path().join(".gitattributes"), "* text eol=lf\n").unwrap();
    seed(&repo, &lines);

    lines[2] = "First AI source stays unchanged".to_string();
    write_lines(&repo, &lines, "\n");
    repo.git_ai(&["checkpoint", "mock_ai", FILE]).unwrap();
    let first = repo.stage_all_and_commit("First AI source").unwrap();
    assert_lines(&repo, &lines, &[3]);
    let first_source = ai_source(&first.authorship_log, 3).unwrap();
    let first_note = repo.read_authorship_note(&first.commit_sha).unwrap();

    lines[5] = "Second AI source changes another line".to_string();
    write_lines(&repo, &lines, "\r\n");
    repo.git_ai(&["checkpoint", "mock_ai", FILE]).unwrap();
    let second = repo
        .stage_all_and_commit("Second AI edit with LF to CRLF")
        .unwrap();
    assert_lines(&repo, &lines, &[3, 6]);
    // Ordinary notes attest additions in their own commit; Git blame inherits
    // the unchanged line 3 from the parent, whose note must remain intact.
    assert_eq!(
        repo.read_authorship_note(&first.commit_sha).unwrap(),
        first_note
    );
    assert!(
        !repo
            .git(&["show", &format!("{}:{FILE}", second.commit_sha)])
            .unwrap()
            .contains('\r')
    );
    assert_eq!(
        fs::read(repo.path().join(FILE)).unwrap(),
        format!("{}\r\n", lines.join("\r\n")).as_bytes()
    );
    let second_source = ai_source(&second.authorship_log, 6).unwrap();
    assert_ne!(first_source, second_source);

    // Real amend inheritance: reverse CRLF to LF and edit a third line, without
    // allowing either prior AI source or untouched source to become this new AI.
    lines[8] = "Third AI source changes one more line".to_string();
    write_lines(&repo, &lines, "\n");
    repo.git_ai(&["checkpoint", "mock_ai", FILE]).unwrap();
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&[
        "commit",
        "--amend",
        "-m",
        "Amended second AI edit with CRLF to LF",
    ])
    .unwrap();
    assert_lines(&repo, &lines, &[3, 6, 9]);
    let head = repo.git(&["rev-parse", "HEAD"]).unwrap();
    let amended =
        AuthorshipLog::deserialize_from_string(&repo.read_authorship_note(head.trim()).unwrap())
            .unwrap();
    assert_eq!(
        repo.read_authorship_note(&first.commit_sha).unwrap(),
        first_note
    );
    let parent_log = AuthorshipLog::deserialize_from_string(&first_note).unwrap();
    assert_eq!(
        ai_source(&parent_log, 3).as_deref(),
        Some(first_source.as_str())
    );
    assert_eq!(
        ai_source(&amended, 6).as_deref(),
        Some(second_source.as_str())
    );
    let third_source = ai_source(&amended, 9).unwrap();
    assert_ne!(third_source, first_source);
    assert_ne!(third_source, second_source);
    for line in 1..=40 {
        if ![3, 6, 9].contains(&line) {
            assert_eq!(ai_source(&amended, line), None);
        }
    }
    assert_eq!(
        fs::read(repo.path().join(FILE)).unwrap(),
        format!("{}\n", lines.join("\n")).as_bytes()
    );
}

#[test]
fn eol_renamed_file_keeps_prior_source_and_only_attributes_real_edit() {
    let repo = TestRepo::new();
    let mut lines = contents();
    fs::write(repo.path().join(".gitattributes"), "* text eol=lf\n").unwrap();
    seed(&repo, &lines);

    lines[2] = "Earlier AI source survives the rename".to_string();
    write_lines(&repo, &lines, "\n");
    repo.git_ai(&["checkpoint", "mock_ai", FILE]).unwrap();
    let first = repo
        .stage_all_and_commit("AI source before rename")
        .unwrap();
    assert_lines(&repo, &lines, &[3]);
    let first_source = ai_source(&first.authorship_log, 3).unwrap();
    let first_note = repo.read_authorship_note(&first.commit_sha).unwrap();

    // Rename before the pre-edit snapshot, matching a tool's new-path scope.
    // A checkpoint recorded under an old path then moved later exercises a
    // separate existing rename behavior, not this EOL attribution regression.
    const RENAMED: &str = "renamed source 文件.txt";
    repo.git(&["mv", FILE, RENAMED]).unwrap();
    repo.git_ai(&["checkpoint", "human", RENAMED]).unwrap();
    lines[19] = "One AI edit accompanies rename and CRLF conversion".to_string();
    fs::write(
        repo.path().join(RENAMED),
        format!("{}\r\n", lines.join("\r\n")),
    )
    .unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", RENAMED]).unwrap();
    let renamed = repo
        .stage_all_and_commit("Rename with CRLF and one real edit")
        .unwrap();
    let expected: Vec<ExpectedLine> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if [3, 20].contains(&(i + 1)) {
                line.clone().ai()
            } else {
                line.clone().unattributed_human()
            }
        })
        .collect();
    repo.filename(RENAMED).assert_committed_lines(expected);
    assert_eq!(
        repo.read_authorship_note(&first.commit_sha).unwrap(),
        first_note
    );
    let new_source = ai_source_for(&renamed.authorship_log, RENAMED, 20).unwrap();
    assert_ne!(new_source, first_source);
    for line in 1..=40 {
        if ![3, 20].contains(&line) {
            assert_eq!(ai_source_for(&renamed.authorship_log, RENAMED, line), None);
        }
    }
}

fn latest_checkpoint_source(repo: &TestRepo, line: u32) -> String {
    let checkpoints = repo.current_working_logs().read_all_checkpoints().unwrap();
    checkpoints
        .iter()
        .rev()
        .find_map(|checkpoint| {
            checkpoint
                .entries
                .iter()
                .find(|entry| entry.file == FILE)
                .and_then(|entry| {
                    entry
                        .line_attributions
                        .iter()
                        .find(|attr| attr.start_line <= line && line <= attr.end_line)
                        .map(|attr| attr.author_id.clone())
                })
        })
        .expect("checkpoint must preserve the AI line source")
}

#[test]
fn eol_multiple_checkpoints_in_one_commit_preserve_each_trace() {
    let repo = TestRepo::new();
    let mut lines = contents();
    fs::write(repo.path().join(".gitattributes"), "* text eol=lf\n").unwrap();
    seed(&repo, &lines);

    lines[2] = "First AI checkpoint edits line three".to_string();
    write_lines(&repo, &lines, "\n");
    repo.git_ai(&["checkpoint", "mock_ai", FILE]).unwrap();
    let first_source = latest_checkpoint_source(&repo, 3);

    lines[5] = "Second AI checkpoint edits line six".to_string();
    write_lines(&repo, &lines, "\r\n");
    repo.git_ai(&["checkpoint", "mock_ai", FILE]).unwrap();
    assert_eq!(latest_checkpoint_source(&repo, 3), first_source);
    let second_source = latest_checkpoint_source(&repo, 6);

    lines[8] = "Third AI checkpoint edits line nine".to_string();
    write_lines(&repo, &lines, "\n");
    repo.git_ai(&["checkpoint", "mock_ai", FILE]).unwrap();
    assert_eq!(latest_checkpoint_source(&repo, 3), first_source);
    assert_eq!(latest_checkpoint_source(&repo, 6), second_source);
    let third_source = latest_checkpoint_source(&repo, 9);
    assert_ne!(first_source, second_source);
    assert_ne!(first_source, third_source);
    assert_ne!(second_source, third_source);

    let commit = repo
        .stage_all_and_commit("Three AI checkpoints with both EOL directions")
        .unwrap();
    assert_lines(&repo, &lines, &[3, 6, 9]);
    for (line, source) in [(3, &first_source), (6, &second_source), (9, &third_source)] {
        assert_eq!(
            ai_source(&commit.authorship_log, line).as_deref(),
            Some(source.as_str())
        );
    }
    for line in 1..=40 {
        if ![3, 6, 9].contains(&line) {
            assert_eq!(ai_source(&commit.authorship_log, line), None);
        }
    }
}
