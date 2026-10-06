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

The `experimental-swap-receiving` feature adds swap receiving to `WalletDb`: a
durable registry of derived refund and incoming keys, their scanning, seed
recovery through receiver-directory sweeps, and spending. It is disabled by
default. The migration creates its tables in every build, so feature changes
preserve existing reservations. Compact scanning and software spending retain each
note's derived key; change uses ordinary account keys. The wallet calls and their
contracts are described in the [shared contract](../../zakura/swap-receiving/README.md).

A refund reservation can share a `transactionally_with_extension` transaction with
application operation state. Expose the address only after commit, and reuse its
key ID when retrying that operation.

A restore sweep credits a directory-reported payment only after the wallet
authenticates its ciphertext, verifies its inclusion against its own chain, and
checks its spend state against locally retained Ironwood nullifiers. Incomplete
evidence leaves the payment queued without crediting balance, and a directory
failure never queues a public replay.

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
