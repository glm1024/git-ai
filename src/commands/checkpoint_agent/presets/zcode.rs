use super::{
    AgentPreset, FailedToolCall, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall,
    PreFileEdit, PresetContext, SessionActivity, StreamFormat, StreamSource,
};
use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::authorship::working_log::AgentId;
use crate::error::GitAiError;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct ZCodePreset;

fn error(message: impl Into<String>) -> GitAiError {
    GitAiError::PresetError(message.into())
}

/// The runtime emits both spellings. Conflicting aliases must never bind an
/// edit to an unrelated session or tool call.
fn alias<'a>(value: &'a Value, keys: &[&str]) -> Result<Option<&'a Value>, GitAiError> {
    let mut found = None;
    for key in keys {
        if let Some(candidate) = value.get(*key).filter(|value| !value.is_null()) {
            if found.is_some_and(|previous| previous != candidate) {
                return Err(error(format!(
                    "ZCode conflicting aliases: {}",
                    keys.join("/")
                )));
            }
            found = Some(candidate);
        }
    }
    Ok(found)
}

fn text<'a>(value: &'a Value, keys: &[&str]) -> Result<Option<&'a str>, GitAiError> {
    alias(value, keys)?
        .map(|value| {
            value
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| error(format!("ZCode invalid {}", keys[0])))
        })
        .transpose()
}

fn required<'a>(value: &'a Value, keys: &[&str]) -> Result<&'a str, GitAiError> {
    text(value, keys)?.ok_or_else(|| error(format!("ZCode missing {}", keys[0])))
}

fn failed_outcome(event: &str, response: Option<&Value>) -> Option<&'static str> {
    if response.is_some_and(|v| {
        v.get("status").and_then(Value::as_str) == Some("backgrounded")
            || v.get("backgroundTaskId").is_some_and(|v| !v.is_null())
            || v.get("backgrounded").and_then(Value::as_bool) == Some(true)
    }) {
        return Some("backgrounded");
    }
    if event == "PostToolUseFailure"
        || response.is_some_and(|v| {
            matches!(
                v.get("status").and_then(Value::as_str),
                Some("failed" | "timed_out" | "cancelled" | "spawn_error")
            ) || v
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0)
                || v.get("isError").and_then(Value::as_bool) == Some(true)
                || ["timedOut", "cancelled", "interrupted"]
                    .iter()
                    .any(|key| v.get(*key).and_then(Value::as_bool) == Some(true))
        })
    {
        Some("failed")
    } else {
        None
    }
}

impl AgentPreset for ZCodePreset {
    fn parse(&self, hook_input: &str, trace_id: &str) -> Result<Vec<ParsedHookEvent>, GitAiError> {
        let observed_at_millis = chrono::Utc::now().timestamp_millis();
        let value: Value = serde_json::from_str(hook_input)?;
        let event = required(&value, &["hook_event_name", "hookEventName", "event"])?;
        if !matches!(
            event,
            "SessionStart"
                | "UserPromptSubmit"
                | "Stop"
                | "PreToolUse"
                | "PostToolUse"
                | "PostToolUseFailure"
        ) {
            return Ok(vec![]);
        }
        let session_id = required(&value, &["session_id", "sessionId"])?;
        let cwd = required(&value, &["cwd"])?;
        let cwd_path = PathBuf::from(cwd);
        if !cwd_path.is_absolute() {
            return Err(error("ZCode cwd must be absolute"));
        }
        let lifecycle = matches!(event, "SessionStart" | "UserPromptSubmit" | "Stop");
        let call = if lifecycle {
            None
        } else {
            Some(required(&value, &["tool_use_id", "toolCallId"])?)
        };
        let source_path = Some(crate::streams::agents::zcode::database_path().ok_or_else(||
            error("Cannot determine ZCode session database; check the absolute config/storage path and JSON configuration"))?);
        let model_context = match (source_path.as_ref(), call) {
            (Some(path), Some(call)) if path.is_file() => {
                crate::streams::agents::zcode::resolve_tool_context(
                    path,
                    session_id,
                    call,
                    text(&value, &["traceId", "trace_id"])?,
                    text(&value, &["turnId", "turn_id"])?,
                )
                .unwrap_or_else(|error| {
                    tracing::debug!(%error, "ZCode exact model evidence is unavailable");
                    None
                })
            }
            _ => None,
        };
        let mut metadata = HashMap::from([
            ("integration".into(), "zcode".into()),
            ("platform".into(), "zcode".into()),
            ("editor_name".into(), "ZCode".into()),
        ]);
        if let Some(model) = model_context.as_ref() {
            metadata.insert("model_provider".into(), model.provider.clone());
        }
        let context = PresetContext {
            agent_id: AgentId {
                tool: "zcode".into(),
                id: session_id.into(),
                model: model_context
                    .as_ref()
                    .map(|m| m.model.clone())
                    .unwrap_or_else(|| "unknown".into()),
            },
            external_session_id: session_id.into(),
            trace_id: trace_id.into(),
            cwd: cwd_path,
            metadata,
        };
        let parent_session_id = model_context
            .as_ref()
            .and_then(|model| model.parent_session_id.clone())
            .or_else(|| {
                source_path
                    .as_ref()
                    .filter(|path| path.is_file())
                    .and_then(|path| {
                        crate::streams::agents::zcode::resolve_session_context(path, session_id)
                            .ok()
                            .flatten()
                    })
                    .and_then(|session| session.parent_session_id)
            });
        let source = source_path.map(|path| StreamSource {
            path,
            format: StreamFormat::ZCodeSqlite,
            session_id: generate_session_id(session_id, "zcode"),
            external_session_id: session_id.into(),
            external_parent_session_id: parent_session_id,
        });
        let activity = source.clone().map(|source| {
            ParsedHookEvent::SessionActivity(SessionActivity {
                context: context.clone(),
                stream_source: source,
                observed_at_millis,
            })
        });
        if lifecycle {
            return Ok(activity.into_iter().collect());
        }

        let call = call.expect("tool event identity was validated").to_string();
        let tool = required(&value, &["tool_name", "toolName"])?;
        if !matches!(tool, "Write" | "Edit" | "ApplyPatch" | "Bash") {
            return Ok(activity.into_iter().collect());
        }
        let input = alias(&value, &["tool_input", "toolInput"])?
            .ok_or_else(|| error("ZCode tool input is required"))?;
        let mut context = context;
        context.metadata.insert("tool_use_id".into(), call.clone());
        let command = text(input, &["command"])?.map(str::to_string);
        let mut raw_paths = Vec::new();
        if matches!(tool, "Write" | "Edit") {
            raw_paths.push(required(input, &["file_path"])?.to_string());
        } else if tool == "ApplyPatch" {
            super::parse::collect_apply_patch_paths_from_text(
                required(input, &["patch_text"])?,
                &mut raw_paths,
            );
            if raw_paths.is_empty() {
                return Err(error("ZCode patch has no declared files"));
            }
        } else if command.is_none() {
            return Err(error("ZCode Bash command is required"));
        }
        let file_paths =
            super::opencode::OpenCodePreset::normalize_explicit_filepaths(&raw_paths, cwd);
        let stream_source = source.filter(|source| source.path.is_file());
        let outcome = failed_outcome(
            event,
            alias(
                &value,
                &["tool_response", "toolResponse", "toolOutput", "tool_output"],
            )?,
        );
        context.metadata.insert(
            "tool_outcome".into(),
            if event == "PreToolUse" {
                "pending"
            } else {
                outcome.unwrap_or("succeeded")
            }
            .into(),
        );
        let parsed = if outcome.is_some() && event != "PreToolUse" {
            ParsedHookEvent::FailedToolCall(FailedToolCall {
                context,
                tool_use_id: call,
                file_paths,
                bash: tool == "Bash",
                command,
                stream_source,
            })
        } else if tool == "Bash" {
            if event == "PreToolUse" {
                ParsedHookEvent::PreBashCall(PreBashCall {
                    context,
                    tool_use_id: call,
                    command,
                })
            } else {
                ParsedHookEvent::PostBashCall(PostBashCall {
                    context,
                    tool_use_id: call,
                    command,
                    stream_source,
                })
            }
        } else if event == "PreToolUse" {
            ParsedHookEvent::PreFileEdit(PreFileEdit {
                context,
                file_paths,
                dirty_files: None,
                tool_use_id: Some(call),
            })
        } else {
            ParsedHookEvent::PostFileEdit(PostFileEdit {
                context,
                file_paths,
                dirty_files: None,
                tool_use_id: Some(call),
                stream_source,
            })
        };
        let mut events = vec![parsed];
        events.extend(activity);
        Ok(events)
    }
}
