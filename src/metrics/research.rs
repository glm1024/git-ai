//! Strict research-v1 metrics receipt contract.
//!
//! This module is intentionally independent from the legacy v1 metrics wire
//! format. Research outbox items are sticky: once created for this contract,
//! they may only be settled by a fully validated research-v1 acknowledgement.

use crate::error::GitAiError;
use serde::{Deserialize, Serialize};

pub(crate) const RESEARCH_RECEIPT_CONTRACT: &str = "research-v1";
pub(crate) const RESEARCH_RECEIPT_SCHEMA: &str = "git-ai-metrics-receipt/1";
pub(crate) const RESEARCH_WIRE_VERSION: u8 = 2;
pub(crate) const UNSUPPORTED_NOT_RETAINED: &str = "UNSUPPORTED_NOT_RETAINED";
pub(crate) const PAYLOAD_CONFLICT: &str = "PAYLOAD_CONFLICT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResearchItemInput {
    pub outbox_item_id: String,
    pub event_key: String,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResearchEnvelopeItem {
    pub outbox_item_id: String,
    pub event_key: String,
    pub payload_sha256: String,
    pub payload_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedResearchUpload {
    pub client_instance_id: String,
    pub batch_attempt_id: String,
    pub item_count: usize,
    pub ordered_item_key_sha256: String,
    pub body: String,
    pub payload_sha256: String,
    pub request_byte_count: usize,
    pub items: Vec<ResearchEnvelopeItem>,
}

pub(crate) fn prepare_research_upload(
    _client_instance_id: &str,
    _batch_attempt_id: &str,
    _inputs: Vec<ResearchItemInput>,
) -> Result<PreparedResearchUpload, GitAiError> {
    Err(GitAiError::Generic(
        "research-v1 envelope preparation is not implemented".to_string(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResearchItemStatus {
    Accepted,
    IdempotentDuplicate,
    Rejected,
    PayloadConflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedResearchItemReceipt {
    pub index: usize,
    pub outbox_item_id: String,
    pub item_receipt_id: String,
    pub event_key: String,
    pub payload_sha256: String,
    pub status: ResearchItemStatus,
    pub raw_record_id: Option<String>,
    pub raw_persisted_at: Option<String>,
    pub error_code: Option<String>,
    pub projection_status: Option<String>,
    pub projection_version: Option<String>,
    pub projected_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedResearchAck {
    pub server_receipt_id: String,
    pub server_received_at: String,
    pub raw_persisted_at: String,
    pub accepted_at: String,
    pub items: Vec<ValidatedResearchItemReceipt>,
}

pub(crate) fn parse_and_validate_research_ack(
    _response_body: &str,
    _prepared: &PreparedResearchUpload,
) -> Result<ValidatedResearchAck, GitAiError> {
    Err(GitAiError::Generic(
        "research-v1 acknowledgement validation is not implemented".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn item(id: &str, key: &str, timestamp: u32) -> ResearchItemInput {
        ResearchItemInput {
            outbox_item_id: id.to_string(),
            event_key: key.to_string(),
            payload_json: format!(r#"{{"t":{timestamp},"e":1,"v":{{}},"a":{{}}}}"#),
        }
    }

    #[test]
    fn v2_envelope_binds_exact_payload_and_ordered_event_keys() {
        let prepared = prepare_research_upload(
            "client-1",
            "attempt-1",
            vec![item("outbox-1", "event-a", 1), item("outbox-2", "event-b", 2)],
        )
        .expect("prepare strict research envelope");

        let expected_ordered =
            format!("{:x}", Sha256::digest(br#"["event-a","event-b"]"#));
        assert_eq!(prepared.ordered_item_key_sha256, expected_ordered);
        assert_eq!(prepared.item_count, 2);
        assert_eq!(prepared.request_byte_count, prepared.body.len());
        assert_eq!(
            prepared.payload_sha256,
            format!("{:x}", Sha256::digest(prepared.body.as_bytes()))
        );
        assert_eq!(
            prepared.items[0].payload_sha256,
            format!(
                "{:x}",
                Sha256::digest(prepared.items[0].payload_json.as_bytes())
            )
        );

        let body: serde_json::Value = serde_json::from_str(&prepared.body).unwrap();
        assert_eq!(body["v"], 2);
        assert_eq!(body["clientInstanceId"], "client-1");
        assert_eq!(body["batchAttemptId"], "attempt-1");
        assert_eq!(body["itemCount"], 2);
        assert_eq!(body["items"][0]["payloadJson"], prepared.items[0].payload_json);
    }

    #[test]
    fn v2_envelope_rejects_duplicate_event_keys_and_invalid_legacy_payload() {
        let duplicate = prepare_research_upload(
            "client-1",
            "attempt-1",
            vec![item("outbox-1", "same", 1), item("outbox-2", "same", 2)],
        )
        .expect_err("duplicate event keys must fail closed");
        assert!(duplicate.to_string().contains("duplicate eventKey"));

        let invalid = prepare_research_upload(
            "client-1",
            "attempt-1",
            vec![ResearchItemInput {
                outbox_item_id: "outbox-1".to_string(),
                event_key: "event-a".to_string(),
                payload_json: "{} trailing".to_string(),
            }],
        )
        .expect_err("payloadJson must be an exact legacy MetricEvent");
        assert!(invalid.to_string().contains("payloadJson"));
    }

    #[test]
    fn strict_ack_validates_complete_partial_item_set() {
        let prepared = prepare_research_upload(
            "client-1",
            "attempt-1",
            vec![item("outbox-1", "event-a", 1), item("outbox-2", "event-b", 2)],
        )
        .unwrap();
        let response = valid_ack_json(
            &prepared,
            serde_json::json!([
                item_receipt(&prepared, 0, "accepted", Some("raw-1"), None),
                item_receipt(
                    &prepared,
                    1,
                    "rejected",
                    None,
                    Some(UNSUPPORTED_NOT_RETAINED)
                )
            ]),
            serde_json::json!([{"index": 1, "error": UNSUPPORTED_NOT_RETAINED}]),
        );

        let ack = parse_and_validate_research_ack(&response.to_string(), &prepared)
            .expect("complete partial ACK is valid");
        assert_eq!(ack.items.len(), 2);
        assert_eq!(ack.items[0].status, ResearchItemStatus::Accepted);
        assert_eq!(ack.items[1].status, ResearchItemStatus::Rejected);
    }

    #[test]
    fn strict_ack_rejects_duplicate_missing_and_out_of_range_indices_atomically() {
        let prepared = prepare_research_upload(
            "client-1",
            "attempt-1",
            vec![item("outbox-1", "event-a", 1), item("outbox-2", "event-b", 2)],
        )
        .unwrap();

        for item_receipts in [
            serde_json::json!([
                item_receipt(&prepared, 0, "accepted", Some("raw-1"), None),
                item_receipt(&prepared, 0, "accepted", Some("raw-1"), None)
            ]),
            serde_json::json!([item_receipt(
                &prepared,
                0,
                "accepted",
                Some("raw-1"),
                None
            )]),
            serde_json::json!([
                item_receipt(&prepared, 0, "accepted", Some("raw-1"), None),
                {
                    "index": 2,
                    "itemReceiptId": "item-receipt-2",
                    "eventKey": "event-b",
                    "payloadSha256": prepared.items[1].payload_sha256,
                    "status": "accepted",
                    "rawRecordId": "raw-2",
                    "rawPersistedAt": "2026-07-28T00:00:01Z",
                    "errorCode": null,
                    "projectionStatus": "pending",
                    "projectionVersion": null,
                    "projectedAt": null
                }
            ]),
        ] {
            let response = valid_ack_json(&prepared, item_receipts, serde_json::json!([]));
            let error = parse_and_validate_research_ack(&response.to_string(), &prepared)
                .expect_err("invalid index set must fail the whole ACK");
            assert!(error.to_string().contains("itemReceipts index set"));
        }
    }

    #[test]
    fn strict_ack_error_indices_must_exactly_equal_rejected_or_conflict_items() {
        let prepared =
            prepare_research_upload("client-1", "attempt-1", vec![item("o", "event-a", 1)])
                .unwrap();
        let rejected = serde_json::json!([item_receipt(
            &prepared,
            0,
            "rejected",
            None,
            Some(UNSUPPORTED_NOT_RETAINED)
        )]);

        for errors in [
            serde_json::json!([]),
            serde_json::json!([
                {"index": 0, "error": UNSUPPORTED_NOT_RETAINED},
                {"index": 0, "error": UNSUPPORTED_NOT_RETAINED}
            ]),
        ] {
            let response = valid_ack_json(&prepared, rejected.clone(), errors);
            let error = parse_and_validate_research_ack(&response.to_string(), &prepared)
                .expect_err("errors must exactly match terminal rejected items");
            assert!(error.to_string().contains("errors index set"));
        }
    }

    fn item_receipt(
        prepared: &PreparedResearchUpload,
        index: usize,
        status: &str,
        raw_record_id: Option<&str>,
        error_code: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "index": index,
            "itemReceiptId": format!("item-receipt-{index}"),
            "eventKey": prepared.items[index].event_key,
            "payloadSha256": prepared.items[index].payload_sha256,
            "status": status,
            "rawRecordId": raw_record_id,
            "rawPersistedAt": raw_record_id.map(|_| "2026-07-28T00:00:01Z"),
            "errorCode": error_code,
            "projectionStatus": if raw_record_id.is_some() { "pending" } else { "not_applicable" },
            "projectionVersion": null,
            "projectedAt": null
        })
    }

    fn valid_ack_json(
        prepared: &PreparedResearchUpload,
        item_receipts: serde_json::Value,
        errors: serde_json::Value,
    ) -> serde_json::Value {
        let partial_error_count = errors.as_array().map(Vec::len).unwrap_or_default();
        serde_json::json!({
            "accepted": true,
            "kind": "git_ai_metrics",
            "itemCount": prepared.item_count,
            "payloadSha256": prepared.payload_sha256,
            "errors": errors,
            "receipt": {
                "schemaVersion": RESEARCH_RECEIPT_SCHEMA,
                "serverReceiptId": "server-receipt-1",
                "clientInstanceId": prepared.client_instance_id,
                "batchAttemptId": prepared.batch_attempt_id,
                "requestByteCount": prepared.request_byte_count,
                "orderedItemKeySha256": prepared.ordered_item_key_sha256,
                "serverReceivedAt": "2026-07-28T00:00:00Z",
                "rawPersistedAt": "2026-07-28T00:00:01Z",
                "acceptedAt": "2026-07-28T00:00:02Z",
                "partialErrorCount": partial_error_count
            },
            "itemReceipts": item_receipts
        })
    }
}
