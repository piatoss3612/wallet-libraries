//! Receiver discovery over a caller-supplied, bounded transport.
use crate::{
    check_next, public_bytes, response_bytes, AcceptedCoverage, Client, Error, Manifest,
    MAX_MANIFEST_BYTES,
};
use receiver_directory::{Payment, RawRecord, ReceiverBytes};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// Hosts supply their route policy, cancellation, and timeout for every request.
/// Enforce `limit` while streaming, reject non-success responses, and map HTTP
/// 410 to [`Error::Revision`]. Do not follow redirects or retry in cleartext.
#[allow(async_fn_in_trait)]
pub trait Transport {
    /// GET `url`, returning a body of at most `limit` bytes.
    async fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>, Error>;
    /// POST `body` to `url`, returning a body of at most `limit` bytes.
    async fn post(&self, url: &str, body: Vec<u8>, limit: usize) -> Result<Vec<u8>, Error>;
}

impl<T: Transport> Transport for &T {
    async fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>, Error> {
        T::get(self, url, limit).await
    }
    async fn post(&self, url: &str, body: Vec<u8>, limit: usize) -> Result<Vec<u8>, Error> {
        T::post(self, url, body, limit).await
    }
}

/// Validated receiver session, independent of the wallet's networking stack.
pub struct DirectoryClient<T> {
    origin: String,
    http: T,
    /// Hex session ID that pins the setup and witness routes.
    id: String,
    client: Client,
}

/// The largest common proof file a client downloads.
pub const MAX_WITNESS_BYTES: usize = 64 * 1024 * 1024;

/// The most pages [`DirectoryClient::lookup`] reads, one query each. A longer history is
/// read with [`DirectoryClient::page`] from a cursor the caller keeps.
pub const MAX_LOOKUP_PAGES: u32 = 64;

impl<T: Transport> DirectoryClient<T> {
    /// Use the exact advertised revision whose coverage the caller checked. Reuse the client
    /// across lookups and reconnect for a new revision.
    pub async fn connect_manifest(
        origin: &str,
        http: T,
        accepted: AcceptedCoverage,
        manifest: Manifest,
    ) -> Result<Self, Error> {
        let origin = origin.trim_end_matches('/').to_owned();
        accepted.check(&manifest.directory)?;
        let id = hex::encode(manifest.id()?);
        let public = http
            .get(
                &format!("{origin}/v1/receiver/public/{id}"),
                public_bytes(manifest.directory.rows)?,
            )
            .await?;
        let client = Client::new(manifest, &public, accepted)?;
        Ok(Self {
            origin,
            http,
            id,
            client,
        })
    }

    /// Fetch bounded public metadata before selecting the corresponding local chain anchor.
    pub async fn fetch_manifest(origin: &str, http: &T) -> Result<Manifest, Error> {
        let bytes = http
            .get(
                &format!("{}/v1/receiver/init", origin.trim_end_matches('/')),
                MAX_MANIFEST_BYTES,
            )
            .await?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(Error::Malformed);
        }
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Downloads `manifest`'s filters, identical for every wallet, so a wallet can test its
    /// receivers before deciding whether to look any up. The file must hold exactly the
    /// sets the manifest declares.
    pub async fn fetch_filters(
        origin: &str,
        http: &T,
        manifest: &Manifest,
    ) -> Result<receiver_directory::filter::Filters, Error> {
        let bytes = http
            .get(
                &format!(
                    "{}/v1/receiver/filters/{}",
                    origin.trim_end_matches('/'),
                    hex::encode(manifest.id()?)
                ),
                receiver_directory::filter::MAX_FILTERS_BYTES,
            )
            .await?;
        if <[u8; 32]>::from(Sha256::digest(&bytes)) != manifest.directory.filters_sha256 {
            return Err(Error::Malformed);
        }
        let filters = receiver_directory::filter::Filters::decode(&bytes)?;
        manifest.directory.check_filters(&filters)?;
        Ok(filters)
    }

    /// The session manifest.
    pub fn manifest(&self) -> &Manifest {
        self.client.manifest()
    }

    /// Download the common proof file, identical for all wallets, of at most
    /// [`MAX_WITNESS_BYTES`]. The request never contains a receiver. The bytes are
    /// unchecked: the wallet verifies them against this session's manifest and its own
    /// chain before trusting any path.
    pub async fn witnesses(&self) -> Result<Vec<u8>, Error> {
        self.http
            .get(
                &format!("{}/v1/receiver/witness/{}", self.origin, self.id),
                MAX_WITNESS_BYTES,
            )
            .await
    }

    /// Reads page `page` of `receiver`'s history over PIR: one query, whose row choice is
    /// hidden in its encryption. Absence is `None` only on page zero, an empty history; a
    /// missing later page is an error. The record is checked structurally only.
    pub async fn page(
        &self,
        receiver: ReceiverBytes,
        page: u32,
    ) -> Result<Option<RawRecord>, Error> {
        let query = self.client.prepare(receiver, page)?;
        let body = self
            .http
            .post(
                &format!("{}/v1/receiver/query", self.origin),
                query.body().to_vec(),
                response_bytes(self.client.manifest().directory.rows)?,
            )
            .await?;
        self.client.decode(query, &body)
    }

    /// Return every payment to `receiver` in one publication, or an error, reading one page
    /// at a time (see [`Self::page`]). A history of more than [`MAX_LOOKUP_PAGES`] pages is
    /// [`Error::TooManyPages`], before any page past the first is read. No partial success or cleartext fallback. This does
    /// not hide from the directory that a receiver has several payments. Revalidate the
    /// anchor before crediting results if the wallet's chain changes while awaiting I/O.
    /// Payments are checked structurally only: the wallet validates their Action fields
    /// cryptographically before use.
    pub async fn lookup(
        &self,
        receiver: ReceiverBytes,
        accepted: AcceptedCoverage,
    ) -> Result<Vec<Payment>, Error> {
        accepted.check(&self.manifest().directory)?;
        let mut records: Vec<RawRecord> = Vec::new();
        let mut outputs = BTreeSet::new();
        loop {
            let page = records.len() as u32;
            let Some(record) = self.page(receiver, page).await? else {
                // The row decoder permits absence only on page zero.
                return Ok(Vec::new());
            };
            if record.total > MAX_LOOKUP_PAGES {
                return Err(Error::TooManyPages);
            }
            if let Some(previous) = records.last() {
                check_next(previous, &record)?;
            }
            if !outputs.insert((record.payment.txid, record.payment.action_index)) {
                return Err(Error::Malformed);
            }
            let complete = record.page + 1 == record.total;
            records.push(record);
            if complete {
                return Ok(records.into_iter().map(|r| r.payment).collect());
            }
        }
    }
}
