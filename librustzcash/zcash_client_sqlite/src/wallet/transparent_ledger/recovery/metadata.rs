//! Atomic, source-bound transaction assertions. Absence is never an asserted zero.
use super::*;
use zcash_client_backend::data_api::transparent_ledger::{
    TransactionMetadata, WholeTransactionFee,
};

pub(super) fn apply_metadata(
    conn: &rusqlite::Connection,
    account: AccountRef,
    revision: i64,
    txid: TxId,
    mined_height: BlockHeight,
    metadata: Option<TransactionMetadata>,
) -> Result<(), SqliteClientError> {
    let Some(metadata) = metadata else {
        return Ok(());
    };
    super::super::require_reader_version(conn, super::super::METADATA_READER_VERSION)?;
    let (fee_state, fee) = match metadata.fee {
        WholeTransactionFee::Exact(value) => (0, Some(value.into_u64() as i64)),
        WholeTransactionFee::Unknown => (1, None),
        WholeTransactionFee::NotApplicable => (2, None),
    };
    let params = named_params![":account": account.0, ":revision": revision, ":txid": txid.as_ref(),
        ":height": u32::from(mined_height), ":fee_state": fee_state, ":fee": fee,
        ":inputs": metadata.transparent_input_count, ":shielded": metadata.has_shielded_components];
    let conflicting: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM tpir_transaction_metadata WHERE txid = :txid
         AND (mined_height != :height OR fee_state != :fee_state OR fee_zat IS NOT :fee
              OR input_count != :inputs OR shielded != :shielded))",
        named_params![":txid": txid.as_ref(), ":height": u32::from(mined_height),
            ":fee_state": fee_state, ":fee": fee, ":inputs": metadata.transparent_input_count,
            ":shielded": metadata.has_shielded_components],
        |row| row.get(0),
    )?;
    if conflicting {
        return Err(reject(CommitRejection::Integrity(
            IntegrityFailure::TransactionMetadata(txid),
        )));
    }
    conn.execute(
        "INSERT INTO tpir_transaction_metadata
        (account_id, txid, revision_id, mined_height, fee_state, fee_zat, input_count, shielded)
        VALUES (:account, :txid, :revision, :height, :fee_state, :fee, :inputs, :shielded)
        ON CONFLICT DO NOTHING",
        params,
    )?;
    Ok(())
}
