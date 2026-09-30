//! Connection capability used by persistent guards on wallet chain and ownership writes.
//!
//! An old connection cannot execute a guarded statement because it does not register this
//! function. This is a compatibility barrier, not authorization against arbitrary SQL clients.

/// Register before migrations and transactional writes, including supplied connections.
/// Registration is fallible; failure leaves guarded writes refused rather than bypassed.
pub(crate) fn register(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "tpir_writer_version",
        0,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_INNOCUOUS,
        |_| Ok(super::TPIR_READER_VERSION),
    )
}
