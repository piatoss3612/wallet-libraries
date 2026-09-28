# Security Disclaimer

This is a beta build, and is currently under active development. Please be advised
of the following:

* This code currently is not audited by an external security auditor, use it at
  your own risk.
* The code **has not been subjected to thorough review** by engineers at the Electric Coin Company.
* We **are actively changing** the codebase and adding features where/when needed.

----

# zcash_client_sqlite

This library contains APIs that collectively implement a Zcash light client in
an SQLite database.

## Experimental swap receiving

The `experimental-swap-receiving` feature exposes a durable receiving-key
registry through `WalletDb`. It supports atomic refund/receive reservations,
authenticated recovery registration, and incoming lookahead that does not
advance allocation. Reservation and application operation state can share a
`transactionally_with_extension` transaction. Expose the address only after
commit, and reuse its key ID when retrying that operation.

The feature is disabled by default. The migration creates its table in every
build so feature changes preserve existing reservations. Compact scanning and
software spending retain each note's derived key. Change uses ordinary account
keys. See the [shared POC contract](../../zakura/swap-receiving/README.md).

`queue_swap_payment` authenticates privately retrieved ciphertext under a
registered key and persists a `PendingPayment`. `pending_swap_payments` reloads
these candidates after reopening. Queuing does not credit balance, advance a
lookahead sequence, or complete recovery. Conflicting output identities are
rejected. Rewinds discard candidates above the retained height while keeping
their receiving keys for another lookup.

`swap_payment_spend_status` checks locally derived nullifiers against both known
wallet spends and retained unlinked spends. Absence means `Unspent` only when
every block from receipt through the accepted scan anchor retains its unlinked
nullifiers. Pruning and rewinds trim that coverage. Account deletion clears it
because it removes known-spend evidence. Existing scans get no inferred
coverage during migration. Otherwise absence is `Unknown`, not evidence of an
unspent note. The method also works inside the transaction helper so eventual
note insertion can share its database snapshot.

`apply_pending_swap_payment` verifies commitment inclusion and position against
the accepted tree before inserting a note. It accepts a supplied witness or one
already built by local scanning. Incomplete evidence leaves the candidate queued.

Enable the recovery policy before the first scan to retain spend evidence.
Address issuance preferences must not gate memo recovery or incoming lookahead.
With private queries selected, use `prepare_swap_recovery_target` and
`swap_recovery_needs_directory` to find unfinished lookups. When the user selects
ordinary retrieval, `queue_swap_recovery_scan` explicitly queues missing local
coverage instead. It is not an automatic fallback on PIR failure.

Restored keys have a fixed target at their first accepted tip. Repeating recovery
does not extend their watch as the tip moves. Local operations keep their pending
watch and terminal grace deadline, and still require the final directory check
when private queries are enabled again. After recovering a payment, maintain the
incoming lookahead and finish the new window before reporting restore complete.

## License

Licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
