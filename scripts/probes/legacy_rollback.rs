//! Runs only against disposable databases created by check-legacy-rollback.py.
use std::path::PathBuf;
use zcash_client_backend::data_api::{
    DecryptedTransaction, WalletRead, WalletWrite, testing::TestRng,
};
use zcash_client_sqlite::{WalletDb, util::SystemClock, wallet::init::WalletMigrator};
use zcash_primitives::transaction::{Transaction, TransactionData};
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
            #[cfg(not(feature = "current"))]
            if db.get_account_ids().unwrap().is_empty() {
                use zcash_client_backend::data_api::{AccountBirthday, chain::ChainState};
                use zcash_primitives::block::BlockHash;
                db.create_account(
                    "old wallet fixture",
                    &secrecy::SecretVec::new(vec![7; 32]),
                    &AccountBirthday::from_parts(
                        ChainState::empty(BlockHeight::from_u32(1_200_000), BlockHash([0; 32])),
                        None,
                    ),
                    None,
                )
                .unwrap();
            }
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
            let parsed = Transaction::read(&raw[..], branch).unwrap();
            // Make wallet involvement unavoidable. The public fixture alone has no decryptable
            // output for this wallet, so its storage path would return successfully without
            // touching any rows. This synthetic fixture retains its Ironwood encoding and adds
            // a wallet-owned transparent output; it is an ingestion fixture, not a valid chain tx.
            let account = db.get_account_ids().unwrap()[0];
            let address = *db
                .get_transparent_receivers(account, false, false)
                .unwrap()
                .keys()
                .next()
                .unwrap();
            let tx = TransactionData::from_parts_v6(
                branch,
                parsed.lock_time(),
                parsed.expiry_height(),
                Some(transparent::bundle::Bundle {
                    vin: vec![],
                    vout: vec![transparent::bundle::TxOut::new(
                        zcash_protocol::value::Zatoshis::const_from_u64(5_000),
                        address.script().into(),
                    )],
                    authorization: transparent::bundle::Authorized,
                }),
                parsed.sapling_bundle().cloned(),
                parsed.orchard_bundle().cloned(),
                parsed.ironwood_bundle().cloned(),
            )
            .freeze()
            .unwrap();
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
                #[cfg(not(feature = "current"))]
                {
                    use transparent::bundle::{OutPoint, TxOut};
                    use zcash_client_backend::wallet::WalletTransparentOutput;
                    use zcash_protocol::value::Zatoshis;
                    let account = db.get_account_ids().unwrap()[0];
                    let address = *db
                        .get_transparent_receivers(account, false, false)
                        .unwrap()
                        .keys()
                        .next()
                        .unwrap();
                    let output = WalletTransparentOutput::from_parts(
                        OutPoint::new([7; 32], 0),
                        TxOut::new(Zatoshis::const_from_u64(50_000), address.script().into()),
                        Some(BlockHeight::from_u32(3_483_365)),
                        Some(account),
                        None,
                        None,
                    )
                    .unwrap();
                    db.put_received_transparent_utxo(&output).unwrap();
                }
            }
        }
        _ => panic!("unexpected command"),
    }
}
