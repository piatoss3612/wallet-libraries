-- Frozen performance/equivalence oracle from chainapsis/vizor-wallet
-- d092429c704e25181f25cfdf2a9bf043f94481c1, transactions.rs read_history_bases.
-- Test-only: this is deliberately not a production accounting definition.

        WITH vt AS (
            WITH
            notes AS (
                SELECT ro.account_id              AS account_id,
                       ro.transaction_id          AS transaction_id,
                       ro.pool                    AS pool,
                       id_within_pool_table,
                       ro.value                   AS value,
                       ro.value                   AS received_value,
                       0                          AS spent_value,
                       0                          AS spent_note_count,
                       CASE WHEN ro.is_change THEN 1 ELSE 0 END AS change_note_count,
                       CASE WHEN ro.is_change THEN 0 ELSE 1 END AS received_count,
                       CASE
                         WHEN (ro.memo IS NULL OR ro.memo = X'F6') THEN 0
                         ELSE 1
                       END AS memo_present,
                       CASE WHEN ro.pool = 0 THEN 1 ELSE 0 END AS does_not_match_shielding
                FROM v_received_outputs ro
                UNION
                SELECT ro.account_id              AS account_id,
                       ros.transaction_id         AS transaction_id,
                       ro.pool                    AS pool,
                       id_within_pool_table,
                       -ro.value                  AS value,
                       0                          AS received_value,
                       ro.value                   AS spent_value,
                       1                          AS spent_note_count,
                       0                          AS change_note_count,
                       0                          AS received_count,
                       0                          AS memo_present,
                       CASE WHEN ro.pool != 0 THEN 1 ELSE 0 END AS does_not_match_shielding
                FROM v_received_outputs ro
                JOIN v_received_output_spends ros
                     ON ros.pool = ro.pool
                     AND ros.received_output_id = ro.id_within_pool_table
            ),
            sent_note_counts AS (
                SELECT sent_notes.from_account_id     AS account_id,
                       sent_notes.transaction_id      AS transaction_id,
                       COUNT(DISTINCT sent_notes.id)  AS sent_notes
                FROM sent_notes
                LEFT JOIN v_received_outputs ro ON sent_notes.id = ro.sent_note_id
                WHERE COALESCE(ro.is_change, 0) = 0
                GROUP BY sent_notes.from_account_id, sent_notes.transaction_id
            ),
            blocks_max_height AS (
                SELECT MAX(blocks.height) AS max_height FROM blocks
            )
            SELECT transactions.txid          AS txid,
                   transactions.mined_height  AS mined_height,
                   transactions.tx_index      AS tx_index,
                   transactions.expiry_height AS expiry_height,
                   transactions.fee           AS fee_paid,
                   blocks.time                AS block_time,
                   SUM(notes.value)           AS account_balance_delta,
                   SUM(notes.spent_value)     AS total_spent,
                   SUM(notes.received_value)  AS total_received,
                   (
                        transactions.mined_height IS NULL
                        AND transactions.expiry_height BETWEEN 1 AND blocks_max_height.max_height
                   ) AS expired_unmined,
                   (
                        SUM(notes.does_not_match_shielding) = 0
                        AND SUM(notes.spent_note_count) > 0
                        AND (SUM(notes.received_count) + SUM(notes.change_note_count)) > 0
                        AND MAX(COALESCE(sent_note_counts.sent_notes, 0)) = 0
                   ) AS is_shielding
            FROM notes
            JOIN accounts ON accounts.id = notes.account_id
            JOIN transactions ON transactions.id_tx = notes.transaction_id
            LEFT JOIN blocks_max_height
            LEFT JOIN blocks ON blocks.height = transactions.mined_height
            LEFT JOIN sent_note_counts
                 ON sent_note_counts.account_id = notes.account_id
                 AND sent_note_counts.transaction_id = notes.transaction_id
            WHERE accounts.uuid = ?1
            GROUP BY notes.account_id, notes.transaction_id
        )

        SELECT
            vt.txid,
            COALESCE(tx.id_tx, -1) AS transaction_id,
            vt.mined_height,
            vt.expired_unmined,
            vt.account_balance_delta,
            vt.fee_paid AS fee_paid,
            COALESCE(vt.block_time, 0) AS block_time,
            COALESCE(vt.total_spent, 0) AS total_spent,
            COALESCE(vt.total_received, 0) AS total_received,
            COALESCE(vt.is_shielding, 0) AS is_shielding,
            vt.expiry_height,
            COALESCE(vt.tx_index, -1) AS tx_index,
            tx.created,
            CAST(COALESCE(strftime('%s', tx.created), 0) AS INTEGER) AS created_time,
            EXISTS (
                SELECT 1
                FROM transactions spent_tx
                JOIN orchard_received_note_spends spent
                    ON spent.transaction_id = spent_tx.id_tx
                JOIN orchard_received_notes spent_note
                    ON spent_note.id = spent.orchard_received_note_id
                WHERE spent_tx.txid = vt.txid
                  AND spent_note.note_version = ?2
            ) AS spent_orchard_note
        FROM vt
        LEFT JOIN transactions tx ON tx.txid = vt.txid
