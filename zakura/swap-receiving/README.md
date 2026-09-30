# Swap receiving POC

Unpublished implementation of the draft v1 receiving-key and refund-memo
conventions. It supports refund and incoming keys with the account's existing
spending authority. The derivation still requires cryptographic review before
issuing live addresses.

## Wallet integration

1. Reserve a purpose-specific index durably before exposing an address.
2. Call `derive_full_viewing_key(account_external_fvk, purpose, index)` and select
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
registration. `reserve_swap_receiving_key` advances a purpose's sequence,
`recover_swap_receiving_key` records validated recovery evidence, and
`watch_swap_receive_key` retains an unpaid incoming lookahead key without
advancing allocation. `get_swap_receiving_keys` reconstructs and verifies stored
receivers after reopen. These are `WalletDb` methods, also usable inside its
transaction helpers so a reservation and an application's operation record can
commit together. Do not expose an address until that transaction commits.

The registry stores full `u64` indices as fixed-width big-endian blobs for SQLite
ordering. This is an internal storage encoding; the KDF and memo remain
little-endian. Registration retains the earliest requested scan height, not
proof that its history was scanned. The feature is disabled by default.

Compact scanning records the keys actually used in each batch, atomically with
its notes and blocks. `get_swap_receiving_scan_ranges` returns their disjoint,
end-exclusive coverage. Registration queues missing history through the known
tip. Later scans and tip updates preserve those gaps until replay completes.
Rewinds trim coverage even in builds without swap support. Refresh the chain tip
before scanning after reopening, including after enabling the feature again.

For a newly issued address, use the next height after the accepted tip as
`scan_from`. For recovery, use the earliest height at which that address could
have received a payment. Requesting older history queues replay and can delay
spending until that history is checked. Key retirement remains a separate step.

The planned selector will prefer swap notes during ordinary sends when doing so
adds neither inputs nor fees, respecting existing input constraints. Confirmed
ordinary internal change then uses normal account recovery. This preference will
not trigger separate transactions or delete receiving keys.

## Completion policy

Normalize NEAR status with `near_status(purpose, status)`. An outgoing refund
expects a Zcash receipt. An incoming source-chain refund does not. Unknown
responses leave the previous observation unchanged.

SQLite persists the observation immediately. A later independently refreshed
chain view anchors ten further scanning blocks. A separate directory check is
required twelve hours after terminal observation. Completed checks and expected
receipt accounting govern closeout. No provider response credits a note.

## Derivation

The HMAC key is the account's canonical 32-byte external `rivk`. The message is
the one-byte label length, ASCII label, little-endian `u64` index, and
little-endian `u32` retry counter. Labels are `swap-refund-v1` and `swap-receive-v1`.
Network and pool identify stored keys but are not v1 derivation inputs.

Interpret HMAC-SHA-512 output as a little-endian integer and reduce modulo the
Pallas scalar order. Keep the account's `ak` and `nk`, replace `rivk`, and accept
the first valid FVK starting at retry zero. Parsing validates both external and
internal incoming viewing keys. Exhaustion returns an error.

## Refund memo

| Offset | Bytes | Field |
|---|---:|---|
| 0 | 5 | `FF 5A 53 57 50` (`0xFF`, `ZSWP`) |
| 5 | 1 | Version `1` |
| 6 | 1 | Refund purpose `0` |
| 7 | 8 | Index, little-endian |
| 15 | 497 | Reserved: written as zero, ignored on decode |

The memo does not store the deposit address. Swaps fund only address-only
transparent deposits, so recovery reads it from the funding transaction's single
P2PKH or P2SH output. Ignoring the reserved bytes keeps prerelease records, which
appended the address there, decoding to their index. Incoming indices are
recovered through lookahead, not this memo. The decoder distinguishes unrelated
memos from unsupported versions or purposes. Callers must retain unsupported
records as incomplete recovery work.

## Validation

From the workspace root:

```sh
cargo test -p zakura-swap-receiving --locked
```

The tests check independent Python HMAC/scalar vectors, purpose separation,
boundary indices, retry behavior, authority substitutions, and malformed memos.
Regenerate the vectors from this directory with
`python3 tests/vectors/generate.py > tests/vectors/rivk.csv`.

The protocol POC builds Ironwood outputs and discards the derived keys. It
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
metadata. Outside private recovery mode, registered keys remain in subsequent compact scans.
Full-transaction retrieval authenticates swap memos before or after compact
scanning, including self-payments also recoverable through the ordinary OVK.
Enhance PIR resolves the registered key after restart and rejects altered
ciphertext without clearing pending work. A build without swap support reports
an error for that retrieval instead of trying the ordinary account key.

Per-key historical coverage, retirement,
automatic seed restore and gap extension, PCZT/firmware qualification, Vizor,
and receiver PIR remain required before live use. Registered keys queue missing historical scans by default.

## Protocol baseline

Zcash Protocol Specification **v2026.7.0-202-gafa086, NU6.3 proposal**, commit
`afa086bd976e316612a5c06fb139429958d07d84`:

- [§4.2.3, Key Components, pp.40–41](https://github.com/zcash/zips/blob/afa086bd976e316612a5c06fb139429958d07d84/protocol/protocol.tex#L5753-L5909),
  PDF anchor `orchardkeycomponents`.
- [§5.6.4.4, Raw Full Viewing Keys, p.123](https://github.com/zcash/zips/blob/afa086bd976e316612a5c06fb139429958d07d84/protocol/protocol.tex#L13077-L13104),
  PDF anchor `orchardfullviewingkeyencoding`.

The swap KDF and memo format are proposed wallet conventions, separate from
these protocol requirements. Passing the POC is not a cryptographic review.

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

The SQLite `apply_pending_swap_payment` helper commits a queued candidate only
at the wallet's fully scanned tip. It authenticates ciphertext again, checks
receipt bounds and retained spend evidence, and verifies a supplied witness at
its explicit local block anchor (or uses an available local witness). It marks
the leaf for future witness updates and writes the note, memo, key and known
spend atomically. Incomplete inputs preserve the queue without adding balance.
A synthetic test spends a privately imported note into ordinary internal change.
Transaction IDs and Action indices remain directory assertions, checked for
conflicts with local data. The inclusion proof binds the commitment and position.

`enable_private_swap_recovery` opts an account out of automatic key-history
replay. Enable it before the first scan. This prototype retains the shared
nullifier map without pruning while any account uses the policy, trading storage
for locally verifiable spend history. Large scan batches also retain every block
instead of skipping old entries at a contiguous scan frontier. Both decisions use
the same store policy. Enabling it cannot repair evidence pruned or skipped by
earlier scans. Those gaps require rescanning. `mark_swap_directory_checked` requires a local block anchor and no pending
candidates. Rewinds remove checks above the retained height. This does not retire
keys or claim that a provider's terminal status rules out future payments.

### Bounded private recovery policy

`prepare_swap_discovery_batch` selects at most 64 metadata records in Vizor and
reports all remaining uncached lookups for transport selection. Lease each record
when its attempt starts. Persist a complete lookup and authenticated ciphertexts
atomically with `queue_swap_lookup`, then apply queued notes using independently
accepted inclusion and spend evidence. `finish_swap_discovery_attempt` records
processed coverage and schedules closeout or another follow-up.

Local operations scan without a count cap. Restored operations use directory
work only. Two days without a supported status observation moves a local watch
to directory follow-ups without marking it complete. Fresh active observations
have no age limit. Follow-ups back off from one to twelve hours. Completed work
makes no routine requests. Reorgs reopen affected coverage and candidates.

Historical spend retention follows the earliest coverage gap or pending note,
independently of provider completion. Missing spend evidence queues one coalesced
replay of the public account recovery interval. A note before that interval
stays explicitly blocked until the account's restore range is widened.

Address issuance preferences do not disable recovery. Directory discovery can
use encrypted PIR or the identical common row file. Full ciphertext retrieval
and witness validation are unchanged, including when issuance is switched off.
