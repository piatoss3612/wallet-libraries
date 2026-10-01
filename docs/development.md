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
`test`, and `lint` use `rust-check`: it ignores only root `docs/**/*.md` prose
and a root `CHANGELOG.md`, and only when nothing Cargo can reach mentions the
file. The audit scans every package file (including Git-ignored ones and a
root package), every manifest including `metadata` tables, and Cargo
configuration, then follows each string literal (escapes decoded, spaces
allowed, each line also lexed alone) to the files it may name, so Markdown
doctests and included modules are scanned in turn. A file-name mention,
compared case-insensitively and including comments and symlinks, keeps that
page an input. Computed or unresolved references fall back to every file and
record `fallback_reason`: non-literal `include_*!`, any `docs` segment that does
not name an existing file, literals that concatenation could join into `docs`,
parent paths naming the checkout root, directory walks, manifest-relative
parent paths, any process launch except audited `sqlite3`, symlinked
documentation directories, unreadable sources, and path dependencies outside
the checkout. Package Markdown, doctests, fixtures and assets of any
extension, build-script inputs, manifests, and `Cargo.lock` always remain
inputs. `verify` and other commands hash every file. Paths supplied at run time
(environment variables, arguments) or spelled character by character are not
audited. Bump `POLICY_VERSION` with any rule change. OS locks
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
