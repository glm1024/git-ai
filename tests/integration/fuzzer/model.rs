use std::collections::HashMap;
use std::fmt;
use std::fs;

use crate::repos::test_repo::TestRepo;

use super::helpers::{BlameClass, classify_show_prompt_author, parse_blame_line};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineAttribution {
    Ai,
    KnownHuman,
    Untracked,
}

impl fmt::Display for LineAttribution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LineAttribution::Ai => write!(f, "Ai"),
            LineAttribution::KnownHuman => write!(f, "KnownHuman"),
            LineAttribution::Untracked => write!(f, "Untracked"),
        }
    }
}

impl LineAttribution {
    fn expected_blame_class(self) -> BlameClass {
        match self {
            LineAttribution::Ai => BlameClass::Ai,
            LineAttribution::KnownHuman => BlameClass::KnownHuman,
            LineAttribution::Untracked => BlameClass::Untracked,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttrRecord {
    pub attr: LineAttribution,
    pub ai_session: Option<u64>,
}

impl AttrRecord {
    pub fn new(attr: LineAttribution) -> Self {
        Self {
            attr,
            ai_session: None,
        }
    }

    pub fn ai(session: u64) -> Self {
        Self {
            attr: LineAttribution::Ai,
            ai_session: Some(session),
        }
    }
}

/// Global registry: maps each unique char to its CHECKPOINT-TIME attribution.
/// This never forgets — once a char is registered, its original attribution is preserved.
/// Reconciliation can downgrade it to Untracked in the FileModel, but the registry
/// always remembers what was checkpointed.
#[derive(Debug, Clone)]
pub struct AttrRegistry {
    map: HashMap<char, AttrRecord>,
    next_ai_session: u64,
}

impl AttrRegistry {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            next_ai_session: 1,
        }
    }

    pub fn register(&mut self, ch: char, attr: LineAttribution) {
        self.map.insert(ch, AttrRecord::new(attr));
    }

    pub fn register_record(&mut self, ch: char, record: AttrRecord) {
        self.map.insert(ch, record);
    }

    pub fn get(&self, ch: char) -> LineAttribution {
        self.get_record(ch).attr
    }

    pub fn get_record(&self, ch: char) -> AttrRecord {
        self.map
            .get(&ch)
            .copied()
            .unwrap_or_else(|| AttrRecord::new(LineAttribution::Untracked))
    }

    pub fn allocate_ai_session(&mut self) -> u64 {
        let session = self.next_ai_session;
        self.next_ai_session += 1;
        session
    }
}

/// The current state of a file as the fuzzer understands it.
/// `lines` contains one char per line — the char identifies the line uniquely.
/// Attribution is looked up from the AttrRegistry + reconciliation state.
#[derive(Debug, Clone)]
pub struct FileModel {
    pub filename: String,
    pub lines: Vec<char>,
    /// Per-line attribution predicted by the model. This is what we assert against.
    /// Reconciliation must not inspect git-ai's actual notes; missing notes are
    /// implementation failures, not new expected behavior.
    pub resolved_attrs: Vec<LineAttribution>,
    pub resolved_ai_sessions: Vec<Option<u64>>,
    pending_attestations: HashMap<char, AttrRecord>,
}

impl FileModel {
    pub fn new(filename: &str) -> Self {
        Self {
            filename: filename.to_string(),
            lines: Vec::new(),
            resolved_attrs: Vec::new(),
            resolved_ai_sessions: Vec::new(),
            pending_attestations: HashMap::new(),
        }
    }

    pub fn write_to_disk(&self, repo: &TestRepo) {
        let content: String = self.lines.iter().map(|ch| format!("{}\n", ch)).collect();
        fs::write(repo.path().join(&self.filename), content).unwrap();
    }

    /// Re-read file content from disk. Updates `lines` to match what's on disk.
    /// Then rebuilds `resolved_attrs` from the registry (before reconciliation).
    pub fn sync_from_disk(&mut self, repo: &TestRepo, registry: &AttrRegistry) {
        let path = repo.path().join(&self.filename);
        if !path.exists() {
            self.lines.clear();
            self.resolved_attrs.clear();
            self.resolved_ai_sessions.clear();
            return;
        }
        let content = fs::read_to_string(&path).unwrap();
        self.lines = content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.chars().next().unwrap_or('?'))
            .collect();
        let records = self
            .lines
            .iter()
            .map(|&ch| registry.get_record(ch))
            .collect::<Vec<_>>();
        self.resolved_attrs = records.iter().map(|record| record.attr).collect();
        self.resolved_ai_sessions = records.iter().map(|record| record.ai_session).collect();
    }

    /// Reconcile hook retained for operation flow symmetry. The model is the
    /// oracle, so this intentionally does not read git blame or authorship notes.
    pub fn reconcile(&mut self, _repo: &TestRepo) {
        let records = self
            .lines
            .iter()
            .map(|&ch| self.resolved_record(ch))
            .collect::<Vec<_>>();
        self.resolved_attrs = records.iter().map(|record| record.attr).collect();
        self.resolved_ai_sessions = records.iter().map(|record| record.ai_session).collect();
    }

    fn resolved_record(&self, ch: char) -> AttrRecord {
        self.lines
            .iter()
            .enumerate()
            .find_map(|(idx, &candidate)| {
                (candidate == ch).then_some(AttrRecord {
                    attr: self.resolved_attrs[idx],
                    ai_session: self.resolved_ai_sessions[idx],
                })
            })
            .unwrap_or_else(|| AttrRecord::new(LineAttribution::Untracked))
    }

    /// Preserve unknown provenance: neighboring AI lines are not evidence for
    /// added lines. Keep the operation hook for commit/rewrite flow symmetry.
    pub fn apply_edge_recovery_for_added_lines(
        &mut self,
        _registry: &mut AttrRegistry,
        _added_lines: &[u32],
    ) {
    }

    /// An untracked edit stays unknown after commit. The Git author and other
    /// KnownHuman lines cannot establish positive human provenance for it.
    /// Keep the hook for operation symmetry without reading actual blame.
    pub fn apply_terminal_human_recovery_for_added_lines(
        &mut self,
        _registry: &mut AttrRegistry,
        _added_lines: &[u32],
    ) {
    }

    pub fn mark_pending_attestation(&mut self, ch: char, record: AttrRecord) {
        self.pending_attestations.insert(ch, record);
    }

    pub fn clear_pending_attestations(&mut self) {
        self.pending_attestations.clear();
    }

    /// Assert that git-ai blame output matches our model EXACTLY.
    /// Every line. Every time. No exceptions.
    pub fn assert_blame(&self, repo: &TestRepo, op_log: &[String], seed: u64) {
        let path = repo.path().join(&self.filename);
        if !path.exists() || self.lines.is_empty() {
            return;
        }

        // --show-prompt surfaces all three attribution classes in the author
        // column: agent tool name for AI, h_-prefixed hash for known-human, plain
        // git author for untracked. Plain blame collapses the latter two.
        let blame_output = match repo.git_ai(&["blame", "--show-prompt", &self.filename]) {
            Ok(output) => output,
            Err(e) => {
                panic!(
                    "git-ai blame failed for '{}'\nSeed: {}\nError: {}\nOp log:\n{}\nModel:\n{}",
                    self.filename,
                    seed,
                    e,
                    op_log.join("\n"),
                    self.dump()
                );
            }
        };

        let blame_lines: Vec<&str> = blame_output
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();

        if blame_lines.len() != self.lines.len() {
            panic!(
                "Line count mismatch for '{}'\nSeed: {}\n\
                 Blame lines: {}\nModel lines: {}\n\
                 Op log:\n{}\nModel:\n{}",
                self.filename,
                seed,
                blame_lines.len(),
                self.lines.len(),
                op_log.join("\n"),
                self.dump()
            );
        }

        for (i, (blame_line, &expected_attr)) in
            blame_lines.iter().zip(&self.resolved_attrs).enumerate()
        {
            let line_num = i + 1;
            let (author, _content) = parse_blame_line(blame_line);
            let actual_class = classify_show_prompt_author(&author);
            let expected_class = expected_attr.expected_blame_class();

            if expected_class != actual_class {
                panic!(
                    "Attribution mismatch on line {} of '{}'\n\
                     Seed: {}\n\
                     Char: '{}'\n\
                     Model says: {:?} (expected class {:?})\n\
                     Blame shows: author='{}' (actual class {:?})\n\
                     Blame line: {}\n\
                     Full blame:\n{}\n\
                     Op log:\n{}\n\
                     Model:\n{}",
                    line_num,
                    self.filename,
                    seed,
                    self.lines[i],
                    expected_attr,
                    expected_class,
                    author,
                    actual_class,
                    blame_line,
                    blame_output,
                    op_log.join("\n"),
                    self.dump()
                );
            }
        }
    }

    pub fn dump(&self) -> String {
        let mut out = format!("File: {} ({} lines)\n", self.filename, self.lines.len());
        for (i, (&ch, &attr)) in self.lines.iter().zip(&self.resolved_attrs).enumerate() {
            let session = self.resolved_ai_sessions[i]
                .map(|session| format!(" #{session}"))
                .unwrap_or_default();
            out.push_str(&format!("  L{}: '{}' -> {}{}\n", i + 1, ch, attr, session));
        }
        out
    }
}
