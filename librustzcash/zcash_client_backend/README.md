# zcash_client_backend

This library contains Rust structs and traits for creating shielded Zcash light
clients.

## Building

Ordinary builds use checked-in GRPC bindings, without requiring `protoc` or
modifying source files. From the repository root, run
`python3 scripts/proto.py check` to verify them or
`python3 scripts/proto.py write` after changing `.proto` files. Generation
requires the pinned compiler in `scripts/protoc-version.txt`.

## License

Licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
   http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

