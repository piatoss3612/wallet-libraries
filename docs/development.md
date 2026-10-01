# Development workflow

Python 3.11+, Rust, and Git are sufficient for ordinary development on macOS
and Linux. `rust-toolchain.toml` pins the CI/development compiler; the facade
verification also checks a fresh Rust 1.91 consumer. Protobuf regeneration
requires the pinned compiler described below; ordinary builds do not.

## Commands

```sh
python3 scripts/dev.py doctor --config transparent
python3 scripts/dev.py test --config transparent -p zakura-client-sqlite transparent_ledger
python3 scripts/dev.py check --config orchard -p zakura-client-backend
python3 scripts/dev.py lint --config transparent -p zakura-client-sqlite
python3 scripts/dev.py verify --only wallet-lib-modes
python3 scripts/dev.py verify
```

| Configuration | Packages and features |
| --- | --- |
| `default` | Workspace except the facade and Enhance PIR; default features |
| `orchard` | Backend + SQLite; `orchard,test-dependencies` |
| `transparent` | Backend + SQLite; `orchard,transparent-inputs,test-dependencies,unstable` |
| `transparent-import` | Backend + SQLite; transparent features plus `transparent-key-import` |
| `sqlite` | SQLite; `test-dependencies`, without Orchard |
| `enhance-wallet` | Enhance PIR; `wallet` |
| `enhance` | Enhance PIR; default features |

The Python configuration table is authoritative. `-p` replaces the package
selection; features still come from the selected configuration. Test-name
filters use libtest substring matching; add `--exact` for exact matching.
An empty selection fails before execution. Unfiltered commands include
doctests. `verify` runs repository checks and all configurations; use it for
final evidence rather than repeatedly during small edits.

`lint` uses Clippy with `--no-deps`, without globally promoting inherited
warnings to errors. Inspect warnings on changed code and compare with the base
when needed. `cargo fmt --all -- --check` remains the read-only formatting check.
A runtime/sandbox rejection is an environment issue; formatting does not need
broader permissions, Cargo compilation, or a shared build lock.

## Build ownership and results

Build directories live under `~/.cache/wallet-libraries/targets/`, keyed by
compiler identity, target platform, configuration, and profile. Set
`WALLET_LIB_BUILD_ROOT` to relocate them. Each invocation takes a nonblocking
OS lease on a persistent numbered directory. Concurrent calls use different
numbers; later calls reuse released directories across worktrees. Cargo still
checks source/dependency fingerprints. Source revisions are result provenance,
not cache keys. Do not run other Cargo processes directly inside leased directories.

`owner.json` records the active process, checkout, and source inputs;
`last-result.json` records duration, source identity, exit code, and status.
Changing HEAD or source inputs during a command invalidates success (exit 3,
status `invalidated`); the receipt lists `changed_inputs` and `ignored_changes`
by category.

Source inputs follow a versioned policy recorded in each receipt. `check`,
`test`, and `lint` use `rust-check`: it may ignore root `docs/**/*.md` prose
and a root `CHANGELOG.md`, but only when an audit proves nothing Cargo can
reach consumes the page. Any reader the audit cannot resolve makes the run use
every file, with `fallback_reason` in the receipt; there is no exception for
run-time or computed paths.

The audit starts from every package file (including Git-ignored ones and a
root package), every manifest including `metadata` values and path-shaped
keys, and Cargo configuration. Documentation is never a starting point. It
follows each string literal (escapes decoded, spaces allowed, each line also
lexed alone) to the files it may name. `include!`, `#[path]`, and
`#[doc = include_str!]` targets are audited as Rust whatever their extension,
and a reached Rust file reaches the modules beside and below it. A page that is
reached, or whose file name or extensionless name is mentioned (compared
case-insensitively, including comments and symlinks), stays an input. These
fall back to every file:

- non-literal `include_*!`, any `docs` segment that does not name an existing
  file, and literals that concatenation could join into `docs`;
- run-time file access (`fs`, `File`, `OpenOptions`, `Connection::open`,
  `Path::new`, and similar) whose path is not one string literal;
- upward navigation: parent paths or placeholder file names at the checkout
  root, pure `..` literals, `Component::ParentDir`, `.parent()`/`.ancestors()`,
  and `current_dir` or `CARGO_MANIFEST_DIR` combined with `.pop()` or `..`;
- directory walkers (`read_dir`, `walkdir`, `ignore`, `glob`, `globwalk`, and
  similar) and checkout-relative Cargo `[env]` values;
- every process launch other than the package's own `CARGO_BIN_EXE_*`
  binaries and the reviewed `sqlite3 <db> -safe -readonly <sql>` call in
  `zcash_client_sqlite/src/testing/db.rs` (`-safe` blocks `readfile()`,
  `ATTACH`, and `.read`; any other `sqlite3` launch or argument, such as
  `--nonce`, falls back), Cargo `runner` settings, and work-tree readers
  (`git2`, `gix`, `vergen`);
- symlinked documentation directories, nested repositories and submodules,
  unreadable sources, and path dependencies outside the checkout.

Ignored files that the audit finds consumed (named, included, or compiled as
modules) join the input digest under every policy. Package Markdown, doctests,
fixtures and assets of any extension, build-script inputs, manifests, and
`Cargo.lock` always remain inputs. `verify` and other commands hash every file.
This checkout currently falls back: run-time database and file paths in
`zcash_client_sqlite`, `zcash_client_backend`, and its `build.rs` cannot be
proven to avoid documentation.
`WALLET_LIB_CARGO_ORACLES=1 python3 -m unittest scripts/tests/test_dev_cargo.py`
runs real Cargo negative oracles: editing an excluded page must not change a
fresh `cargo test` result. Bump `POLICY_VERSION` with any rule change. OS locks
release on exit or process death; a stale owner file is replaced on reuse.
Doctor reports recorded owners, which may be stale after a killed process.
Keep rust-analyzer's target directory separate (for example,
`rust-analyzer.cargo.targetDir = true`). No caches belong in Git.

For durable checks use the installed roman-dev-ux skill's helper:

```sh
python3 ~/.agents/skills/roman-dev-ux/scripts/start-local-check.py \
  --tool codex --session YOUR_SESSION --cwd "$PWD" -- \
  python3 scripts/dev.py test --config transparent -p zakura-client-sqlite transparent_ledger
```

The helper returns PID, owner, log, result path, and HEAD. The development
command additionally checks source inputs. Inspect both results. This retains
completion after a turn ends; automatic agent wake depends on the runtime.
Use the native skill to record commands over 60 seconds and actual blockers,
without credentials or raw output in observations.

Keep the optimized `test` profile for final checks. To evaluate an iteration
profile without editing generated Cargo.toml, use `--profile iteration`. The
opt-in candidate in `.cargo/config.toml` keeps dependencies optimized and lowers
workspace optimization/debug information. Measure cold setup and repeated edits
separately before adopting it for a workload. Do not adopt a slower aggregate
compile-and-test configuration merely because compilation alone improves.

## Protobufs

`python3 scripts/proto.py check` generates into Cargo build output and compares
with all three checked-in bindings. `python3 scripts/proto.py write` explicitly
updates them. Both require `libprotoc 34.1`; set `PROTOC` to that executable.
The locked Rust generator dependencies determine the remaining output.
Source packages without `.proto` files keep using checked-in bindings.

## CI and completion

CI runs existing configurations independently, retains doctests and repository
checks, and publishes a final `tests` result. Documentation/guidance-only PRs
run lightweight checks. Unknown file types conservatively trigger full checks.
Older revisions of the same PR cancel; main runs remain independent. Compiler
and configuration specific caches accelerate compilation without retaining the
fresh external-consumer lockfiles.

See [development-speed.md](development-speed.md) for evidence, acceptance results,
and outstanding measurements. A passing focused check does not prove all
privacy, migration, protocol, or feature combinations; use their affected lanes
and final CI. Keep process-local proving-key caches and the cargo test runner.
