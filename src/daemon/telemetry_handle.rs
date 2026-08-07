//! Global daemon telemetry handle for sending events over the control socket.
//!
//! When daemon mode is active, this handle is initialized once on process start
//! and used by the observability and metrics modules to route events through the
//! daemon instead of writing to per-PID log files.
//!
//! The handle maintains a persistent socket connection that is shared across all
//! callers (telemetry, CAS, and potentially checkpoints). This avoids the
//! overhead of opening a new connection for every fire-and-forget event.

use crate::daemon::control_api::{
    CasSyncPayload, ControlRequest, ControlResponse, TelemetryEnvelope,
};
use crate::daemon::{DaemonClientStream, open_local_socket_stream_with_timeout};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Read/write timeout for the persistent daemon socket.
/// Prevents indefinite blocking if the daemon becomes unresponsive.
const DAEMON_SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum time to wait for the daemon socket on process start.
#[cfg(not(any(test, feature = "test-support")))]
const DAEMON_TELEMETRY_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Global handle to the daemon control socket for telemetry submission.
static DAEMON_TELEMETRY_HANDLE: OnceLock<Mutex<Option<DaemonTelemetryHandle>>> = OnceLock::new();

struct DaemonTelemetryHandle {
    socket_path: PathBuf,
    conn: BufReader<DaemonClientStream>,
}

impl DaemonTelemetryHandle {
    /// Apply read/write timeouts to the underlying socket so that I/O never
    /// blocks indefinitely (which would hold the global mutex and stall the
    /// entire process).
    fn apply_socket_timeouts(stream: &mut DaemonClientStream, socket_path: &std::path::Path) {
        let _ = crate::daemon::set_daemon_client_stream_timeouts(
            stream,
            socket_path,
            DAEMON_SOCKET_IO_TIMEOUT,
        );
    }

    /// Send a control request over the persistent connection and read the response.
    /// On I/O error, attempts to reconnect once before giving up.
    fn send(&mut self, request: &ControlRequest) -> Result<ControlResponse, String> {
        match self.send_inner(request) {
            Ok(resp) => Ok(resp),
            Err(first_err) => {
                // Connection may have been dropped by the daemon; try reconnecting once.
                match open_local_socket_stream_with_timeout(
                    &self.socket_path,
                    Duration::from_secs(1),
                ) {
                    Ok(mut stream) => {
                        Self::apply_socket_timeouts(&mut stream, &self.socket_path);
                        self.conn = BufReader::new(stream);
                        self.send_inner(request)
                            .map_err(|e| format!("reconnect ok but send failed: {}", e))
                    }
                    Err(reconnect_err) => Err(format!(
                        "send failed ({}), reconnect also failed ({})",
                        first_err, reconnect_err
                    )),
                }
            }
        }
    }

    fn send_inner(&mut self, request: &ControlRequest) -> Result<ControlResponse, String> {
        let mut body = serde_json::to_vec(request).map_err(|e| e.to_string())?;
        body.push(b'\n');
        self.conn
            .get_mut()
            .write_all(&body)
            .map_err(|e| format!("write: {}", e))?;
        self.conn
            .get_mut()
            .flush()
            .map_err(|e| format!("flush: {}", e))?;

        let mut line = String::new();
        self.conn
            .read_line(&mut line)
            .map_err(|e| format!("read: {}", e))?;
        if line.trim().is_empty() {
            return Err("empty response from daemon".to_string());
        }
        serde_json::from_str(line.trim()).map_err(|e| format!("parse: {}", e))
    }
}

/// Result of attempting to initialize the global daemon telemetry handle.
pub enum DaemonTelemetryInitResult {
    /// Successfully connected to daemon.
    Connected,
    /// Failed to connect; contains the error message.
    Failed(String),
    /// Not in daemon mode or already inside the daemon process.
    Skipped,
}

/// Initialize the global daemon telemetry handle.
///
/// Should be called once on process start when daemon mode is active.
/// Attempts to connect to the daemon control socket (starting the daemon if needed)
/// with a 2-second timeout. The connection is kept open and reused for all
/// subsequent telemetry and CAS submissions.
///
/// Returns the result indicating success, failure, or skip.
pub fn init_daemon_telemetry_handle() -> DaemonTelemetryInitResult {
    // Don't initialize if we're inside the daemon process itself
    if crate::daemon::daemon_process_active() {
        let _ = DAEMON_TELEMETRY_HANDLE.get_or_init(|| Mutex::new(None));
        return DaemonTelemetryInitResult::Skipped;
    }

    // In test builds, only connect if the daemon control socket is explicitly set.
    #[cfg(any(test, feature = "test-support"))]
    {
        let socket_path = std::env::var("GIT_AI_DAEMON_CONTROL_SOCKET")
            .ok()
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
            .filter(|p| p.exists());

        match socket_path {
            Some(path) => {
                match open_local_socket_stream_with_timeout(&path, Duration::from_secs(2)) {
                    Ok(mut stream) => {
                        DaemonTelemetryHandle::apply_socket_timeouts(&mut stream, &path);
                        let handle = DaemonTelemetryHandle {
                            socket_path: path,
                            conn: BufReader::new(stream),
                        };
                        let _ = DAEMON_TELEMETRY_HANDLE.get_or_init(|| Mutex::new(Some(handle)));
                        DaemonTelemetryInitResult::Connected
                    }
                    Err(e) => {
                        let _ = DAEMON_TELEMETRY_HANDLE.get_or_init(|| Mutex::new(None));
                        DaemonTelemetryInitResult::Failed(e.to_string())
                    }
                }
            }
            None => {
                let _ = DAEMON_TELEMETRY_HANDLE.get_or_init(|| Mutex::new(None));
                DaemonTelemetryInitResult::Skipped
            }
        }
    }

    #[cfg(not(any(test, feature = "test-support")))]
    {
        // Try to ensure daemon is running and connect
        let config = match crate::commands::daemon::ensure_daemon_running(
            DAEMON_TELEMETRY_CONNECT_TIMEOUT,
        ) {
            Ok(config) => config,
            Err(e) => {
                let _ = DAEMON_TELEMETRY_HANDLE.get_or_init(|| Mutex::new(None));
                return DaemonTelemetryInitResult::Failed(e);
            }
        };

        // Open a persistent connection to the control socket
        match open_local_socket_stream_with_timeout(
            &config.control_socket_path,
            DAEMON_TELEMETRY_CONNECT_TIMEOUT,
        ) {
            Ok(mut stream) => {
                DaemonTelemetryHandle::apply_socket_timeouts(
                    &mut stream,
                    &config.control_socket_path,
                );
                let handle = DaemonTelemetryHandle {
                    socket_path: config.control_socket_path,
                    conn: BufReader::new(stream),
                };
                let _ = DAEMON_TELEMETRY_HANDLE.get_or_init(|| Mutex::new(Some(handle)));
                DaemonTelemetryInitResult::Connected
            }
            Err(e) => {
                let _ = DAEMON_TELEMETRY_HANDLE.get_or_init(|| Mutex::new(None));
                DaemonTelemetryInitResult::Failed(e.to_string())
            }
        }
    }
}

/// Check if the daemon telemetry handle is available for sending events.
pub fn daemon_telemetry_available() -> bool {
    DAEMON_TELEMETRY_HANDLE
        .get()
        .and_then(|m| m.lock().ok())
        .is_some_and(|guard| guard.is_some())
}

/// Send a control request over the shared persistent connection.
///
/// This is the unified entry point used by telemetry, CAS submissions,
/// and any other code that needs to talk to the daemon. The connection
/// is reused across calls; if the socket is dead it will reconnect once.
///
/// Returns the daemon's response, or an error string on failure.
pub fn send_via_daemon(request: &ControlRequest) -> Result<ControlResponse, String> {
    let Some(handle_mutex) = DAEMON_TELEMETRY_HANDLE.get() else {
        return Err("daemon telemetry handle not initialized".to_string());
    };
    let Ok(mut guard) = handle_mutex.lock() else {
        return Err("daemon telemetry handle lock poisoned".to_string());
    };
    let Some(handle) = guard.as_mut() else {
        return Err("daemon telemetry handle not connected".to_string());
    };
    handle.send(request)
}

/// Submit telemetry envelopes to the daemon over the control socket.
///
/// Formal metric events fall back to a synchronous local SQLite transaction
/// when the daemon cannot be reached or rejects the request. Other telemetry
/// retains its existing best-effort behavior.
pub fn submit_telemetry(envelopes: Vec<TelemetryEnvelope>) -> Result<(), String> {
    if envelopes.is_empty() {
        return Ok(());
    }
    submit_telemetry_with(envelopes, send_via_daemon, |events| {
        crate::daemon::telemetry_worker::persist_metrics_to_db_blocking(events)
    })
}

fn submit_telemetry_with<Send, Persist>(
    envelopes: Vec<TelemetryEnvelope>,
    send: Send,
    persist: Persist,
) -> Result<(), String>
where
    Send: FnOnce(&ControlRequest) -> Result<ControlResponse, String>,
    Persist: FnOnce(&[crate::metrics::MetricEvent]) -> Result<(), crate::error::GitAiError>,
{
    let metric_events = envelopes
        .iter()
        .filter_map(|envelope| match envelope {
            TelemetryEnvelope::Metrics { events } => Some(events.as_slice()),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    let request = ControlRequest::SubmitTelemetry { envelopes };
    let submission_error = match send(&request) {
        Ok(response) if response.ok => return Ok(()),
        Ok(response) => response
            .error
            .unwrap_or_else(|| "daemon rejected telemetry without an error message".to_string()),
        Err(error) => error,
    };

    if metric_events.is_empty() {
        return Ok(());
    }

    persist(&metric_events).map_err(|fallback_error| {
        format!(
            "daemon metric submission failed ({submission_error}); synchronous local fallback failed ({fallback_error})"
        )
    })
}

/// Submit CAS sync records to the daemon over the control socket.
///
/// Fire-and-forget: same as submit_telemetry.
pub fn submit_cas(records: Vec<CasSyncPayload>) {
    if records.is_empty() {
        return;
    }
    let request = ControlRequest::SubmitCas { records };
    let _ = send_via_daemon(&request);
}

/// Signal the daemon that new notes are pending in `notes-db` and should be
/// flushed to the remote backend.
///
/// Fire-and-forget: silently drops on failure (flush will happen on the next
/// periodic tick regardless).
pub fn submit_notes() {
    let request = ControlRequest::FlushNotes;
    let _ = send_via_daemon(&request);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::GitAiError;
    use crate::metrics::MetricEvent;
    use std::cell::Cell;

    fn metric_envelope() -> TelemetryEnvelope {
        TelemetryEnvelope::Metrics {
            events: vec![MetricEvent {
                timestamp: 1,
                event_id: 1,
                instance_id: None,
                values: Default::default(),
                attrs: Default::default(),
            }],
        }
    }

    #[test]
    fn failed_daemon_submission_persists_metrics_with_local_fallback() {
        let fallback_called = Cell::new(false);

        submit_telemetry_with(
            vec![metric_envelope()],
            |_| Err("daemon socket closed".to_string()),
            |_| {
                fallback_called.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(fallback_called.get());
    }

    #[test]
    fn negative_daemon_ack_uses_fallback_and_reports_fallback_failure() {
        let error = submit_telemetry_with(
            vec![metric_envelope()],
            |_| Ok(ControlResponse::err("metrics transaction failed")),
            |_| Err(GitAiError::Generic("fallback database failed".to_string())),
        )
        .unwrap_err();

        assert!(error.contains("metrics transaction failed"));
        assert!(error.contains("fallback database failed"));
    }

    #[test]
    fn successful_daemon_ack_does_not_duplicate_metrics_in_fallback() {
        let fallback_called = Cell::new(false);

        submit_telemetry_with(
            vec![metric_envelope()],
            |_| Ok(ControlResponse::ok(None, None)),
            |_| {
                fallback_called.set(true);
                Ok(())
            },
        )
        .unwrap();

        assert!(!fallback_called.get());
    }
}
