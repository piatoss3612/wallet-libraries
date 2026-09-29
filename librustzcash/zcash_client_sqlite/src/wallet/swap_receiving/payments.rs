//! Durable ownership-checked candidates. These rows never contribute to balance.
use super::{Error, KeyId, account_key, corrupt, purpose_code};
use crate::{AccountUuid, SqlTransaction, WalletDb, wallet};
use rusqlite::{Connection, OptionalExtension, params};
use std::borrow::{Borrow, BorrowMut};
use zakura_swap_receiving::{lifecycle::ChainAnchor, recovery::EncryptedNote};
use zcash_primitives::{block::BlockHash, transaction::TxId};
use zcash_protocol::{
    PoolType,
    consensus::{BlockHeight, Parameters},
};

/// A privately retrieved output awaiting chain inclusion and spendability checks.
/// Transaction metadata remains an indexer assertion until independently checked.
#[derive(Clone, PartialEq, Eq)]
pub struct PendingPayment {
    /// Transaction hash in protocol byte order.
    pub txid: TxId,
    /// Original Action index.
    pub action_index: u32,
    /// Claimed mined height.
    pub height: BlockHeight,
    /// Claimed canonical block hash.
    pub block_hash: BlockHash,
    /// Original transaction index in that block.
    pub tx_index: u16,
    /// Global Ironwood commitment position.
    pub position: u32,
    /// Full incoming encrypted-note context, without service fee or expiry assertions.
    pub encrypted_note: EncryptedNote,
}

/// Local spend evidence as of one independently checked scan anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpendStatus {
    /// Scan coverage is missing, pruned, or the requested anchor is not yet scanned.
    Unknown,
    /// Every block retains its unlinked nullifiers, and no recorded wallet spend matches.
    Unspent,
    /// A retained canonical transaction reveals the locally derived nullifier.
    Spent {
        /// Spending transaction ID.
        txid: TxId,
        /// Spending block height.
        height: BlockHeight,
    },
}

pub(super) fn key_ref(conn: &Connection, account: AccountUuid, key: KeyId) -> Result<i64, Error> {
    conn.query_row(
        "SELECT k.id FROM ironwood_receiving_keys k JOIN accounts a ON a.id=k.account_id
        WHERE a.uuid=?1 AND k.purpose=?2 AND k.derivation_version=1 AND k.key_index=?3",
        params![
            account.0,
            purpose_code(key.purpose()),
            key.index().to_be_bytes()
        ],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| corrupt("unregistered swap recovery key"))
}
pub(super) fn authenticate<P: Parameters>(
    conn: &Connection,
    parameters: &P,
    account: AccountUuid,
    key: KeyId,
    candidate: &PendingPayment,
) -> Result<(i64, zakura_swap_receiving::recovery::RecoveredNote), Error> {
    let id = key_ref(conn, account, key)?;
    let (_, parent) = account_key(conn, parameters, account)?;
    let note = candidate
        .encrypted_note
        .decrypt(&parent, key)
        .map_err(|_| corrupt("swap note authentication failed"))?;
    let receiver: Vec<u8> = conn.query_row(
        "SELECT receiver FROM ironwood_receiving_keys WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    if receiver != note.note().recipient().to_raw_address_bytes() {
        return Err(corrupt("stored swap receiver does not match note"));
    }
    Ok((id, note))
}
fn payment(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingPayment> {
    let bytes: Vec<u8> = row.get(6)?;
    let bytes = bytes
        .try_into()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    Ok(PendingPayment {
        txid: TxId::from_bytes(row.get(0)?),
        action_index: row.get(1)?,
        height: BlockHeight::from(row.get::<_, u32>(2)?),
        block_hash: BlockHash(row.get(3)?),
        tx_index: row.get(4)?,
        position: row.get(5)?,
        encrypted_note: EncryptedNote::from_bytes(bytes),
    })
}
impl<C: Borrow<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Reload pending work for one registered key. Loading does not credit or complete it.
    pub fn pending_swap_payments(
        &self,
        account: AccountUuid,
        key: KeyId,
    ) -> Result<Vec<PendingPayment>, Error> {
        let conn = self.conn.borrow();
        let id = key_ref(conn, account, key)?;
        let mut stmt = conn.prepare(
            "SELECT txid,action_index,height,block_hash,tx_index,position,encrypted_note
            FROM ironwood_swap_payment_recovery WHERE receiving_key_id=?1 ORDER BY position",
        )?;
        Ok(stmt.query_map([id], payment)?.collect::<Result<_, _>>()?)
    }
}
impl<C: BorrowMut<Connection>, P: Parameters, CL, R> WalletDb<C, P, CL, R> {
    /// Checks local spend evidence in one database snapshot. This result is bound to
    /// `through`; recheck it in the transaction that applies a recovered note.
    pub fn swap_payment_spend_status(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        candidate: &PendingPayment,
        through: ChainAnchor,
    ) -> Result<SpendStatus, Error> {
        self.transactionally(|db| db.swap_payment_spend_status(account, key, candidate, through))
    }

    /// Authenticates and persists one candidate atomically. It does not change balance,
    /// scan coverage, allocation, or completion state. Retries are idempotent.
    pub fn queue_swap_payment(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        candidate: &PendingPayment,
    ) -> Result<(), Error> {
        self.transactionally(|db| db.queue_swap_payment(account, key, candidate))
    }
}
impl<P: Parameters, CL, R> WalletDb<SqlTransaction<'_>, P, CL, R> {
    /// Checks retained nullifiers without accepting a service's claim of absence.
    /// The caller must separately authenticate inclusion and position before crediting a note.
    pub fn swap_payment_spend_status(
        &self,
        account: AccountUuid,
        key: KeyId,
        candidate: &PendingPayment,
        through: ChainAnchor,
    ) -> Result<SpendStatus, Error> {
        let conn = self.conn.0;
        let (_, note) = authenticate(conn, &self.params, account, key, candidate)?;
        if through.height < candidate.height
            || wallet::fully_scanned_height(conn)?.is_none_or(|h| h < through.height)
        {
            return Ok(SpendStatus::Unknown);
        }
        if wallet::get_block_hash(conn, through.height)? != Some(BlockHash(through.hash))
            || wallet::get_block_hash(conn, candidate.height)? != Some(candidate.block_hash)
        {
            return Err(corrupt("swap recovery anchor changed"));
        }
        let spent = conn
            .query_row(
                // The scanner removes known wallet spends from the unlinked nullifier map.
                // Both stores must be checked before interpreting absence as unspent.
                "SELECT t.txid,t.block_height FROM nullifier_map n
            JOIN tx_locator_map t USING(block_height,tx_index)
            WHERE n.spend_pool=?1 AND n.nf=?2 AND t.block_height BETWEEN ?3 AND ?4
            UNION ALL
            SELECT t.txid,t.mined_height FROM ironwood_received_notes n
            JOIN ironwood_received_note_spends s ON s.ironwood_received_note_id=n.id
            JOIN transactions t ON t.id_tx=s.transaction_id
            JOIN blocks b ON b.height=t.block AND b.height=t.mined_height
            WHERE n.nf=?2 AND t.mined_height BETWEEN ?3 AND ?4
            LIMIT 1",
                params![
                    wallet::encoding::pool_code(PoolType::IRONWOOD),
                    note.nullifier().to_bytes(),
                    u32::from(candidate.height),
                    u32::from(through.height)
                ],
                |r| {
                    Ok(SpendStatus::Spent {
                        txid: TxId::from_bytes(r.get(0)?),
                        height: BlockHeight::from(r.get::<_, u32>(1)?),
                    })
                },
            )
            .optional()?;
        if let Some(spent) = spent {
            return Ok(spent);
        }
        let covered: u64 = conn.query_row(
            "SELECT COUNT(*) FROM ironwood_nullifier_scan_blocks WHERE height BETWEEN ?1 AND ?2",
            params![u32::from(candidate.height), u32::from(through.height)],
            |r| r.get(0),
        )?;
        Ok(
            if covered == u64::from(u32::from(through.height) - u32::from(candidate.height)) + 1 {
                SpendStatus::Unspent
            } else {
                SpendStatus::Unknown
            },
        )
    }

    /// Transaction-scoped form of [`WalletDb::queue_swap_payment`].
    pub fn queue_swap_payment(
        &mut self,
        account: AccountUuid,
        key: KeyId,
        candidate: &PendingPayment,
    ) -> Result<(), Error> {
        let conn = self.conn.0;
        let (id, _) = authenticate(conn, &self.params, account, key, candidate)?;
        if wallet::get_block_hash(conn, candidate.height)?
            .is_some_and(|hash| hash != candidate.block_hash)
        {
            return Err(corrupt("swap recovery block changed"));
        }
        let old = conn
            .query_row(
                "SELECT txid, action_index, height, block_hash, tx_index, position,
                    encrypted_note, receiving_key_id
             FROM ironwood_swap_payment_recovery WHERE txid=?1 AND action_index=?2",
                params![candidate.txid.as_ref(), candidate.action_index],
                |r| Ok((payment(r)?, r.get::<_, i64>(7)?)),
            )
            .optional()?;
        if let Some((old, owner)) = old {
            return if old == *candidate && owner == id {
                Ok(())
            } else {
                Err(corrupt("conflicting swap recovery identity"))
            };
        }
        conn.execute("UPDATE ironwood_swap_discovery SET closed=0,next_attempt_at=0 WHERE receiving_key_id=?1 AND closed=1",[id])?;
        conn.execute(
            "INSERT INTO ironwood_swap_payment_recovery (
                receiving_key_id, txid, action_index, height, block_hash,
                tx_index, position, encrypted_note
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                id,
                candidate.txid.as_ref(),
                candidate.action_index,
                u32::from(candidate.height),
                candidate.block_hash.0,
                candidate.tx_index,
                candidate.position,
                candidate.encrypted_note.to_bytes().as_slice()
            ],
        )?;
        Ok(())
    }
}
