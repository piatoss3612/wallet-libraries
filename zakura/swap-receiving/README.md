# Swap receiving

Unpublished implementation of the draft v1 receiving-key and refund-memo
conventions. It supports refund and incoming keys with the account's existing
spending authority. The derivation still requires cryptographic review before
issuing live addresses.

## Wallet integration

1. Reserve a purpose-specific index durably before exposing an address.
2. Call `KeyId::new(purpose, index).derive(account_external_fvk)` and select
   `address_at(0u32, Scope::External)` for the receiver.
3. Register that FVK for scanning and retain its purpose/index with received notes.
4. For a refund, encode `RefundMemo` in an ordinary internal note in the funding
   transaction, whose only transparent output pays the P2PKH or P2SH deposit.
   Decode only after note authentication and verification that the transaction
   was the wallet's own send, including zero-value and spent notes.
5. Reconstruct inputs with their derived FVKs. Use the account's spending key to
   sign, and its ordinary internal key for change.

`has_same_spending_authority` compares the `ak` and `nk` of valid FVKs. It does not
replace recipient, nullifier, signature, value, or change validation. The wallet
owns reservation, persistence, coverage, lifecycle, and note selection. There is
no alternate balance store in this crate.

The SQLite backend's `experimental-swap-receiving` feature adds durable key
registration. `get_swap_receiving_keys` reconstructs and verifies stored receivers
after reopen. `reserve_swap_refund_key` is also usable inside the wallet's
transaction helpers, so a reservation and an application's operation record can
commit together. Do not expose an address until that transaction commits.

A wallet drives the feature with these calls:

- `maintain_swap_receiving` when each sync starts and again at the chain tip. It
  retains spend history and, at the tip, recovers funding memos and keeps the
  incoming lookahead.
- `close_finished_swap_keys` once sync reaches a tip it confirmed with the network,
  passing that tip.
- `reserve_swap_refund_key` for a refund address and
  `prepare_swap_receive_reservation` for an incoming one, with the latest network
  tip. Both require scanning within `ISSUANCE_TIP_LAG` blocks of it.
- For an outgoing swap, `record_swap_refund_quote` when the quote arrives, then
  `swap_funding_memo` for the funding transaction and
  `verify_swap_funding_proposal` before signing. Storing the funding transaction
  starts its refund key.
- For an incoming swap, `begin_swap_receive_quote` just before the request leaves
  the device, which returns the request's identity, `finish_swap_receive_quote`
  with its outcome, `start_swap_receive_quote` for the deposit instructions to
  show, and `reap_swap_receive_reservations` after reconciling due quotes.
- For restore sweeps, the steps in [Restore sweeps](#restore-sweeps), then
  `finish_swap_nullifier_recovery` at the scanned tip. `swap_history_pending`
  reports sweeps still unfinished.

The registry stores full `u64` indices as fixed-width big-endian blobs for SQLite
ordering. This is an internal storage encoding; the KDF and memo remain
little-endian. The feature is disabled by default.

A key issued on this device is trial-decrypted from its `scan_from` height until
it closes (`active_from` set, `closed_at` unset). Every compact scan batch
includes all active keys. Activation queues a forced rescan of blocks already
scanned at or above that height, and a batch whose key snapshot missed an active
key requeues its range, so no block in a key's active range goes unchecked.
Catching up after time offline scans active keys like any other blocks.

A newly issued incoming key starts at the first unscanned block. A refund can only
follow its deposit, so a refund key starts when the wallet stores the transaction
funding its swap, at the first block above the scanned chain, and a quote that is
never funded never starts its key. Keys found only through restore are not
scanned until their receiver-directory sweep completes (see
[Restore sweeps](#restore-sweeps)).

With `NoteSelection::PreferConsolidation`, swap notes fill a send's spare input
slots before any other note, within the existing input constraints. A spare slot
adds neither an action nor a fee, and the swap note's value moves into ordinary
internal change, which normal account recovery finds. This never triggers a
separate transaction or deletes a receiving key; a send without a spare slot
leaves swap notes for a later one.

## Completion policy

Normalize a NEAR status response with `lifecycle::near_observation(purpose,
&ProviderStatus)`, or read a raw `/v0/status` body with
`lifecycle::near_status_observation`. A refund key expects ZEC after `REFUNDED`, after `SUCCESS`
with a positive `refundedAmount`, and after an `EXACT_OUTPUT` `SUCCESS`, which
can return unused input. An incoming key expects `amountOut` after `SUCCESS`.
An incoming source-chain refund expects nothing on Zcash. Unknown statuses
leave the previous observation unchanged.

`record_swap_observation` persists each observation immediately.
`close_finished_swap_keys` stops trial decryption for a key as soon as every
operation on it has a conclusive terminal status (`FAILED` is not) and its
receipts cover the expected amounts with ZIP 315's untrusted confirmations, so a
reorg cannot strand a receipt on a closed key. It also stops 30 days after the
latest quote deadline (or registration, without one), whatever the provider
reports. Incoming keys this wallet issued stay active until paid and their
reservation ends. Nothing closes unless the wallet is scanned to the tip the
caller confirmed, and closing uses the earlier of the caller's clock and that
tip's block time, so a clock that runs fast cannot end scanning early. No
provider response credits a note, and a closed key keeps its notes. A rare
payment after a key closed, such as a second refund, is found by
`recheck_swap_history`, which sweeps every closed key once, or by a seed restore.

A recorded refund quote expects nothing until it is funded, so a stale quote never
holds a key open. Storing its funding transaction makes the swap's outcome decide
instead.

## Derivation

A swap key keeps the account's `ak` and `nk` and replaces its external `rivk`:

```text
rivk' = ToScalar(PRF^expand_rivk([0x85] || ak || nk || [purpose] || I2LEOSP_64(index)))
```

`PRF^expand` is BLAKE2b-512 personalized with `Zcash_ExpandSeed` over
`rivk || input`, and `ToScalar` reduces its little-endian output modulo the Pallas
scalar order. `purpose` is 0 for refund and 1 for incoming. This is ZIP 32's
internal-key derivation (first byte `0x83`, no suffix) with a different first byte
and a 9-byte suffix. The input fits one BLAKE2b block, as the internal key's does,
so a ZIP 2005 recovery circuit could accept swap keys as one more `rivk` case
with purpose and index as private inputs. Network and pool identify stored keys
but are not derivation inputs.

`0x85` was unused by the protocol specification and ZIPs when chosen. Reserve it
with the ZIP editors before issuing live addresses. Parsing the result validates
both external and internal incoming viewing keys; with negligible probability an
index has no valid key, and derivation returns an error.

## Refund memo

| Offset | Bytes | Field |
|---|---:|---|
| 0 | 5 | `FF 5A 53 57 50` (`0xFF`, `ZSWP`) |
| 5 | 1 | Version `1` |
| 6 | 1 | Refund purpose `0` |
| 7 | 8 | Index, little-endian |
| 15 | 497 | Reserved: written as zero, ignored on decode |

The memo does not store the deposit address. A restored refund key needs none: it
is swept once and then scans until 30 days after its funding block, with no
provider lookups. Ignoring the reserved bytes keeps prerelease records, which
appended the address there, decoding to their index. Incoming indices are
recovered through lookahead, not this memo. The decoder distinguishes unrelated
memos from unsupported versions or purposes. Recovery leaves a record it cannot
read unprocessed, without failing, and refund issuance waits for it, since it may
hold a refund index.

## Validation

From the workspace root:

```sh
cargo test -p zakura-swap-receiving --locked
```

The tests check independent Python BLAKE2b/scalar vectors, that the same
construction with `0x83` reproduces zakura-orchard's internal key, purpose
separation, boundary indices, authority substitutions, and malformed memos.
Regenerate the vectors from this directory with
`python3 tests/vectors/generate.py > tests/vectors/rivk.csv`.

The protocol test builds Ironwood outputs and discards the derived keys. It
decrypts a zero-value internal recovery memo, reconstructs the refund key and
an incoming lookahead key, then spends those notes with an ordinary note. It
verifies real proofs and spend/binding signatures and decrypts ordinary internal
change. It also checks that ordinary viewing keys cannot read the swap notes and
that zero OVK reveals the fixture payouts but not the recovery marker.

This is a bundle-level test with synthetic commitments as signing messages and a
local commitment tree. Separate SQLite registry tests cover durable allocation,
reopening, lookahead, recovery bounds, rollback, and concurrent connections:

```sh
cargo test -p zakura-client-sqlite --features experimental-swap-receiving --locked swap_receiving
```

The SQLite integration tests also scan both purposes alongside ordinary keys,
reopen the database, and construct a mixed-input transaction whose change returns
to the ordinary internal key. They check replay, late payments, and invalid note
metadata. Active keys remain in every subsequent compact scan until they close.
Full-transaction retrieval authenticates swap memos before or after compact
scanning, including self-payments also recoverable through the ordinary OVK.
Enhance PIR resolves the registered key after restart and rejects altered
ciphertext without clearing pending work. A build without swap support reports
an error for that retrieval instead of trying the ordinary account key.

Software PCZT signing of swap notes works through the same builder. Hardware
signers need firmware qualification before swap keys are enabled for hardware
accounts.

## Protocol baseline

Zcash Protocol Specification **v2026.7.0-202-gafa086, NU6.3 proposal**, commit
`afa086bd976e316612a5c06fb139429958d07d84`:

- [§4.2.3, Key Components, pp.40–41](https://github.com/zcash/zips/blob/afa086bd976e316612a5c06fb139429958d07d84/protocol/protocol.tex#L5753-L5909),
  PDF anchor `orchardkeycomponents`.
- [§5.6.4.4, Raw Full Viewing Keys, p.123](https://github.com/zcash/zips/blob/afa086bd976e316612a5c06fb139429958d07d84/protocol/protocol.tex#L13077-L13104),
  PDF anchor `orchardfullviewingkeyencoding`.

The swap KDF and memo format are proposed wallet conventions, separate from
these protocol requirements. Passing these tests is not a cryptographic review.

## Private recovery authentication

`recovery::EncryptedNote` joins compact Action fields with the ciphertext suffix
returned by enhancement. `decrypt` derives the requested purpose/index key from
the account, authenticates the complete Ironwood plaintext, and requires the
expected diversifier-zero receiver. It returns the actual note, memo, and locally
computed spend nullifier. Public zero-OVK recovery is not used as ownership proof.

`RecoveredNote::verify_position` checks the exact position and commitment path
against an independently accepted Ironwood root. It rejects positions above the
32-bit tree capacity rather than truncating them. Wallet storage must recheck the
anchor after asynchronous work and separately establish spend-history coverage.
Neither helper authenticates service transaction metadata or makes a note spendable.

`cargo test -p zakura-swap-receiving --test recovery` covers both derivation
purposes, wrong account/index, altered compact and memo ciphertext, wrong roots,
and substituted or overflowing positions. Decryption and Merkle hashing delegate
to `zakura-orchard`, following the pinned NU6.3 proposal at `afa086bd976e316612a5c06fb139429958d07d84`.

SQLite's `apply_swap_sweep` commits each queued candidate only at the wallet's
fully scanned tip. It authenticates ciphertext again, checks receipt bounds and
retained spend evidence, and verifies the publication's inclusion path at its
local block anchor. It marks the leaf for future witness updates and writes the
note, memo, key and known spend atomically. Incomplete inputs preserve the queue
without adding balance. A synthetic test spends a privately imported note into
ordinary internal change. Transaction IDs and Action indices remain directory
assertions, checked for conflicts with local data. The inclusion proof binds the
commitment and position. When scanning or a full transaction finds the same
nullifier, that transaction is authoritative: the note moves to it and the
claimed transaction is removed, and a directory answer for a note already stored
under another transaction is dropped.

`maintain_swap_receiving` retains Ironwood spend evidence from the account's
birthday, so a recovered note's spend state is checked locally rather than taken
from the directory. Other pools keep ordinary pruning.

### Restore sweeps

Receiver-directory lookups happen only for keys recovered from the seed: refund
keys named by funding memos and the incoming lookahead. Each such key gets one
sweep up to a fixed target block. `prepare_swap_discovery_batch` selects due
sweeps and reports all remaining uncached lookups for transport selection. The
app performs the directory and note-data lookups; the wallet steps around them
are:

1. `swap_publication_anchor` binds a directory publication to the wallet's own
   chain and refuses one more than `MAX_PUBLICATION_LAG` blocks behind it.
2. `begin_swap_discovery_attempt` leases a record against that publication when
   its attempt starts, refusing a publication short of the sweep's target before
   any lookup. Attempts back off from one minute to twelve hours.
3. Unless the record's lookup is already queued, `swap_note_data_needed` names the
   positions of the directory's payments that need note data, and
   `queue_swap_directory_lookup` queues the complete lookup with that data.
4. `apply_swap_sweep` applies the queued notes with the publication's inclusion
   paths and finishes the sweep at the anchor its lookup reached.

A step that must wait for more scanning or a newer publication returns
`Error::SweepDeferred`. After its sweep, a key scans from the next block: a refund key until its
swap closes, and an incoming key never issued here for 24 hours after it was
registered, catching a payout from a swap in flight at restore. During that
watch, incoming issuance takes the highest free index in the recovery window
rather than the lowest, since the lowest unpaid indices may be the old device's
open swaps. Issuing a closed key again starts scanning at the tip.
`recheck_swap_history` sweeps closed keys again on request. Reorgs below a sweep
reopen it. Until incoming sweeps finish, new incoming reservations wait.

Historical spend retention follows the earliest unfinished sweep or pending note.
Missing spend evidence queues one coalesced replay of the public account recovery
interval. A note before that interval stays explicitly blocked until the
account's restore range is widened.
