const MAGIC: &[u8; 5] = b"\xffZSWP";

/// An invalid or unsupported refund recovery record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoError {
    /// Keep this record pending until its version can be interpreted.
    UnsupportedVersion(u8),
    /// Version one only stores refund indices in funding memos.
    InvalidPurpose(u8),
}

impl core::fmt::Display for MemoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => write!(f, "unsupported swap memo version {v}"),
            Self::InvalidPurpose(p) => write!(f, "invalid swap memo purpose {p}"),
        }
    }
}

impl std::error::Error for MemoError {}

/// A v1 refund index carried by a swap funding transaction.
///
/// The deposit address is not stored: recovery reads it from the funding
/// transaction's single transparent output.
///
/// Decoding does not establish provenance. Accept a record only after
/// authenticating an ordinary internal note and verifying that its transaction
/// was the wallet's own send. Process zero-value and spent notes too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefundMemo {
    index: u64,
}

impl RefundMemo {
    /// Creates a record for the given refund index.
    pub fn new(index: u64) -> Self {
        Self { index }
    }

    /// The index in the refund sequence, not a globally unique operation ID.
    pub fn index(&self) -> u64 {
        self.index
    }

    /// Encodes the fixed 512-byte binary memo, zeroing the reserved bytes.
    pub fn encode(&self) -> [u8; 512] {
        let mut bytes = [0; 512];
        bytes[..5].copy_from_slice(MAGIC);
        bytes[5] = 1;
        bytes[6] = 0;
        bytes[7..15].copy_from_slice(&self.index.to_le_bytes());
        bytes
    }

    /// Decodes a record, returning `None` only for a memo without our discriminator.
    ///
    /// Unsupported versions are errors so callers cannot silently mark their
    /// recovery work complete. Preserve those raw memos for a future decoder.
    /// Bytes after the index are reserved and ignored, so prerelease records
    /// that appended a deposit address still decode.
    pub fn decode(bytes: &[u8; 512]) -> Result<Option<Self>, MemoError> {
        if &bytes[..5] != MAGIC {
            return Ok(None);
        }
        if bytes[5] != 1 {
            return Err(MemoError::UnsupportedVersion(bytes[5]));
        }
        if bytes[6] != 0 {
            return Err(MemoError::InvalidPurpose(bytes[6]));
        }
        let index = u64::from_le_bytes(bytes[7..15].try_into().expect("eight-byte index"));
        Ok(Some(Self { index }))
    }
}
