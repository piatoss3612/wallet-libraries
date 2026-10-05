//! Bounded transparent PIR retrieval. Candidate evidence never grants financial authority.
#[cfg(feature = "wallet")]
mod recovery;
#[cfg(feature = "wallet")]
pub use recovery::{
    Outcome, Progress, RecoveryBatch, RecoveryConfig, RecoveryError, ReferenceRecovery, SCHEMA,
};
// What a caller implements for `recover`: the chain view, and transports that reach
// the companion's origin and map service refusals.
#[cfg(feature = "wallet")]
pub use transparent_wallet::ChainView;
#[cfg(feature = "wallet")]
pub use transparent_wallet::client::Table;
#[cfg(feature = "wallet")]
pub use transparent_wallet::transport::{
    BoxError, FilterSource, Overloaded, ShardRequest, ShardTransport, StaleRevision, refusal,
};
