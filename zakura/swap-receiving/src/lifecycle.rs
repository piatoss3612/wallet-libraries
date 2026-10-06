//! Shared swap completion policy. Wallet storage owns canonical-chain validation,
//! receipt attribution, and atomic persistence of this state with scanning.

use zcash_protocol::value::Zatoshis;

/// When a key's trial decryption may stop. Defaults are wallet conventions, not
/// consensus parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletionPolicy {
    /// Seconds an incoming key found only by a restore sweep keeps scanning, for a
    /// payout from a swap in flight at restore.
    pub restore_watch_secs: i64,
    /// Seconds after the quote deadline, or after registration when no deadline
    /// is known, after which scanning stops whatever the provider reports.
    pub limit_secs: i64,
}

impl Default for CompletionPolicy {
    fn default() -> Self {
        Self {
            restore_watch_secs: 24 * 60 * 60,
            limit_secs: 30 * 24 * 60 * 60,
        }
    }
}

/// The route adapter's interpretation of the expected Zcash receipt.
/// Incoming source-chain refunds must not be interpreted as Zcash refunds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptExpectation {
    /// The supported route explicitly establishes that no Zcash receipt is expected.
    None,
    /// A positive receipt is expected. `None` means its amount is unavailable.
    Positive(Option<Zatoshis>),
    /// Receipt details are missing, malformed, or otherwise inconclusive.
    Unknown,
}

/// Provider outcome normalized by direction. It schedules scanning but never
/// establishes ownership, inclusion, or an on-chain amount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationStatus {
    /// A supported status confirms that the operation is in progress.
    Active,
    /// The provider reports completion with the route's receipt expectation.
    Terminal(ReceiptExpectation),
}

/// The fields of a NEAR 1Click status response that affect scanning.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProviderStatus<'a> {
    /// The `status` string.
    pub status: &'a str,
    /// The quote request's `swapType`, such as `EXACT_OUTPUT`.
    pub swap_type: Option<&'a str>,
    /// `swapDetails.refundedAmount` in the origin asset's base units.
    pub refunded_amount: Option<Zatoshis>,
    /// `swapDetails.amountOut` in the destination asset's base units.
    pub amount_out: Option<Zatoshis>,
    /// The quote deadline as a Unix timestamp.
    pub deadline: Option<i64>,
}

/// A normalized provider observation and the deadline that bounds scanning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The direction-specific outcome.
    pub status: OperationStatus,
    /// The quote deadline, if the response carried one.
    pub deadline: Option<i64>,
}

/// Normalizes a NEAR status response for a key of `purpose`. Unrecognized
/// statuses return `None`, preserving the previous observation.
///
/// A refund key expects ZEC whenever the provider reports a positive refunded
/// amount, and after an exact-output `SUCCESS`, which returns unused input once
/// the swap completes. A refund on the source chain is not a Zcash receipt. A
/// zero payout amount is treated as unreported.
pub fn near_observation(
    purpose: crate::Purpose,
    status: &ProviderStatus<'_>,
) -> Option<Observation> {
    use crate::Purpose;
    use OperationStatus::*;
    use ReceiptExpectation::{Positive, Unknown};
    let refund = status.refunded_amount.filter(|v| !v.is_zero());
    let outcome = match (status.status, purpose) {
        ("KNOWN_DEPOSIT_TX" | "PENDING_DEPOSIT" | "INCOMPLETE_DEPOSIT" | "PROCESSING", _) => Active,
        ("SUCCESS", Purpose::Receive) => {
            Terminal(Positive(status.amount_out.filter(|v| !v.is_zero())))
        }
        ("SUCCESS", Purpose::Refund) => Terminal(match refund {
            Some(value) => Positive(Some(value)),
            None if status.swap_type == Some("EXACT_OUTPUT") => Positive(None),
            None => ReceiptExpectation::None,
        }),
        ("REFUNDED", Purpose::Refund) => Terminal(Positive(refund)),
        ("REFUNDED", Purpose::Receive) => Terminal(ReceiptExpectation::None),
        ("FAILED", _) => Terminal(Unknown),
        _ => return None,
    };
    Some(Observation {
        status: outcome,
        deadline: status.deadline,
    })
}

/// Reads a NEAR 1Click `/v0/status` response body for a key of `purpose` with
/// [`near_observation`]. Returns `None` for a malformed body or an unrecognized
/// status. A deadline without a zone designator is taken as UTC, as 1Click reports.
pub fn near_status_observation(purpose: crate::Purpose, body: &[u8]) -> Option<Observation> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let text = |pointer: &str| value.pointer(pointer).and_then(|v| v.as_str());
    let amount = |pointer: &str| {
        text(pointer)
            .and_then(|v| v.parse::<u64>().ok())
            .and_then(|v| Zatoshis::from_u64(v).ok())
    };
    near_observation(
        purpose,
        &ProviderStatus {
            status: text("/status")?,
            swap_type: text("/quoteResponse/quoteRequest/swapType"),
            refunded_amount: amount("/swapDetails/refundedAmount"),
            amount_out: amount("/swapDetails/amountOut"),
            deadline: text("/quoteResponse/quoteRequest/deadline").and_then(unix_seconds),
        },
    )
}

/// Unix seconds of an RFC 3339 timestamp, or of one without a zone designator taken as UTC.
fn unix_seconds(text: &str) -> Option<i64> {
    use time::{
        OffsetDateTime, PrimitiveDateTime,
        format_description::well_known::{Iso8601, Rfc3339},
    };
    OffsetDateTime::parse(text, &Rfc3339)
        .ok()
        .or_else(|| {
            PrimitiveDateTime::parse(text, &Iso8601::DEFAULT)
                .ok()
                .map(PrimitiveDateTime::assume_utc)
        })
        .map(OffsetDateTime::unix_timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Purpose::*;

    /// The normalized status of `status` for a key of `purpose`.
    fn observe(purpose: crate::Purpose, status: ProviderStatus<'_>) -> Option<OperationStatus> {
        near_observation(purpose, &status).map(|o| o.status)
    }

    #[test]
    fn provider_outcomes_preserve_direction_and_unknowns() {
        let status = |status| ProviderStatus {
            status,
            ..Default::default()
        };
        let positive = |v| Some(OperationStatus::Terminal(ReceiptExpectation::Positive(v)));
        assert_eq!(
            observe(Refund, status("PROCESSING")),
            Some(OperationStatus::Active)
        );
        assert_eq!(observe(Refund, status("REFUNDED")), positive(None));
        assert_eq!(
            observe(Receive, status("REFUNDED")),
            Some(OperationStatus::Terminal(ReceiptExpectation::None))
        );
        assert_eq!(observe(Receive, status("SUCCESS")), positive(None));
        assert_eq!(
            observe(Refund, status("SUCCESS")),
            Some(OperationStatus::Terminal(ReceiptExpectation::None))
        );
        assert_eq!(
            observe(Refund, status("FAILED")),
            Some(OperationStatus::Terminal(ReceiptExpectation::Unknown))
        );
        assert_eq!(observe(Refund, status("NEW_STATE")), None);
    }

    #[test]
    fn amounts_and_exact_output_set_refund_expectations() {
        let amount = Zatoshis::const_from_u64(1_000);
        let positive = |v| Some(OperationStatus::Terminal(ReceiptExpectation::Positive(v)));
        // Excess deposits and exact-output leftovers come back after SUCCESS.
        let refunded = ProviderStatus {
            status: "SUCCESS",
            refunded_amount: Some(amount),
            ..Default::default()
        };
        assert_eq!(observe(Refund, refunded), positive(Some(amount)));
        let exact_output = ProviderStatus {
            status: "SUCCESS",
            swap_type: Some("EXACT_OUTPUT"),
            ..Default::default()
        };
        assert_eq!(observe(Refund, exact_output), positive(None));
        let zero = ProviderStatus {
            status: "SUCCESS",
            refunded_amount: Some(Zatoshis::ZERO),
            ..Default::default()
        };
        assert_eq!(
            observe(Refund, zero),
            Some(OperationStatus::Terminal(ReceiptExpectation::None))
        );
        // A source-chain refund on an incoming swap is never a Zcash receipt.
        let incoming_refund = ProviderStatus {
            status: "REFUNDED",
            refunded_amount: Some(amount),
            ..Default::default()
        };
        assert_eq!(
            observe(Receive, incoming_refund),
            Some(OperationStatus::Terminal(ReceiptExpectation::None))
        );
        let zero_payout = ProviderStatus {
            status: "SUCCESS",
            amount_out: Some(Zatoshis::ZERO),
            ..Default::default()
        };
        assert_eq!(observe(Receive, zero_payout), positive(None));
        let payout = ProviderStatus {
            status: "SUCCESS",
            amount_out: Some(amount),
            deadline: Some(42),
            ..Default::default()
        };
        assert_eq!(
            near_observation(Receive, &payout),
            Some(Observation {
                status: OperationStatus::Terminal(ReceiptExpectation::Positive(Some(amount))),
                deadline: Some(42),
            })
        );
    }

    #[test]
    fn status_responses_yield_expectations_and_utc_deadlines() {
        let body = br#"{"status":"SUCCESS","swapDetails":{"refundedAmount":"1500"},
            "quoteResponse":{"quoteRequest":{"swapType":"EXACT_INPUT",
            "deadline":"2026-09-01T12:00:00Z"}}}"#;
        let refund = near_status_observation(Refund, body).unwrap();
        assert_eq!(
            refund.status,
            OperationStatus::Terminal(ReceiptExpectation::Positive(Some(
                Zatoshis::const_from_u64(1500)
            )))
        );
        assert_eq!(refund.deadline, Some(1_788_264_000));
        let zoneless = br#"{"status":"PENDING_DEPOSIT",
            "quoteResponse":{"quoteRequest":{"deadline":"2026-09-01T12:00:00"}}}"#;
        let pending = near_status_observation(Receive, zoneless).unwrap();
        assert_eq!(pending.status, OperationStatus::Active);
        assert_eq!(pending.deadline, Some(1_788_264_000));
        let payout = br#"{"status":"SUCCESS","swapDetails":{"amountOut":"70000"}}"#;
        assert_eq!(
            near_status_observation(Receive, payout).unwrap().status,
            OperationStatus::Terminal(ReceiptExpectation::Positive(Some(
                Zatoshis::const_from_u64(70_000)
            )))
        );
        for unusable in [&br#"{"status":"NEW_STATE"}"#[..], b"not json", b"{}"] {
            assert_eq!(near_status_observation(Refund, unusable), None);
        }
    }
}
