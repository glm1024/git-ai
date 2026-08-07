//! Metrics API endpoints

use crate::api::client::ApiClient;
use crate::api::types::ApiErrorResponse;
use crate::error::GitAiError;
use crate::metrics::MetricsBatch;
use crate::observability::log_error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Retry delay in seconds: single retry after 60s
const RETRY_DELAYS_SECS: [u64; 1] = [60];
const METRICS_UPLOAD_MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(500);
static LAST_METRICS_UPLOAD_STARTED_AT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// Returns whether metrics are allowed to upload for the current API context.
///
/// Hosted metrics require authentication. An explicitly configured enterprise
/// metrics URL is an opt-in local deployment and may accept anonymous uploads.
pub fn metrics_upload_allowed(client: &ApiClient) -> bool {
    client.context().allows_anonymous_metrics_upload()
        || client.is_logged_in()
        || client.has_api_key()
}

fn wait_for_metrics_upload_rate_limit() -> Result<(), GitAiError> {
    let limiter = LAST_METRICS_UPLOAD_STARTED_AT.get_or_init(|| Mutex::new(None));
    let mut last_started_at = limiter.lock().map_err(|_| {
        GitAiError::Generic("metrics upload rate limiter lock poisoned".to_string())
    })?;

    wait_for_metrics_upload_rate_limit_with(&mut last_started_at, Instant::now, std::thread::sleep);
    Ok(())
}

fn wait_for_metrics_upload_rate_limit_with<Now, Sleep>(
    last_started_at: &mut Option<Instant>,
    mut now: Now,
    mut sleep: Sleep,
) where
    Now: FnMut() -> Instant,
    Sleep: FnMut(Duration),
{
    let started_at = now();
    if let Some(previous_started_at) = *last_started_at {
        let elapsed = started_at.saturating_duration_since(previous_started_at);
        if let Some(remaining) = METRICS_UPLOAD_MIN_REQUEST_INTERVAL.checked_sub(elapsed)
            && !remaining.is_zero()
        {
            sleep(remaining);
        }
    }

    *last_started_at = Some(now());
}

/// Error for a single event in the batch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsUploadError {
    /// Index of the failed event in the request
    pub index: usize,
    /// Error message
    pub error: String,
}

/// Response from metrics upload endpoint
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsUploadResponse {
    /// List of errors (only failed events, empty = all success)
    pub errors: Vec<MetricsUploadError>,
}

/// Wire acknowledgement for the enterprise Git AI metrics endpoint.
///
/// The acknowledgement fields are optional only so older responses remain
/// deserializable and can fail with a precise protocol error. They are all
/// mandatory before any local fact may be marked delivered.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetricsUploadWireResponse {
    accepted: Option<bool>,
    kind: Option<String>,
    item_count: Option<usize>,
    payload_sha256: Option<String>,
    errors: Vec<MetricsUploadError>,
}

struct PreparedMetricsUpload {
    body: String,
    payload_sha256: String,
}

/// Whether a failed upload should consume the helper's one short, in-process
/// retry. Payload/configuration rejections will not improve after 60 seconds.
pub fn metrics_upload_error_should_retry_immediately(error: &GitAiError) -> bool {
    match error {
        GitAiError::HttpStatusError { status, .. } => {
            *status == 408 || *status == 425 || *status == 429 || *status >= 500
        }
        _ => true,
    }
}

/// Whether every event in the request is deterministically undeliverable.
///
/// Authentication failures remain queued for configuration recovery. A 413 is
/// also deliberately non-permanent: the byte-aware dequeuer prevents normal
/// batches from reaching it, while a single legacy oversized commit must stay
/// on disk with a visible error instead of being silently discarded.
pub fn metrics_upload_error_is_permanent(error: &GitAiError) -> bool {
    matches!(
        error,
        GitAiError::HttpStatusError {
            status: 400 | 415 | 422,
            ..
        }
    )
}

impl MetricsUploadResponse {
    /// Validate that all failed-event indices refer to events in this batch.
    pub fn validate_error_indices(&self, batch_size: usize) -> Result<(), GitAiError> {
        if let Some(error) = self.errors.iter().find(|error| error.index >= batch_size) {
            return Err(GitAiError::Generic(format!(
                "Metrics upload response error index {} is out of bounds for batch size {}",
                error.index, batch_size
            )));
        }
        Ok(())
    }

    /// Get indices of successfully uploaded events
    pub fn successful_indices(&self, batch_size: usize) -> Vec<usize> {
        let error_indices: std::collections::HashSet<_> =
            self.errors.iter().map(|e| e.index).collect();
        (0..batch_size)
            .filter(|i| !error_indices.contains(i))
            .collect()
    }
}

fn validate_metrics_upload_response(
    response: MetricsUploadWireResponse,
    batch_size: usize,
    expected_payload_sha256: &str,
) -> Result<MetricsUploadResponse, GitAiError> {
    if response.accepted != Some(true) {
        return Err(GitAiError::Generic(
            "Metrics upload response must include accepted=true".to_string(),
        ));
    }
    if response.kind.as_deref() != Some("git_ai_metrics") {
        return Err(GitAiError::Generic(
            "Metrics upload response must include kind=git_ai_metrics".to_string(),
        ));
    }
    if response.item_count != Some(batch_size) {
        return Err(GitAiError::Generic(format!(
            "Metrics upload response itemCount {:?} does not match request event count {}",
            response.item_count, batch_size
        )));
    }
    if response.payload_sha256.as_deref() != Some(expected_payload_sha256) {
        return Err(GitAiError::Generic(
            "Metrics upload response payloadSha256 does not match the request body".to_string(),
        ));
    }

    let response = MetricsUploadResponse {
        errors: response.errors,
    };
    response.validate_error_indices(batch_size)?;
    Ok(response)
}

fn prepare_metrics_upload(batch: &MetricsBatch) -> Result<PreparedMetricsUpload, GitAiError> {
    let body = serde_json::to_string(batch).map_err(GitAiError::JsonError)?;
    let payload_sha256 = format!("{:x}", Sha256::digest(body.as_bytes()));
    Ok(PreparedMetricsUpload {
        body,
        payload_sha256,
    })
}

fn parse_metrics_upload_response(
    body: &str,
    batch_size: usize,
    expected_payload_sha256: &str,
) -> Result<MetricsUploadResponse, GitAiError> {
    let response: MetricsUploadWireResponse =
        serde_json::from_str(body).map_err(GitAiError::JsonError)?;
    validate_metrics_upload_response(response, batch_size, expected_payload_sha256)
}

fn parse_metrics_upload_error_message(body: &str) -> String {
    serde_json::from_str::<ApiErrorResponse>(body)
        .ok()
        .map(|response| response.error)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| "metrics upload request was rejected".to_string())
}

/// Upload metrics batch with retry logic.
///
/// Returns Ok(response) on success (200 response, even with partial errors).
/// Returns Err on failure after all retries exhausted.
///
/// Partial errors (200 + errors array) are logged to Sentry and returned so
/// callers can mark only the failed rows as permanently undeliverable.
pub fn upload_metrics_with_retry(
    client: &ApiClient,
    batch: &MetricsBatch,
    operation: &str,
) -> Result<MetricsUploadResponse, GitAiError> {
    // First attempt (no delay), then retry with delays
    for (attempt, delay_secs) in std::iter::once(&0u64)
        .chain(RETRY_DELAYS_SECS.iter())
        .enumerate()
    {
        if attempt > 0 {
            eprintln!(
                "[metrics] Retrying upload after {}s delay (attempt {}/{})",
                delay_secs,
                attempt + 1,
                RETRY_DELAYS_SECS.len() + 1
            );
            std::thread::sleep(std::time::Duration::from_secs(*delay_secs));
        }

        match client.upload_metrics(batch) {
            Ok(response) => {
                // 200 response - log any validation errors to Sentry
                for error in &response.errors {
                    log_error(
                        &GitAiError::Generic(format!(
                            "Metrics {} error at index {}: {}",
                            operation, error.index, error.error
                        )),
                        Some(serde_json::json!({
                            "operation": operation,
                            "error_index": error.index
                        })),
                    );
                }
                return Ok(response);
            }
            Err(e) => {
                let should_retry = metrics_upload_error_should_retry_immediately(&e);
                if attempt == RETRY_DELAYS_SECS.len() || !should_retry {
                    if !should_retry {
                        eprintln!(
                            "[metrics] Upload rejected without an immediate retry: {}",
                            e
                        );
                    } else {
                        eprintln!("[metrics] All retries exhausted, giving up");
                    }
                    return Err(e);
                }
                eprintln!("[metrics] Upload failed: {}, will retry...", e);
            }
        }
    }

    Err(GitAiError::Generic(
        "All upload retries exhausted".to_string(),
    ))
}

/// Metrics API endpoints
impl ApiClient {
    /// Upload metrics batch to the server (max 1000 events)
    ///
    /// # Arguments
    /// * `batch` - The metrics batch to upload
    ///
    /// # Returns
    /// * `Ok(MetricsUploadResponse)` - Response with errors (empty = all success)
    /// * `Err(GitAiError)` - Request failed
    pub fn upload_metrics(
        &self,
        batch: &MetricsBatch,
    ) -> Result<MetricsUploadResponse, GitAiError> {
        wait_for_metrics_upload_rate_limit()?;
        let prepared = prepare_metrics_upload(batch)?;
        let response = self
            .context()
            .post_serialized_json("/worker/metrics/upload", &prepared.body)?;
        let status_code = response.status_code;

        let body = response
            .as_str()
            .map_err(|e| GitAiError::Generic(format!("Failed to read response body: {}", e)))?;

        match status_code {
            200 => {
                parse_metrics_upload_response(body, batch.events.len(), &prepared.payload_sha256)
            }
            _ => Err(GitAiError::HttpStatusError {
                status: status_code,
                message: parse_metrics_upload_error_message(body),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::client::{ApiContext, MetricsEndpointMode};
    use sha2::{Digest, Sha256};
    use std::cell::{Cell, RefCell};

    #[test]
    fn test_successful_indices() {
        let response = MetricsUploadResponse {
            errors: vec![
                MetricsUploadError {
                    index: 1,
                    error: "error".to_string(),
                },
                MetricsUploadError {
                    index: 3,
                    error: "error".to_string(),
                },
            ],
        };

        let successful = response.successful_indices(5);
        assert_eq!(successful, vec![0, 2, 4]);
    }

    #[test]
    fn test_successful_indices_empty_errors() {
        let response = MetricsUploadResponse { errors: vec![] };
        let successful = response.successful_indices(3);
        assert_eq!(successful, vec![0, 1, 2]);
    }

    #[test]
    fn test_successful_indices_all_errors() {
        let response = MetricsUploadResponse {
            errors: vec![
                MetricsUploadError {
                    index: 0,
                    error: "error".to_string(),
                },
                MetricsUploadError {
                    index: 1,
                    error: "error".to_string(),
                },
            ],
        };
        let successful = response.successful_indices(2);
        assert!(successful.is_empty());
    }

    #[test]
    fn metrics_upload_permission_uses_the_constructed_context_mode() {
        let client = |mode, auth_token: Option<&str>, api_key: Option<&str>| {
            ApiClient::new(ApiContext {
                base_url: "https://metrics.example.com".to_string(),
                auth_token: auth_token.map(str::to_string),
                api_key: api_key.map(str::to_string),
                author_identity: None,
                timeout_secs: Some(30),
                metrics_endpoint_mode: mode,
            })
        };

        assert!(metrics_upload_allowed(&client(
            MetricsEndpointMode::DedicatedAnonymous,
            None,
            None,
        )));
        assert!(!metrics_upload_allowed(&client(
            MetricsEndpointMode::Standard,
            None,
            None,
        )));
        assert!(metrics_upload_allowed(&client(
            MetricsEndpointMode::Standard,
            Some("token"),
            None,
        )));
        assert!(metrics_upload_allowed(&client(
            MetricsEndpointMode::Standard,
            None,
            Some("api-key"),
        )));
    }

    #[test]
    fn metrics_upload_rate_limiter_enforces_half_second_spacing() {
        let base = Instant::now();
        let current = Cell::new(base);
        let sleeps = RefCell::new(Vec::new());
        let mut last_started_at = None;

        wait_for_metrics_upload_rate_limit_with(
            &mut last_started_at,
            || current.get(),
            |duration| {
                sleeps.borrow_mut().push(duration);
                current.set(current.get() + duration);
            },
        );
        assert!(sleeps.borrow().is_empty());
        assert_eq!(last_started_at, Some(base));

        current.set(base + Duration::from_millis(100));
        wait_for_metrics_upload_rate_limit_with(
            &mut last_started_at,
            || current.get(),
            |duration| {
                sleeps.borrow_mut().push(duration);
                current.set(current.get() + duration);
            },
        );
        assert_eq!(&*sleeps.borrow(), &[Duration::from_millis(400)]);
        assert_eq!(last_started_at, Some(base + Duration::from_millis(500)));

        current.set(base + Duration::from_millis(1000));
        wait_for_metrics_upload_rate_limit_with(
            &mut last_started_at,
            || current.get(),
            |duration| {
                sleeps.borrow_mut().push(duration);
                current.set(current.get() + duration);
            },
        );
        assert_eq!(sleeps.borrow().len(), 1);
        assert_eq!(last_started_at, Some(base + Duration::from_millis(1000)));
    }

    #[test]
    fn test_validate_error_indices_rejects_out_of_bounds_index() {
        let response = MetricsUploadResponse {
            errors: vec![MetricsUploadError {
                index: 2,
                error: "error".to_string(),
            }],
        };

        assert!(response.validate_error_indices(2).is_err());
    }

    #[test]
    fn deterministic_client_errors_skip_immediate_retry_and_are_permanent() {
        for status in [400, 415, 422] {
            let error = GitAiError::HttpStatusError {
                status,
                message: "invalid request".to_string(),
            };
            assert!(!metrics_upload_error_should_retry_immediately(&error));
            assert!(metrics_upload_error_is_permanent(&error));
        }
    }

    #[test]
    fn configuration_conflict_and_oversized_errors_remain_queued() {
        for status in [401, 403, 404, 405, 409, 413] {
            let error = GitAiError::HttpStatusError {
                status,
                message: "configuration must change".to_string(),
            };
            assert!(!metrics_upload_error_should_retry_immediately(&error));
            assert!(!metrics_upload_error_is_permanent(&error));
        }
    }

    #[test]
    fn transient_http_and_transport_errors_allow_immediate_retry() {
        for status in [408, 425, 429, 500, 502, 503] {
            let error = GitAiError::HttpStatusError {
                status,
                message: "temporary failure".to_string(),
            };
            assert!(metrics_upload_error_should_retry_immediately(&error));
            assert!(!metrics_upload_error_is_permanent(&error));
        }
        assert!(metrics_upload_error_should_retry_immediately(
            &GitAiError::Generic("network failure".to_string())
        ));
    }

    #[test]
    fn legacy_metrics_200_deserializes_but_fails_closed_without_acknowledgement() {
        let wire_response: MetricsUploadWireResponse =
            serde_json::from_str(r#"{"errors":[]}"#).expect("legacy response must deserialize");

        let error = validate_metrics_upload_response(wire_response, 1, "expected-sha")
            .expect_err("a legacy 200 must not acknowledge local facts");

        assert!(
            error.to_string().contains("accepted=true"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn metrics_acknowledgement_is_validated_before_error_indices() {
        let wire_response: MetricsUploadWireResponse =
            serde_json::from_str(r#"{"errors":[{"index":99,"error":"bad"}]}"#)
                .expect("legacy response must deserialize");

        let error = validate_metrics_upload_response(wire_response, 1, "expected-sha")
            .expect_err("missing acknowledgement must fail");

        assert!(
            error.to_string().contains("accepted=true"),
            "acknowledgement must be checked before error indices: {error}"
        );
    }

    #[test]
    fn metrics_acknowledgement_requires_exact_kind_count_and_payload_digest() {
        let valid = |accepted, kind: &str, item_count, payload_sha256: &str| {
            serde_json::from_value::<MetricsUploadWireResponse>(serde_json::json!({
                "accepted": accepted,
                "kind": kind,
                "itemCount": item_count,
                "payloadSha256": payload_sha256,
                "errors": []
            }))
            .expect("response must deserialize")
        };

        assert!(
            validate_metrics_upload_response(
                valid(true, "git_ai_metrics", 2, "expected-sha"),
                2,
                "expected-sha"
            )
            .is_ok()
        );

        for (response, expected_message) in [
            (
                valid(false, "git_ai_metrics", 2, "expected-sha"),
                "accepted=true",
            ),
            (
                valid(true, "other", 2, "expected-sha"),
                "kind=git_ai_metrics",
            ),
            (
                valid(true, "git_ai_metrics", 1, "expected-sha"),
                "itemCount",
            ),
            (
                valid(true, "git_ai_metrics", 2, "different-sha"),
                "payloadSha256",
            ),
        ] {
            let error = validate_metrics_upload_response(response, 2, "expected-sha")
                .expect_err("mismatched acknowledgement must fail");
            assert!(
                error.to_string().contains(expected_message),
                "unexpected error for {expected_message}: {error}"
            );
        }
    }

    #[test]
    fn every_metrics_acknowledgement_field_is_required() {
        for (missing_field, expected_message) in [
            ("accepted", "accepted=true"),
            ("kind", "kind=git_ai_metrics"),
            ("itemCount", "itemCount"),
            ("payloadSha256", "payloadSha256"),
        ] {
            let mut response = serde_json::json!({
                "accepted": true,
                "kind": "git_ai_metrics",
                "itemCount": 1,
                "payloadSha256": "expected-sha",
                "errors": []
            });
            response
                .as_object_mut()
                .expect("response is an object")
                .remove(missing_field);
            let response: MetricsUploadWireResponse =
                serde_json::from_value(response).expect("missing ACK fields remain deserializable");

            let error = validate_metrics_upload_response(response, 1, "expected-sha")
                .expect_err("missing acknowledgement field must fail closed");
            assert!(
                error.to_string().contains(expected_message),
                "unexpected error for missing {missing_field}: {error}"
            );
        }
    }

    #[test]
    fn metrics_upload_hash_binds_to_the_exact_serialized_request_body() {
        let batch = MetricsBatch::new(vec![]);
        let prepared = prepare_metrics_upload(&batch).expect("prepare metrics batch");
        let expected_body = serde_json::to_string(&batch).expect("serialize metrics batch");
        let expected_sha256 = format!("{:x}", Sha256::digest(expected_body.as_bytes()));

        assert_eq!(prepared.body, expected_body);
        assert_eq!(prepared.payload_sha256, expected_sha256);
    }

    #[test]
    fn parsing_a_forged_legacy_200_response_fails_closed() {
        let error = parse_metrics_upload_response(r#"{"errors":[]}"#, 0, "expected-sha")
            .expect_err("legacy 200 must not delete local facts");

        assert!(
            error.to_string().contains("accepted=true"),
            "unexpected error: {error}"
        );
        assert!(
            !metrics_upload_error_is_permanent(&error),
            "protocol failures must retain local facts for retry"
        );
    }

    #[test]
    fn metrics_upload_error_accepts_backend_msg_and_legacy_error_fields() {
        assert_eq!(
            parse_metrics_upload_error_message(r#"{"code":429,"msg":"server busy"}"#),
            "server busy"
        );
        assert_eq!(
            parse_metrics_upload_error_message(r#"{"error":"legacy failure"}"#),
            "legacy failure"
        );
    }
}
