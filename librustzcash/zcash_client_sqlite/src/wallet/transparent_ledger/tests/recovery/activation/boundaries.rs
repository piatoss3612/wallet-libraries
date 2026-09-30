//! Regressions at the candidate/trusted-transition and consumer-read boundaries.
use super::*;
use std::num::NonZeroUsize;
use zcash_client_backend::data_api::transparent_ledger::{
    TransparentRecoveryWork, TransparentRecoveryWorkBatch,
};

fn work(
    st: &State,
    account: AccountUuid,
    limit: usize,
) -> TransparentRecoveryWorkBatch<AccountUuid> {
    st.wallet()
        .db()
        .transparent_recovery_work(account, NonZeroUsize::new(limit).unwrap())
        .unwrap()
}

fn active_and_candidate() -> (State, AccountUuid, AccountUuid, ReceiveEvent) {
    let (mut st, accounts) = shadow_wallet_with(1);
    let rev = revision(1, false);
    qualify(&mut st, &rev);
    let receive = recover_completely(&mut st, accounts[0], &rev, 51);
    set_policy(&mut st, PrivateRequired);
    promote(&mut st, accounts[0]).unwrap();
    (st, accounts[0], accounts[1], receive)
}

mod configuration;
mod financial;
mod revisions;
mod work;
