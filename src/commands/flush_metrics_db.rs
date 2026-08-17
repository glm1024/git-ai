//! Handle flush-metrics-db command (kept for manual human use).
//!
//! Uploads pending metrics database rows to the API.

use crate::api::{
    ApiClient, ApiContext, metrics_upload_allowed, metrics_upload_error_is_permanent,
    upload_metrics_with_retry,
};
use crate::metrics::db::MetricsDatabase;
use crate::metrics::{MetricEvent, MetricsBatch};

/// Max events per batch upload
const MAX_BATCH_SIZE: usize = 1000;

fn with_locked_state<State, Value, Error>(
    state: &std::sync::Mutex<State>,
    context: &str,
    operation: impl FnOnce(&mut State) -> Result<Value, Error>,
) -> Result<Value, String>
where
    Error: std::fmt::Display,
{
    let mut state = state.lock().map_err(|error| {
        format!("{context}: failed to acquire local metrics database lock: {error}")
    })?;
    operation(&mut state).map_err(|error| format!("{context}: {error}"))
}

/// Handle the flush-metrics-db command
pub fn handle_flush_metrics_db(_args: &[String]) -> Result<(), String> {
    let context = ApiContext::for_metrics();
    let client = ApiClient::new(context);

    if !metrics_upload_allowed(&client) {
        eprintln!("flush-metrics-db: skipping (requires an API key or login)");
        return Ok(());
    }

    // Get database connection
    let db = MetricsDatabase::global()
        .map_err(|error| format!("failed to open metrics database: {error}"))?;
    with_locked_state(
        db,
        "failed to repair compact token buckets before upload",
        |db_lock| db_lock.repair_daily_token_buckets(),
    )?;

    let mut total_uploaded = 0usize;
    let mut total_batches = 0usize;
    let mut total_invalid = 0usize;
    let mut total_undeliverable = 0usize;

    loop {
        // Get batch from DB
        let batch = with_locked_state(db, "failed to read pending batch", |db_lock| {
            db_lock.dequeue_pending_batch(MAX_BATCH_SIZE)
        })?;

        // If batch is empty, we're done
        if batch.is_empty() {
            break;
        }

        // Parse events and build MetricsBatch
        let mut events = Vec::new();
        let mut record_ids = Vec::new();
        let mut invalid_records = Vec::new();

        for record in &batch {
            match serde_json::from_str::<MetricEvent>(&record.event_json) {
                Ok(event) => {
                    events.push(event);
                    record_ids.push(record.id);
                }
                Err(error) => {
                    invalid_records
                        .push((record.id, format!("invalid local metric JSON: {error}")));
                }
            }
        }

        if !invalid_records.is_empty() {
            with_locked_state(
                db,
                "failed to retain invalid local metrics as undeliverable",
                |db_lock| db_lock.mark_records_undeliverable(&invalid_records, current_unix_ts()),
            )?;
            total_invalid += invalid_records.len();
        }

        if events.is_empty() {
            continue;
        }

        let event_count = events.len();
        let metrics_batch = MetricsBatch::new(events);

        // Upload with the HTTP helper's short retry, then persist DB backoff on failure.
        match upload_metrics_with_retry(&client, &metrics_batch, "flush_metrics_db") {
            Ok(response) => {
                if let Err(e) = response.validate_error_indices(record_ids.len()) {
                    eprintln!(
                        "  ✗ batch upload response invalid ({} events kept for retry): {}",
                        event_count, e
                    );
                    let error = e.to_string();
                    with_locked_state(
                        db,
                        "failed to defer metrics after an invalid upload response",
                        |db_lock| {
                            db_lock.mark_records_deferred(&record_ids, &error, current_unix_ts())
                        },
                    )?;
                    return Err(format!(
                        "batch upload response invalid ({event_count} events kept for retry): {e}"
                    ));
                }

                let successful_ids: Vec<i64> = response
                    .successful_indices(record_ids.len())
                    .into_iter()
                    .map(|index| record_ids[index])
                    .collect();
                let undeliverable_records: Vec<(i64, String)> = response
                    .errors
                    .iter()
                    .map(|error| (record_ids[error.index], error.error.clone()))
                    .collect();

                // The remote ACK is only a successful flush after its matching
                // local state transition is durable. Propagate lock/SQLite
                // failures so callers cannot report success while rows remain
                // processing or are uploaded again.
                with_locked_state(db, "failed to persist upload receipt", |db_lock| {
                    let now = current_unix_ts();
                    db_lock.mark_records_delivered(&successful_ids, now)?;
                    db_lock.mark_records_undeliverable(&undeliverable_records, now)
                })?;

                total_uploaded += successful_ids.len();
                total_batches += 1;
                total_undeliverable += undeliverable_records.len();
                eprintln!(
                    "  ✓ batch {} - uploaded {} events{}",
                    total_batches,
                    successful_ids.len(),
                    if undeliverable_records.is_empty() {
                        String::new()
                    } else {
                        format!(" ({} marked undeliverable)", undeliverable_records.len())
                    }
                );
            }
            Err(e) => {
                let permanent = metrics_upload_error_is_permanent(&e);
                if permanent {
                    eprintln!(
                        "  ✗ batch upload was permanently rejected ({} events retained locally without retry): {}",
                        event_count, e
                    );
                } else {
                    eprintln!(
                        "  ✗ batch upload failed ({} events kept for retry): {}",
                        event_count, e
                    );
                }
                let error = e.to_string();
                with_locked_state(
                    db,
                    if permanent {
                        "failed to retain permanently rejected metrics"
                    } else {
                        "failed to defer metrics after upload failure"
                    },
                    |db_lock| {
                        let now = current_unix_ts();
                        if permanent {
                            let records = record_ids
                                .iter()
                                .map(|id| (*id, error.clone()))
                                .collect::<Vec<_>>();
                            db_lock.mark_records_undeliverable(&records, now)
                        } else {
                            db_lock.mark_records_deferred(&record_ids, &error, now)
                        }
                    },
                )?;
                return Err(if permanent {
                    format!(
                        "batch upload was permanently rejected ({event_count} events retained locally without retry): {e}"
                    )
                } else {
                    format!("batch upload failed ({event_count} events kept for retry): {e}")
                });
            }
        }
    }

    if total_invalid > 0 {
        eprintln!(
            "flush-metrics-db: retained {} invalid record(s) as undeliverable",
            total_invalid
        );
    }
    if total_undeliverable > 0 {
        eprintln!(
            "flush-metrics-db: marked {} server-rejected record(s) undeliverable",
            total_undeliverable
        );
    }

    eprintln!(
        "flush-metrics-db: uploaded {} events in {} batch(es)",
        total_uploaded, total_batches
    );
    Ok(())
}

fn current_unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::with_locked_state;
    use std::sync::{Arc, Mutex};

    #[test]
    fn locked_state_propagates_update_errors() {
        let state = Mutex::new(0_u8);

        let error = with_locked_state(&state, "persist upload receipt", |_state| {
            Err::<(), _>("sqlite write failed")
        })
        .expect_err("a failed local ACK must fail the flush");

        assert!(error.contains("persist upload receipt"));
        assert!(error.contains("sqlite write failed"));
    }

    #[test]
    fn locked_state_propagates_poisoned_lock_errors() {
        let state = Arc::new(Mutex::new(0_u8));
        let thread_state = Arc::clone(&state);
        let _ = std::thread::spawn(move || {
            let _guard = thread_state.lock().unwrap();
            panic!("poison test lock");
        })
        .join();

        let error = with_locked_state(
            &state,
            "persist upload receipt",
            |_state| Ok::<(), &str>(()),
        )
        .expect_err("a poisoned DB lock must fail the flush");

        assert!(error.contains("persist upload receipt"));
        assert!(error.contains("lock"));
    }
}
