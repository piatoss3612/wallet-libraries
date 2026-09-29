//! Shared swap completion policy. Wallet storage owns canonical-chain validation,
//! receipt attribution, and atomic persistence of this state with scan coverage.

use zcash_protocol::{consensus::BlockHeight, value::Zatoshis};

/// A block on the wallet's accepted Zcash chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainAnchor {
    /// Block height.
    pub height: BlockHeight,
    /// Block hash in the wallet's canonical byte representation.
    pub hash: [u8; 32],
}

/// Defaults are wallet conventions, not consensus parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompletionPolicy {
    /// Additional blocks to scan after observing terminal status.
    pub grace_blocks: u32,
    /// Seconds after first terminal observation before directory reconciliation.
    pub reconciliation_delay_secs: u64,
}

impl CompletionPolicy {
    /// Inclusive end of temporary trial decryption. This deadline does not depend
    /// on receipt accounting or directory availability; reconciliation can outlive it.
    pub fn scan_through(self, observed_height: BlockHeight) -> Result<BlockHeight, LifecycleError> {
        u32::from(observed_height)
            .checked_add(self.grace_blocks)
            .map(BlockHeight::from)
            .ok_or(LifecycleError::DeadlineOverflow)
    }
}

impl Default for CompletionPolicy {
    fn default() -> Self {
        Self {
            grace_blocks: 10,
            reconciliation_delay_secs: 12 * 60 * 60,
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

/// A height deadline cannot be represented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleError {
    /// Grace would exceed the representable chain height.
    DeadlineOverflow,
}
impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("swap completion deadline overflow")
    }
}
impl std::error::Error for LifecycleError {}

/// Provider outcome normalized by direction. It schedules discovery but never
/// establishes ownership, inclusion, or an on-chain amount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OperationStatus {
    /// A supported status confirms that the operation is in progress.
    Active,
    /// The provider reports completion with the route's receipt expectation.
    Terminal(ReceiptExpectation),
}

/// Normalize the supported NEAR states once. Unrecognized states preserve the
/// previous observation. A refund on the source chain is not a Zcash receipt.
pub fn near_status(purpose: crate::Purpose, status: &str) -> Option<OperationStatus> {
    use crate::Purpose;
    use OperationStatus::*;
    use ReceiptExpectation::*;
    match status {
        "KNOWN_DEPOSIT_TX" | "PENDING_DEPOSIT" | "INCOMPLETE_DEPOSIT" | "PROCESSING" => {
            Some(Active)
        }
        "SUCCESS" => Some(Terminal(if purpose == Purpose::Receive {
            Positive(std::option::Option::None)
        } else {
            None
        })),
        "REFUNDED" => Some(Terminal(if purpose == Purpose::Refund {
            Positive(std::option::Option::None)
        } else {
            None
        })),
        "FAILED" => Some(Terminal(Unknown)),
        _ => std::option::Option::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_outcomes_preserve_direction_and_unknowns() {
        use crate::Purpose::*;
        assert_eq!(
            near_status(Refund, "PROCESSING"),
            Some(OperationStatus::Active)
        );
        assert_eq!(
            near_status(Refund, "REFUNDED"),
            Some(OperationStatus::Terminal(ReceiptExpectation::Positive(
                None
            )))
        );
        assert_eq!(
            near_status(Receive, "REFUNDED"),
            Some(OperationStatus::Terminal(ReceiptExpectation::None))
        );
        assert_eq!(
            near_status(Receive, "SUCCESS"),
            Some(OperationStatus::Terminal(ReceiptExpectation::Positive(
                None
            )))
        );
        assert_eq!(
            near_status(Refund, "FAILED"),
            Some(OperationStatus::Terminal(ReceiptExpectation::Unknown))
        );
        assert_eq!(near_status(Refund, "NEW_STATE"), None);
        assert!(
            CompletionPolicy::default()
                .scan_through(u32::MAX.into())
                .is_err()
        );
    }
}
