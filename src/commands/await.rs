//! Handle the `await` command.
//!
//! Waits for the git-ai daemon to finish all in-flight work and telemetry
//! flushing before the process exits. This is useful in cloud sandboxes where
//! the container may be torn down immediately after a build step.

use crate::daemon::{ControlRequest, ControlResponse, send_control_request};
use serde::Deserialize;
use std::thread;
use std::time::Duration;

const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
const LOG_INTERVAL_SECONDS: u64 = 5;

#[derive(Debug, Deserialize)]
struct AwaitResponse {
    done: bool,
    timed_out: bool,
    metrics_remaining: usize,
    notes_remaining: usize,
    #[serde(default)]
    blocked_checkpoints: usize,
    #[serde(default)]
    blocked_checkpoint_reasons: Vec<String>,
    #[serde(default)]
    phase_error: Option<String>,
}

/// Handle `git-ai await [--timeout <seconds>]`.
///
/// Beta command. Blocks until the daemon reports it has drained all work and flushed
/// telemetry, or until the configured timeout elapses. Prints a progress
/// message every few seconds while waiting.
pub(crate) fn handle_await(args: &[String]) {
    let mut timeout_secs = DEFAULT_TIMEOUT_SECONDS;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--timeout" => {
                let value = iter.next().unwrap_or_else(|| {
                    eprintln!("await: --timeout requires a value");
                    std::process::exit(1);
                });
                timeout_secs = match value.parse::<u64>() {
                    Ok(value) if value > 0 => value,
                    _ => {
                        eprintln!("await: --timeout must be a positive integer");
                        std::process::exit(1);
                    }
                };
            }
            "--help" | "-h" | "help" => {
                print_usage();
                std::process::exit(0);
            }
            _ => {
                eprintln!("await: unknown argument: {}", arg);
                print_usage();
                std::process::exit(1);
            }
        }
    }

    let config = match crate::commands::daemon::ensure_daemon_running(Duration::from_secs(5)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("await: failed to reach git-ai background service: {}", e);
            std::process::exit(1);
        }
    };

    let request = ControlRequest::Await { timeout_secs };

    eprintln!("await: waiting for background service to finish work...");

    let (stop_progress_tx, stop_progress_rx) = std::sync::mpsc::channel();
    let progress_handle = thread::spawn(move || {
        loop {
            match stop_progress_rx.recv_timeout(Duration::from_secs(LOG_INTERVAL_SECONDS)) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    eprintln!("await: still waiting for background service...");
                }
            }
        }
    });

    let response = send_control_request(&config.control_socket_path, &request);
    let _ = stop_progress_tx.send(());
    let _ = progress_handle.join();

    let response = match response {
        Ok(r) => r,
        Err(e) => {
            eprintln!("await: failed to send request to background service: {}", e);
            std::process::exit(1);
        }
    };

    if !response.ok {
        let message = response_error_message(&response);
        eprintln!("await: {}", message);
        std::process::exit(1);
    }

    let data = match response.data {
        Some(value) => match serde_json::from_value::<AwaitResponse>(value) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("await: failed to parse response: {}", e);
                std::process::exit(1);
            }
        },
        None => {
            eprintln!("await: background service returned an empty response");
            std::process::exit(1);
        }
    };

    if data.timed_out {
        eprintln!("await: timed out after {}s", timeout_secs);
        std::process::exit(1);
    }

    if !data.done {
        let blocked = if data.blocked_checkpoints > 0 {
            format!(
                "; {} blocked checkpoint(s): {}. Original evidence is preserved; stop the background service, then run `git-ai repair checkpoint-baseline --job-key <printed-key>` for an impact preview",
                data.blocked_checkpoints,
                data.blocked_checkpoint_reasons.join(" | ")
            )
        } else {
            String::new()
        };
        if let Some(error) = data.phase_error.as_deref() {
            eprintln!(
                "await: background service could not finish: {} ({} metrics, {} notes remaining{})",
                error, data.metrics_remaining, data.notes_remaining, blocked
            );
        } else {
            eprintln!(
                "await: background service could not finish ({} metrics, {} notes remaining{})",
                data.metrics_remaining, data.notes_remaining, blocked
            );
        }
        std::process::exit(1);
    }

    eprintln!("await: background service finished");
}

fn response_error_message(response: &ControlResponse) -> &str {
    response
        .error
        .as_deref()
        .unwrap_or("background service returned an error")
}

fn print_usage() {
    eprintln!("Usage: git-ai await [--timeout <seconds>]");
    eprintln!(
        "  [beta] Wait for the background service to finish all work and telemetry flushing."
    );
    eprintln!();
    eprintln!("Options:");
    eprintln!(
        "  --timeout <seconds>  Maximum time to wait (default: {})",
        DEFAULT_TIMEOUT_SECONDS
    );
    eprintln!("  -h, --help           Show this help");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn await_reports_daemon_error_message() {
        let response = ControlResponse::err("telemetry flush failed");

        assert_eq!(response_error_message(&response), "telemetry flush failed");
    }

    #[test]
    fn await_response_remains_compatible_with_daemons_without_phase_error() {
        let response: AwaitResponse = serde_json::from_value(serde_json::json!({
            "done": true,
            "timed_out": false,
            "metrics_remaining": 0,
            "notes_remaining": 0
        }))
        .unwrap();

        assert!(response.phase_error.is_none());
        assert_eq!(response.blocked_checkpoints, 0);
        assert!(response.blocked_checkpoint_reasons.is_empty());
    }
}
