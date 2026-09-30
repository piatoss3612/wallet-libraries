//! Runs only against disposable databases created by check-legacy-rollback.py.
use std::path::PathBuf;
use zcash_client_backend::data_api::{
    DecryptedTransaction, WalletRead, WalletWrite, testing::TestRng,
};
use zcash_client_sqlite::{WalletDb, util::SystemClock, wallet::init::WalletMigrator};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::{BlockHeight, BranchId, Network};

fn main() {
    let args: Vec<_> = std::env::args().collect();
    let mut db = WalletDb::for_path(
        PathBuf::from(&args[2]),
        Network::MainNetwork,
        SystemClock,
        TestRng::seed_from_u64(0),
    )
    .unwrap();
    match args[1].as_str() {
        "init" => {
            WalletMigrator::new().init_or_migrate(&mut db).unwrap();
        }
        #[cfg(feature = "current")]
        "prepare" => {
            zcash_client_sqlite::wallet::init::prepare_legacy_rollback(&mut db).unwrap();
        }
        "ingest" | "expect-failure" => {
            // Exercise the old migrator too: it must leave this handover schema usable.
            WalletMigrator::new().init_or_migrate(&mut db).unwrap();
            db.update_chain_tip(BlockHeight::from_u32(3_483_367))
                .unwrap();
            let raw = hex::decode(std::fs::read_to_string(&args[3]).unwrap().trim()).unwrap();
            let branch =
                BranchId::try_from(u32::from_le_bytes(raw[8..12].try_into().unwrap())).unwrap();
            let tx = Transaction::read(&raw[..], branch).unwrap();
            let decrypted = DecryptedTransaction::new(
                Some(BlockHeight::from_u32(3_483_367)),
                &tx,
                vec![],
                vec![],
                vec![],
            );
            let result = db.store_decrypted_tx(decrypted);
            if args[1] == "expect-failure" {
                let err = result
                    .expect_err("negative control must fail before compatibility preparation");
                assert!(
                    err.to_string().contains("zip318_kind"),
                    "unexpected failure: {err}"
                );
            } else {
                result.unwrap();
                assert!(db.get_transaction(tx.txid()).unwrap().is_some());
                // Repeat ingestion of the existing row, as sync/enhancement does.
                db.store_decrypted_tx(DecryptedTransaction::new(
                    Some(BlockHeight::from_u32(3_483_367)),
                    &tx,
                    vec![],
                    vec![],
                    vec![],
                ))
                .unwrap();
            }
        }
        _ => panic!("unexpected command"),
    }
}
