//! Lossless, content-free facts derived from raw agent transcript events.

use super::types::SparseArray;
use crate::metrics::attrs::attr_pos;
use crate::metrics::pos_encoded::sparse_get_string;
use crate::streams::types::TOKEN_BASELINE_ONLY_FIELD;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenSnapshot {
    pub source_key: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub cumulative: bool,
    pub baseline_only: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionObservation {
    pub timestamp: u32,
    pub attrs: SparseArray,
    pub external_event_id: Option<String>,
    pub external_parent_event_id: Option<String>,
    pub external_tool_use_id: Option<String>,
    pub token: Option<TokenSnapshot>,
}

pub(crate) fn compact_session_event(
    raw: &Value,
    timestamp: u32,
    mut attrs: SparseArray,
    external_event_id: Option<String>,
    external_parent_event_id: Option<String>,
    external_tool_use_id: Option<String>,
) -> SessionObservation {
    let tool = sparse_get_string(&attrs, attr_pos::TOOL)
        .flatten()
        .unwrap_or_else(|| "unknown".to_string());
    let session_id = sparse_get_string(&attrs, attr_pos::SESSION_ID)
        .flatten()
        .unwrap_or_default();
    let baseline_only = raw
        .get(TOKEN_BASELINE_ONLY_FIELD)
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let token = extract_token_snapshot(
        raw,
        &tool,
        &session_id,
        external_event_id.as_deref(),
        baseline_only,
    );
    if let Some(model) = token.as_ref().and_then(|token| token.model.as_ref()) {
        attrs.insert(
            attr_pos::MODEL.to_string(),
            Value::String(model.to_string()),
        );
    }

    SessionObservation {
        timestamp,
        attrs,
        external_event_id,
        external_parent_event_id,
        external_tool_use_id,
        token,
    }
}

fn extract_token_snapshot(
    raw: &Value,
    tool: &str,
    session_id: &str,
    external_event_id: Option<&str>,
    baseline_only: bool,
) -> Option<TokenSnapshot> {
    // Codex reports an absolute session total. Prefer it over last_token_usage
    // when both are present so replay and repeated token_count events cannot
    // inflate usage.
    let payload = raw.get("payload");
    if let Some(usage) = payload
        .and_then(|value| value.get("info"))
        .and_then(|value| value.get("total_token_usage"))
        .filter(|value| value.is_object())
    {
        let input = non_negative(usage, "input_tokens");
        let output = non_negative(usage, "output_tokens");
        let cache_read = non_negative(usage, "cached_input_tokens");
        if input + output + cache_read == 0 {
            return None;
        }
        let model = payload
            .and_then(|value| value.get("model"))
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| attr_model_from_raw(raw));
        return Some(TokenSnapshot {
            source_key: hashed_source_key(tool, session_id, "cumulative-session", session_id),
            input,
            output,
            cache_read,
            cache_write: 0,
            provider: provider_from_raw(raw, tool, model.as_deref()),
            model,
            cumulative: true,
            baseline_only,
        });
    }

    // Claude-compatible assistant messages. The same message id can occur in
    // several transcript rows while output is streaming; persistence keeps the
    // field-wise maximum for this stable source key.
    if let Some(message) = raw.get("message")
        && message.get("role").and_then(Value::as_str) == Some("assistant")
        && let Some(usage) = message.get("usage").filter(|value| value.is_object())
    {
        let source_id = message
            .get("id")
            .and_then(Value::as_str)
            .or(external_event_id)
            .or_else(|| raw.get("requestId").and_then(Value::as_str))?;
        let input = non_negative(usage, "input_tokens");
        let output = non_negative(usage, "output_tokens");
        let cache_read = non_negative(usage, "cache_read_input_tokens");
        let cache_write = non_negative(usage, "cache_creation_input_tokens");
        if input + output + cache_read + cache_write == 0 {
            return None;
        }
        let model = message
            .get("model")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        return Some(TokenSnapshot {
            source_key: hashed_source_key(tool, session_id, "message", source_id),
            input,
            output,
            cache_read,
            cache_write,
            provider: provider_from_raw(raw, tool, model.as_deref()),
            model,
            cumulative: false,
            baseline_only,
        });
    }

    // OpenCode and Kilo expose a complete assistant-message row with token
    // counters under message.data.tokens. Some older readers expose data.tokens
    // directly, so both layouts are accepted.
    let message = raw.get("message");
    let data = message
        .and_then(|value| value.get("data"))
        .or_else(|| raw.get("data"));
    if let Some(usage) = data
        .and_then(|value| value.get("tokens"))
        .filter(|value| value.is_object())
    {
        let source_id = message
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .or_else(|| {
                data.and_then(|value| value.get("id"))
                    .and_then(Value::as_str)
            })
            .or(external_event_id)?;
        let input = non_negative(usage, "input");
        let output = non_negative(usage, "output");
        let cache_read = usage
            .get("cache")
            .map(|cache| non_negative(cache, "read"))
            .unwrap_or(0);
        let cache_write = usage
            .get("cache")
            .map(|cache| non_negative(cache, "write"))
            .unwrap_or(0);
        if input + output + cache_read + cache_write == 0 {
            return None;
        }
        let model = data
            .and_then(|value| value.get("modelID"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        return Some(TokenSnapshot {
            source_key: hashed_source_key(tool, session_id, "message", source_id),
            input,
            output,
            cache_read,
            cache_write,
            provider: data
                .and_then(|value| value.get("providerID"))
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .or_else(|| provider_from_raw(raw, tool, model.as_deref())),
            model,
            cumulative: false,
            baseline_only,
        });
    }

    // Codex variants without total_token_usage may expose only the latest turn.
    if let Some(usage) = payload
        .and_then(|value| value.get("info"))
        .and_then(|value| value.get("last_token_usage"))
        .filter(|value| value.is_object())
    {
        let source_id = external_event_id.or_else(|| {
            payload
                .and_then(|value| value.get("turn_id"))
                .and_then(Value::as_str)
        })?;
        let input = non_negative(usage, "input_tokens");
        let output = non_negative(usage, "output_tokens");
        let cache_read = non_negative(usage, "cached_input_tokens");
        if input + output + cache_read == 0 {
            return None;
        }
        let model = payload
            .and_then(|value| value.get("model"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        return Some(TokenSnapshot {
            source_key: hashed_source_key(tool, session_id, "turn", source_id),
            input,
            output,
            cache_read,
            cache_write: 0,
            provider: provider_from_raw(raw, tool, model.as_deref()),
            model,
            cumulative: false,
            baseline_only,
        });
    }

    None
}

fn non_negative(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn attr_model_from_raw(raw: &Value) -> Option<String> {
    raw.pointer("/payload/model")
        .or_else(|| raw.pointer("/payload/info/model"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn provider_from_raw(raw: &Value, tool: &str, model: Option<&str>) -> Option<String> {
    raw.pointer("/message/providerID")
        .or_else(|| raw.pointer("/message/data/providerID"))
        .or_else(|| raw.pointer("/data/providerID"))
        .or_else(|| raw.pointer("/payload/model_provider"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            let model = model.unwrap_or_default().to_ascii_lowercase();
            if model.starts_with("claude") {
                Some("anthropic".to_string())
            } else if tool == "codex" || model.starts_with("gpt-") || model.starts_with("o3") {
                Some("openai".to_string())
            } else {
                None
            }
        })
}

fn hashed_source_key(tool: &str, session_id: &str, kind: &str, source_id: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [tool, session_id, kind, source_id] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let revision = if tool == "codex" { "ts2" } else { "ts1" };
    format!("{}:{:x}", revision, hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::attrs::{EventAttributes, attr_pos};
    use crate::metrics::pos_encoded::{PosEncoded, sparse_get_string};
    use serde_json::json;

    fn attrs(tool: &str, session_id: &str) -> SparseArray {
        EventAttributes::with_version("test")
            .tool(tool)
            .session_id(session_id)
            .external_session_id(format!("external-{session_id}"))
            .repo_url("github.com/acme/repo")
            .to_sparse()
    }

    #[test]
    fn claude_streaming_copies_share_one_source_and_keep_fieldwise_snapshot() {
        let first = json!({
            "message": {
                "id": "msg-1", "role": "assistant", "model": "claude-sonnet-4",
                "usage": {"input_tokens": 10, "output_tokens": 8,
                    "cache_read_input_tokens": 20, "cache_creation_input_tokens": 3},
                "content": [{"type": "text", "text": "secret prompt output"}]
            }
        });
        let final_copy = json!({
            "message": {
                "id": "msg-1", "role": "assistant", "model": "claude-sonnet-4",
                "usage": {"input_tokens": 10, "output_tokens": 41,
                    "cache_read_input_tokens": 20, "cache_creation_input_tokens": 3},
                "content": [{"type": "tool_use", "input": {"file": "private.rs"}}]
            }
        });

        let first = compact_session_event(&first, 100, attrs("claude", "s1"), None, None, None);
        let final_copy =
            compact_session_event(&final_copy, 101, attrs("claude", "s1"), None, None, None);

        let first_token = first.token.expect("first usage snapshot");
        let final_token = final_copy.token.expect("final usage snapshot");
        assert_eq!(first_token.source_key, final_token.source_key);
        assert_eq!(final_token.input, 10);
        assert_eq!(final_token.output, 41);
        assert_eq!(final_token.cache_read, 20);
        assert_eq!(final_token.cache_write, 3);
        assert_eq!(final_token.model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(final_token.provider.as_deref(), Some("anthropic"));
        assert!(!final_token.source_key.contains("msg-1"));
    }

    #[test]
    fn opencode_and_kilo_shape_extracts_tokens_without_message_content() {
        let raw = json!({
            "message": {
                "id": "msg-assistant-001",
                "data": {
                    "role": "assistant", "modelID": "gpt-5", "providerID": "openai",
                    "tokens": {"input": 12, "output": 34, "cache": {"read": 5, "write": 2}},
                    "summary": "must not leave the parser"
                }
            },
            "parts": [{"data": {"text": "private output"}}]
        });

        let observation = compact_session_event(
            &raw,
            200,
            attrs("kilo", "s2"),
            Some("msg-assistant-001".to_string()),
            None,
            Some("call-1".to_string()),
        );

        let token = observation.token.expect("OpenCode token snapshot");
        assert_eq!(
            (
                token.input,
                token.output,
                token.cache_read,
                token.cache_write
            ),
            (12, 34, 5, 2)
        );
        assert_eq!(token.model.as_deref(), Some("gpt-5"));
        assert_eq!(token.provider.as_deref(), Some("openai"));
        assert_eq!(observation.external_tool_use_id.as_deref(), Some("call-1"));
        assert_eq!(
            sparse_get_string(&observation.attrs, attr_pos::MODEL)
                .flatten()
                .as_deref(),
            Some("gpt-5")
        );
    }

    #[test]
    fn codex_prefers_cumulative_totals_and_uses_session_scoped_source() {
        let raw = json!({
            "payload": {
                "model": "gpt-5.1-codex",
                "info": {
                    "last_token_usage": {"input_tokens": 3, "output_tokens": 4, "cached_input_tokens": 1},
                    "total_token_usage": {"input_tokens": 103, "output_tokens": 44, "cached_input_tokens": 31}
                }
            }
        });

        let observation = compact_session_event(&raw, 300, attrs("codex", "s3"), None, None, None);
        let token = observation.token.expect("Codex cumulative snapshot");
        assert_eq!((token.input, token.output, token.cache_read), (103, 44, 31));
        assert!(token.cumulative);
        assert!(!token.baseline_only);
        assert!(token.source_key.starts_with("ts2:"));
        assert_eq!(token.model.as_deref(), Some("gpt-5.1-codex"));
        assert_eq!(token.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn codex_fork_baseline_marker_is_carried_without_changing_usage() {
        let raw = json!({
            "_git_ai_token_baseline_only": true,
            "payload": {
                "info": {
                    "total_token_usage": {
                        "input_tokens": 200,
                        "output_tokens": 20,
                        "cached_input_tokens": 80
                    }
                }
            }
        });

        let observation =
            compact_session_event(&raw, 300, attrs("codex", "child"), None, None, None);
        let token = observation.token.expect("Codex fork baseline");
        assert!(token.baseline_only);
        assert_eq!((token.input, token.output, token.cache_read), (200, 20, 80));
    }

    #[test]
    fn content_only_event_keeps_recovery_metadata_but_emits_no_token_fact() {
        let raw = json!({
            "message": {"id": "user-1", "role": "user", "content": "private prompt"}
        });
        let observation = compact_session_event(
            &raw,
            400,
            attrs("claude", "s4"),
            Some("user-1".to_string()),
            Some("parent-1".to_string()),
            None,
        );

        assert!(observation.token.is_none());
        assert_eq!(observation.timestamp, 400);
        assert_eq!(observation.external_event_id.as_deref(), Some("user-1"));
        assert_eq!(
            observation.external_parent_event_id.as_deref(),
            Some("parent-1")
        );
    }
}
