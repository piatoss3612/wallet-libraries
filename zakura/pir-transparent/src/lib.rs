//! Bounded transparent PIR retrieval. Candidate evidence never grants financial authority.
#[cfg(feature = "wallet")]
mod recovery;
#[cfg(feature = "wallet")]
pub use recovery::*;
