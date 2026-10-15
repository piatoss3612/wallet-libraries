//! Early restore of an account's one-time keys from a receiver directory's public
//! filters (see the backend's `OneTimeIvkRestoreWrite::begin_one_time_early_restore`).

use receiver_directory::{
    Receiver,
    filter::{Filters, PAID, RECENT},
};
use receiver_pir::{AcceptedCoverage, transport::DirectoryClient};
use zakura_one_time_ivk::{KeyId, Purpose};
use zcash_client_backend::data_api::one_time_ivk::restore::{
    DiscoveryWork, EarlyRestoreKey, OneTimeIvkRestoreWrite, PublicationEnd,
};
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};

use crate::{
    DirectoryError, Transport,
    seen::seen_sets,
    sweep::{AccountError, AccountId, Error, WriteLock, covers_restore_watch, directory_payment},
};

/// Keys an early restore always checks, from index zero, in each purpose.
pub const FIRST_WINDOW: u64 = 100;
/// Consecutive keys in no filter after which an early restore's lookahead stops, past
/// [`FIRST_WINDOW`].
pub const GAP: u64 = 30;
/// The most keys an early restore checks in one purpose. A purpose that reaches it is
/// left to restore sweeps beyond it.
pub const MAX_KEYS: u64 = 5_000;
/// Receivers derived per wallet read.
const CHUNK: u32 = 100;

/// An early restore that registered its keys, with the paid ones still to look up (see
/// [`EarlyRestore::look_up`]).
pub struct EarlyRestore<'a, T> {
    origin: &'a str,
    transport: T,
    manifest: receiver_pir::Manifest,
    accepted: AcceptedCoverage,
    /// Paid keys with their receivers.
    paid: Vec<DiscoveryWork>,
}

/// What [`EarlyRestore::look_up`] did. `E` is the wallet's error type.
#[derive(Debug)]
pub struct LookedUp<E> {
    /// Paid keys whose lookups the wallet recorded.
    pub recorded: usize,
    /// Paid keys left to restore sweeps, with why.
    pub failed: Vec<(KeyId, Error<E>)>,
}

/// Starts `account`'s early restore against the publication `origin` serves, if the
/// wallet allows one (see `OneTimeIvkRestoreRead::one_time_early_restore_start`). Call
/// it before the restore scan stores any of the account's history, with a time budget:
/// once the scan stores a block from the restore start on, the wallet refuses to begin.
///
/// The publication must commit to `genesis` and cover history from Ironwood activation on
/// `params`' network. It is provisional: the wallet accepts it only once its scan stores
/// the publication's last block with the declared hash and tree size. Only the public
/// filters are read here, so this makes no private query. Each purpose's keys are checked
/// from index zero: always the first [`FIRST_WINDOW`], then on until [`GAP`] consecutive
/// keys are in no seen set and not in the paid filter, up to [`MAX_KEYS`]. Every incoming
/// key checked is registered, and refund keys only when seen or paid, since refund
/// issuance skips registered indices. Recent keys follow [`crate::sweep`]'s rule.
///
/// Returns `None` when the wallet has no early restore to run.
#[allow(clippy::too_many_arguments)]
pub async fn begin_early_restore<'a, W, P, T, L>(
    wallet: &mut W,
    params: &P,
    account: AccountId<W>,
    genesis: [u8; 32],
    origin: &'a str,
    transport: T,
    lock: &L,
    now: i64,
) -> Result<Option<EarlyRestore<'a, T>>, Error<AccountError<W>>>
where
    W: OneTimeIvkRestoreWrite,
    P: Parameters,
    T: Transport,
    L: WriteLock,
{
    if wallet
        .one_time_early_restore_start(account)
        .map_err(Error::Wallet)?
        .is_none()
    {
        return Ok(None);
    }
    let activation = params
        .activation_height(NetworkUpgrade::Nu6_3)
        .ok_or(Error::Unavailable("Ironwood is not active on this network"))?;
    let manifest = DirectoryClient::fetch_manifest(origin, &transport).await?;
    let directory = &manifest.directory;
    let accepted = AcceptedCoverage {
        genesis,
        required_start: activation.into(),
        height: directory.end_height,
        hash: directory.end_hash,
    };
    accepted.check(directory)?;
    let filters = DirectoryClient::fetch_filters(origin, &transport, &manifest).await?;
    let sets = Sets::new(&manifest, &filters, now)?;
    let mut keys = Vec::new();
    for purpose in [Purpose::Receive, Purpose::Refund] {
        let mut from = 0;
        let mut last_used: Option<u64> = None;
        while from < MAX_KEYS {
            let reach = last_used.map_or(0, |used| used + 1 + GAP).max(FIRST_WINDOW);
            if from >= reach {
                break;
            }
            let count = u32::try_from((reach - from).min(MAX_KEYS - from))
                .unwrap_or(CHUNK)
                .min(CHUNK);
            let work = wallet
                .one_time_receivers(account, purpose, from, count)
                .map_err(Error::Wallet)?;
            for (item, check) in work.iter().zip(sets.check(&work)?) {
                let used = check.seen || check.paid;
                if used {
                    last_used = Some(item.key.index());
                }
                if purpose == Purpose::Receive || used {
                    keys.push((item.clone(), check));
                }
            }
            from += u64::from(count);
        }
    }
    let end = PublicationEnd {
        height: BlockHeight::from(directory.end_height),
        hash: directory.end_hash,
        tree_size: directory.end_position,
    };
    let registered: Vec<_> = keys
        .iter()
        .map(|(item, check)| check.key(item.key))
        .collect();
    let begun = lock
        .write("one_time_early_restore.begin", || {
            wallet.begin_one_time_early_restore(account, end, &registered)
        })
        .map_err(Error::Wallet)?;
    if !begun {
        return Ok(None);
    }
    let paid = keys
        .into_iter()
        .filter(|(_, check)| check.paid)
        .map(|(item, _)| item)
        .collect();
    Ok(Some(EarlyRestore {
        origin,
        transport,
        manifest,
        accepted,
        paid,
    }))
}

impl<T: Transport> EarlyRestore<'_, T> {
    /// The paid keys [`Self::look_up`] looks up.
    pub fn paid(&self) -> usize {
        self.paid.len()
    }

    /// Looks up every paid key over PIR and records each answer with the publication's
    /// witnesses (see `OneTimeIvkRestoreWrite::record_one_time_early_lookup`). Runs
    /// alongside the restore scan. The keys looked up come only from the public filters.
    /// A key whose lookup fails is listed in [`LookedUp::failed`] and left to restore
    /// sweeps, while the others go on, unless the directory's connection failed or it
    /// revoked the session: then the remaining keys are left too.
    pub async fn look_up<W, L>(
        self,
        wallet: &mut W,
        account: AccountId<W>,
        lock: &L,
    ) -> Result<LookedUp<AccountError<W>>, Error<AccountError<W>>>
    where
        W: OneTimeIvkRestoreWrite,
        L: WriteLock,
    {
        let mut looked_up = LookedUp {
            recorded: 0,
            failed: Vec::new(),
        };
        if self.paid.is_empty() {
            return Ok(looked_up);
        }
        let mut client = DirectoryClient::connect_manifest(
            self.origin,
            self.transport,
            self.accepted,
            self.manifest,
            self.paid.len(),
        )
        .await?;
        let witnesses = client.witnesses().await?;
        let mut keys = self.paid.into_iter();
        for item in keys.by_ref() {
            let receiver = Receiver::from_bytes(item.receiver).map_err(DirectoryError::from)?;
            let payments = match client.lookup(receiver, self.accepted).await {
                Ok(payments) => payments
                    .into_iter()
                    .map(directory_payment)
                    .collect::<Vec<_>>(),
                Err(e) => {
                    let run_wide =
                        matches!(e, DirectoryError::Revision | DirectoryError::Transport(_));
                    looked_up.failed.push((item.key, e.into()));
                    if run_wide {
                        break;
                    }
                    continue;
                }
            };
            lock.write("one_time_early_restore.lookup", || {
                wallet.record_one_time_early_lookup(account, item.key, &payments, |p, cmx| {
                    witnesses.path(p, cmx).ok()
                })
            })
            .map_err(Error::Wallet)?;
            looked_up.recorded += 1;
        }
        for item in keys {
            looked_up
                .failed
                .push((item.key, Error::Unavailable("the directory session ended")));
        }
        Ok(looked_up)
    }
}

/// What a publication's filters say about one key's receiver.
#[derive(Clone, Copy)]
struct Check {
    paid: bool,
    seen: bool,
    recent: bool,
}

impl Check {
    /// The registration of `key` this check gives.
    fn key(self, key: KeyId) -> EarlyRestoreKey {
        EarlyRestoreKey {
            key,
            seen: self.seen,
            paid: self.paid,
            recent: self.recent,
        }
    }
}

/// A publication's filters and the provider sets [`crate::sweep`] reads from them.
struct Sets<'a> {
    salt: [u8; 32],
    filters: &'a Filters,
    /// Each provider's seen set label, and its recent set label when that set is current.
    providers: Vec<(String, Option<String>)>,
}

impl<'a> Sets<'a> {
    /// The sets of `manifest`'s `filters`, with recent sets current at `now`.
    fn new(
        manifest: &receiver_pir::Manifest,
        filters: &'a Filters,
        now: i64,
    ) -> Result<Self, DirectoryError> {
        let sets = &manifest.directory.filters;
        let providers = seen_sets(sets)?
            .into_iter()
            .map(|(label, _, _)| {
                let provider = label.split_once('/').map_or(label, |(p, _)| p);
                let recent = format!("{provider}/{RECENT}");
                let current = sets
                    .iter()
                    .any(|set| set.label == recent && covers_restore_watch(set, now));
                (label.to_owned(), current.then_some(recent))
            })
            .collect();
        Ok(Self {
            salt: manifest.directory.salt,
            filters,
            providers,
        })
    }

    /// What the sets say about each of `work`'s receivers. Without a current recent set
    /// from every provider, every receiver is recent, as for sweeps.
    fn check<E>(&self, work: &[DiscoveryWork]) -> Result<Vec<Check>, Error<E>> {
        let receivers = work
            .iter()
            .map(|item| Receiver::from_bytes(item.receiver))
            .collect::<Result<Vec<_>, _>>()
            .map_err(DirectoryError::from)?;
        // Fetching the filters checked them against the manifest's sets.
        let matches = |label: &str| {
            self.filters
                .get(label)
                .expect("the manifest declares every set")
                .matches(&self.salt, &receivers)
        };
        let mut checks: Vec<_> = matches(PAID)
            .into_iter()
            .map(|paid| Check {
                paid,
                seen: false,
                recent: false,
            })
            .collect();
        let complete = self.providers.iter().all(|(_, recent)| recent.is_some());
        for (seen, recent) in &self.providers {
            for (check, hit) in checks.iter_mut().zip(matches(seen)) {
                check.seen |= hit;
            }
            if let Some(recent) = recent {
                for (check, hit) in checks.iter_mut().zip(matches(recent)) {
                    check.recent |= hit;
                }
            }
        }
        if !complete {
            for check in &mut checks {
                check.recent = true;
            }
        }
        Ok(checks)
    }
}
