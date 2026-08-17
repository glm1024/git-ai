use std::fmt::Display;

use crate::config;

#[derive(Debug, Clone, Copy)]
pub(crate) struct CheckpointContentBudget {
    max_file_size_bytes: usize,
    max_total_size_bytes: usize,
    max_total_lines: usize,
    used_size_bytes: usize,
    used_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckpointContentBudgetError {
    FileSize {
        size_bytes: usize,
        max_bytes: usize,
    },
    TotalSize {
        size_bytes: usize,
        used_bytes: usize,
        max_bytes: usize,
    },
    TotalLines {
        line_count: usize,
        used_lines: usize,
        max_lines: usize,
    },
}

impl Display for CheckpointContentBudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileSize {
                size_bytes,
                max_bytes,
            } => write!(
                f,
                "file has {size_bytes} bytes, exceeding the per-file checkpoint limit of {max_bytes} bytes"
            ),
            Self::TotalSize {
                size_bytes,
                used_bytes,
                max_bytes,
            } => write!(
                f,
                "file has {size_bytes} bytes and would exceed the total checkpoint byte budget ({used_bytes} bytes already used, {max_bytes} bytes max)"
            ),
            Self::TotalLines {
                line_count,
                used_lines,
                max_lines,
            } => write!(
                f,
                "file has {line_count} lines and would exceed the total checkpoint line budget ({used_lines} lines already used, {max_lines} lines max)"
            ),
        }
    }
}

impl CheckpointContentBudget {
    pub(crate) fn from_config(config: &config::Config) -> Self {
        Self {
            max_file_size_bytes: config.max_checkpoint_file_size_bytes(),
            max_total_size_bytes: config.max_checkpoint_total_size_bytes(),
            max_total_lines: config.max_checkpoint_total_lines(),
            used_size_bytes: 0,
            used_lines: 0,
        }
    }

    pub(crate) fn max_file_size_bytes(&self) -> usize {
        self.max_file_size_bytes
    }

    pub(crate) fn reserve(&mut self, path: impl Display, content: &str) -> bool {
        self.try_reserve(path, content).is_ok()
    }

    pub(crate) fn try_reserve(
        &mut self,
        path: impl Display,
        content: &str,
    ) -> Result<(), CheckpointContentBudgetError> {
        let size_bytes = content.len();
        if size_bytes > self.max_file_size_bytes {
            tracing::warn!(
                "skipping file larger than max_checkpoint_file_size_bytes: {} ({} bytes)",
                path,
                size_bytes,
            );
            return Err(CheckpointContentBudgetError::FileSize {
                size_bytes,
                max_bytes: self.max_file_size_bytes,
            });
        }

        let line_count = checkpoint_content_line_count(content);
        if self.used_size_bytes.saturating_add(size_bytes) > self.max_total_size_bytes {
            tracing::warn!(
                "skipping file over max_checkpoint_total_size_bytes budget: {} ({} bytes, {} bytes already used, {} bytes max)",
                path,
                size_bytes,
                self.used_size_bytes,
                self.max_total_size_bytes,
            );
            return Err(CheckpointContentBudgetError::TotalSize {
                size_bytes,
                used_bytes: self.used_size_bytes,
                max_bytes: self.max_total_size_bytes,
            });
        }
        if self.used_lines.saturating_add(line_count) > self.max_total_lines {
            tracing::warn!(
                "skipping file over max_checkpoint_total_lines budget: {} ({} lines, {} lines already used, {} lines max)",
                path,
                line_count,
                self.used_lines,
                self.max_total_lines,
            );
            return Err(CheckpointContentBudgetError::TotalLines {
                line_count,
                used_lines: self.used_lines,
                max_lines: self.max_total_lines,
            });
        }

        self.used_size_bytes += size_bytes;
        self.used_lines += line_count;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn with_limits(
        max_file_size_bytes: usize,
        max_total_size_bytes: usize,
        max_total_lines: usize,
    ) -> Self {
        Self {
            max_file_size_bytes,
            max_total_size_bytes,
            max_total_lines,
            used_size_bytes: 0,
            used_lines: 0,
        }
    }
}

fn checkpoint_content_line_count(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    content.as_bytes().iter().filter(|&&b| b == b'\n').count()
        + usize::from(!content.as_bytes().ends_with(b"\n"))
}
