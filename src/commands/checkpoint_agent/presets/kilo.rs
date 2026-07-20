//! Kilo v7 checkpoint preset.
//!
//! Kilo v7 persists sessions in an OpenCode-compatible SQLite schema, but uses
//! its own data root, database name, agent identity, and IDE/runtime metadata.
//! Keeping this as a separate preset avoids teaching the upstream OpenCode
//! adapter about fork-specific paths and makes future upstream rebases additive.

use super::{
    AgentPreset, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    PresetContext, StreamFormat, StreamSource,
};
use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::presets::opencode::OpenCodePreset;
use crate::error::GitAiError;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const TOOL_NAME: &str = "kilo";
const DEFAULT_DB_NAME: &str = "kilo.db";

pub struct KiloPreset;

#[derive(Debug, Deserialize)]
struct KiloHookInput {
    hook_event_name: String,
    session_id: String,
    cwd: String,
    tool_input: Option<serde_json::Value>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default, alias = "toolUseId")]
    tool_use_id: Option<String>,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    client: Option<String>,
    #[serde(default)]
    editor_name: Option<String>,
    #[serde(default)]
    database_path: Option<String>,
}

#[derive(Debug, Default)]
struct SessionMetadata {
    parent_id: Option<String>,
    version: Option<String>,
}

impl KiloPreset {
    fn resolve_stream_source(
        session_id: &str,
        database_path: Option<&str>,
    ) -> Option<(StreamSource, SessionMetadata)> {
        let data_path = if let Ok(test_path) = std::env::var("GIT_AI_KILO_STORAGE_PATH") {
            PathBuf::from(test_path)
        } else {
            Self::kilo_data_path().ok()?
        };

        let db_path = Self::resolve_sqlite_db_path(&data_path, database_path, session_id)?;
        let session = Self::lookup_session_metadata(&db_path, session_id);
        Some((
            StreamSource {
                path: db_path,
                format: StreamFormat::OpenCodeSqlite,
                session_id: generate_session_id(session_id, TOOL_NAME),
                external_session_id: session_id.to_string(),
                external_parent_session_id: session.parent_id.clone(),
            },
            session,
        ))
    }

    fn lookup_session_metadata(db_path: &Path, session_id: &str) -> SessionMetadata {
        let Ok(conn) = crate::streams::agents::opencode::open_sqlite_readonly(db_path) else {
            return SessionMetadata::default();
        };

        conn.query_row(
            "SELECT parent_id, version FROM session WHERE id = ?",
            [session_id],
            |row| {
                Ok(SessionMetadata {
                    parent_id: row.get::<_, Option<String>>(0)?,
                    version: row.get::<_, Option<String>>(1)?,
                })
            },
        )
        .unwrap_or_default()
    }

    fn lookup_model_provider(db_path: &Path, session_id: &str) -> Option<String> {
        let conn = crate::streams::agents::opencode::open_sqlite_readonly(db_path).ok()?;
        let data: String = conn
            .query_row(
                "SELECT data FROM message WHERE session_id = ? AND data LIKE '%\"providerID\"%' ORDER BY time_updated DESC, id DESC LIMIT 1",
                [session_id],
                |row| row.get(0),
            )
            .ok()?;
        let json: serde_json::Value = serde_json::from_str(&data).ok()?;
        json.get("providerID")
            .and_then(|value| value.as_str())
            .or_else(|| {
                json.get("model")
                    .and_then(|model| model.get("providerID"))
                    .and_then(|value| value.as_str())
            })
            .map(ToString::to_string)
    }

    fn session_exists(db_path: &Path, session_id: &str) -> bool {
        let Ok(conn) = crate::streams::agents::opencode::open_sqlite_readonly(db_path) else {
            return false;
        };
        conn.query_row(
            "SELECT 1 FROM session WHERE id = ? LIMIT 1",
            [session_id],
            |_| Ok(()),
        )
        .is_ok()
    }

    fn kilo_data_path() -> Result<PathBuf, GitAiError> {
        if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME")
            && !xdg_data.trim().is_empty()
        {
            return Ok(PathBuf::from(xdg_data).join(TOOL_NAME));
        }

        // Kilo v7 uses xdg-basedir on every supported OS, including Windows.
        // Without XDG_DATA_HOME its data root is therefore ~/.local/share/kilo,
        // not LOCALAPPDATA. Keep this aligned with packages/core/src/global.ts.
        let home = dirs::home_dir()
            .ok_or_else(|| GitAiError::Generic("Could not determine home directory".to_string()))?;
        Ok(Self::default_data_path(&home))
    }

    fn default_data_path(home: &Path) -> PathBuf {
        home.join(".local").join("share").join(TOOL_NAME)
    }

    fn resolve_sqlite_db_path(
        data_path: &Path,
        database_path: Option<&str>,
        session_id: &str,
    ) -> Option<PathBuf> {
        let override_path = database_path
            .map(str::trim)
            .filter(|path| !path.is_empty() && *path != ":memory:")
            .map(PathBuf::from)
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    data_path.join(path)
                }
            });

        if let Some(path) = override_path
            && path.is_file()
        {
            return Some(path);
        }

        if data_path.is_file() {
            return Some(data_path.to_path_buf());
        }
        if !data_path.is_dir() {
            return None;
        }

        let mut candidates = Vec::new();
        let direct = data_path.join(DEFAULT_DB_NAME);
        if direct.exists() {
            candidates.push(direct);
        }

        let mut channel_dbs: Vec<PathBuf> = fs::read_dir(data_path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("kilo-") && name.ends_with(".db"))
            })
            .collect();
        channel_dbs.sort();
        candidates.extend(channel_dbs);

        candidates
            .iter()
            .find(|path| Self::session_exists(path, session_id))
            .cloned()
            .or_else(|| candidates.into_iter().next())
    }

    fn is_bash_tool(tool_name: Option<&str>) -> bool {
        tool_name.is_some_and(|name| matches!(name.to_ascii_lowercase().as_str(), "bash" | "shell"))
    }

    fn insert_metadata(metadata: &mut HashMap<String, String>, key: &str, value: Option<String>) {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            metadata.insert(key.to_string(), value);
        }
    }
}

impl AgentPreset for KiloPreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let hook_input: KiloHookInput = serde_json::from_str(hook_input).map_err(|error| {
            GitAiError::PresetError(format!("Invalid JSON in hook_input: {error}"))
        })?;

        let is_bash = Self::is_bash_tool(hook_input.tool_name.as_deref());
        let is_pre = hook_input.hook_event_name == "PreToolUse";

        let KiloHookInput {
            hook_event_name: _,
            session_id,
            cwd,
            tool_input,
            tool_name: _,
            tool_use_id,
            platform,
            client,
            editor_name,
            database_path,
        } = hook_input;

        let file_paths =
            OpenCodePreset::extract_filepaths_from_tool_input(tool_input.as_ref(), &cwd);
        let bash_command = tool_input
            .as_ref()
            .and_then(|value| {
                value
                    .get("command")
                    .or_else(|| value.get("cmd"))
                    .and_then(|value| value.as_str())
            })
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(ToString::to_string);
        let tool_use_id = tool_use_id.as_deref().unwrap_or("bash").to_string();

        let transcript = Self::resolve_stream_source(&session_id, database_path.as_deref());
        let extracted_model = transcript.as_ref().and_then(|(source, _)| {
            crate::streams::model_extraction::extract_model(
                &source.path,
                crate::streams::sweep::StreamFormat::OpenCodeSqlite,
                Some(session_id.as_str()),
            )
            .ok()
            .flatten()
        });
        let model_provider = transcript
            .as_ref()
            .and_then(|(source, _)| Self::lookup_model_provider(&source.path, &session_id));

        let mut metadata = HashMap::from([
            ("session_id".to_string(), session_id.clone()),
            ("integration".to_string(), "kilo-v7".to_string()),
        ]);
        Self::insert_metadata(
            &mut metadata,
            "platform",
            platform
                .or_else(|| client.clone())
                .or_else(|| Some("cli".to_string())),
        );
        Self::insert_metadata(&mut metadata, "client", client);
        Self::insert_metadata(&mut metadata, "editor_name", editor_name);
        Self::insert_metadata(&mut metadata, "model_provider", model_provider);
        if let Some((_, session)) = &transcript {
            Self::insert_metadata(&mut metadata, "kilo_version", session.version.clone());
        }
        if let Ok(test_path) = std::env::var("GIT_AI_KILO_STORAGE_PATH") {
            metadata.insert("__test_storage_path".to_string(), test_path);
        }

        let context = PresetContext {
            agent_id: AgentId {
                tool: TOOL_NAME.to_string(),
                id: session_id.clone(),
                model: extracted_model.unwrap_or_else(|| "unknown".to_string()),
            },
            external_session_id: session_id,
            trace_id: trace_id.to_string(),
            cwd: PathBuf::from(&cwd),
            metadata,
        };
        let stream_source = transcript.map(|(source, _)| source);

        let event = match (is_pre, is_bash) {
            (true, true) => ParsedHookEvent::PreBashCall(PreBashCall {
                context,
                tool_use_id,
                command: bash_command,
            }),
            (true, false) => ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                dirty_files: None,
                tool_use_id: Some(tool_use_id),
            }),
            (false, true) => ParsedHookEvent::PostBashCall(PostBashCall {
                context,
                tool_use_id,
                command: bash_command,
                stream_source,
            }),
            (false, false) => ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                dirty_files: None,
                stream_source,
                tool_use_id: Some(tool_use_id),
            }),
        };

        Ok(vec![event])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_kilo_pre_file_edit_uses_kilo_identity() {
        let input = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "kilo-session",
            "cwd": "/project",
            "tool_name": "edit",
            "tool_use_id": "call-1",
            "tool_input": {"filePath": "src/main.rs"},
            "platform": "jetbrains"
        })
        .to_string();

        let events = KiloPreset.parse(&input, "t_test").unwrap();
        match &events[0] {
            ParsedHookEvent::PreFileEdit(event) => {
                assert_eq!(event.context.agent_id.tool, TOOL_NAME);
                assert_eq!(
                    event.file_paths,
                    vec![PathBuf::from("/project/src/main.rs")]
                );
                assert_eq!(
                    event.context.metadata.get("platform").map(String::as_str),
                    Some("jetbrains")
                );
                assert_eq!(
                    event
                        .context
                        .metadata
                        .get("model_provider")
                        .map(String::as_str),
                    None,
                    "pre-hook metadata must not invent a provider without a transcript"
                );
            }
            _ => panic!("Expected PreFileEdit"),
        }
    }

    #[test]
    fn test_kilo_sqlite_metadata_includes_model_provider() {
        let temp = tempfile::tempdir().unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/opencode-sqlite/opencode.db");
        fs::copy(source, temp.path().join(DEFAULT_DB_NAME)).unwrap();

        let previous = std::env::var_os("GIT_AI_KILO_STORAGE_PATH");
        unsafe {
            std::env::set_var("GIT_AI_KILO_STORAGE_PATH", temp.path());
        }
        let input = json!({
            "hook_event_name": "PostToolUse",
            "session_id": "test-session-123",
            "cwd": "/project",
            "tool_name": "edit",
            "tool_use_id": "call-1",
            "tool_input": {"filePath": "src/main.rs"},
            "platform": "vscode"
        })
        .to_string();
        let events = KiloPreset.parse(&input, "t_test").unwrap();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("GIT_AI_KILO_STORAGE_PATH", value),
                None => std::env::remove_var("GIT_AI_KILO_STORAGE_PATH"),
            }
        }

        let ParsedHookEvent::PostFileEdit(event) = &events[0] else {
            panic!("Expected PostFileEdit");
        };
        assert_eq!(event.context.agent_id.model, "gpt-5");
        assert_eq!(
            event
                .context
                .metadata
                .get("model_provider")
                .map(String::as_str),
            Some("openai")
        );
    }

    #[test]
    fn test_kilo_database_override_resolves_relative_to_data_path() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("custom.db");
        fs::write(&db, "not a sqlite database").unwrap();
        assert_eq!(
            KiloPreset::resolve_sqlite_db_path(temp.path(), Some("custom.db"), "session"),
            Some(db)
        );
    }

    #[test]
    fn test_kilo_default_data_path_matches_kilo_xdg_layout_on_all_platforms() {
        assert_eq!(
            KiloPreset::default_data_path(Path::new("/Users/developer")),
            PathBuf::from("/Users/developer/.local/share/kilo")
        );
        assert_eq!(
            KiloPreset::default_data_path(Path::new(r"C:\Users\developer")),
            PathBuf::from(r"C:\Users\developer").join(".local/share/kilo")
        );
    }

    #[test]
    fn test_kilo_channel_database_is_selected_by_session() {
        let temp = tempfile::tempdir().unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/opencode-sqlite/opencode.db");
        let channel = temp.path().join("kilo-beta.db");
        fs::copy(source, &channel).unwrap();

        assert_eq!(
            KiloPreset::resolve_sqlite_db_path(temp.path(), None, "test-session-123"),
            Some(channel)
        );
    }
}
