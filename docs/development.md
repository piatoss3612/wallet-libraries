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

The audit starts from every file of the packages the command builds: the
`-p` selection with its dev-dependencies, then their normal and build
dependencies (`--workspace` selects every package). It includes Git-ignored
files and a root package, every manifest including `metadata` values and
path-shaped keys, and Cargo configuration. Documentation is never a starting point. It
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
  `Path::new`, and similar, including turbofish calls) whose path is not one
  whole string literal (`"../CHANGE".to_owned() + "LOG.md"` is computed), and
  path probes such as `.exists()` or `.metadata()`, unless it is a reviewed
  reader (below);
- file access under another name: a `use` that renames or globs file access
  or a reviewed wrapper (`use std::fs as f;`, `use std::fs::*;`), imports a
  file-system function by its bare name (`use std::fs::read;`), or renames an
  `include_*!` macro; and file-system functions, openers, or wrappers used as
  values (`let load = fs::read_to_string;`, `.map(File::open)`);
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

Run-time rules apply only to code the operation executes. `check` and `lint`
execute build scripts and what they reach, including modules a build script
declares (`mod helper;`), proc-macro crates, and path build-dependencies with
their own dependencies. A selected package keeps its dev-dependencies even
when another selected package also depends on it. `test` also executes library,
test, and doctest code, plus examples when an `[[example]]` sets
`test = true`, and targets a manifest places by `path`. Code in packages the selection does
not build never runs.

`scripts/audited-readers.toml` lists reviewed run-time readers whose computed
paths cannot name documentation: temporary files and directories, the test
wallet's temporary database, and build-script output. Each entry is bound to
its file, enclosing function, and exact call text, through the call's closing
parenthesis, with the reason it is safe. Its `context` digest also covers the
whole enclosing function (every path initializer), everything in the file
outside function bodies (imports, constants, statics, macros), and every
function or `macro_rules!` it calls that is defined anywhere in the checkout,
transitively, with those files' outside-function text. Calls resolve by name,
so every same-named definition counts; in this workspace that reaches most
sources, and most Rust edits stale most entries until they are reviewed again.
String and character literals and doc comments are bound byte for byte; only
whitespace and ordinary comments elsewhere are ignored. Any change, or a
missing digest, makes the entry stale. A parameter that flows into a
reviewed reader (by assignment, binding, receiver mutation, or match arm) is a
caller input: the function must be a `[[wrapper]]`, so every caller is
reviewed, or the entry must state in `caller_input` why the input cannot carry
a path. That statement is bound by the same digest. Every identifier in a
parameter pattern (`P: String`, `(p,): (String,)`) is a parameter; a parameter
list that does not parse counts as a caller input.

The audit reads Rust through a lexer: comments cannot split a path, a `use`,
or a `mod` (`use/*x*/std::fs as f;`), raw identifiers read as their names
(`std::r#fs`), and a `macro_rules!` template that substitutes a path segment
after file access, a path prefix, or a callee (`std::fs::$reader($path)`)
falls back. Full inputs (`verify`, or any fallback) hash every Git-ignored
file except build output (`target`, `.git`, `.vscode`, `__pycache__`), so an
unknown reader of an ignored file outside every package still invalidates. `[[wrapper]]` names reviewed APIs
that open a caller's path, and every call site of one, qualified or not
(`crate::WalletDb::for_path(…)`), needs its own entry. An
edited call, a new reader, or a stale entry falls back until the registry is
reviewed again. The registry is itself an input, so editing it invalidates
results.

Git-ignored files the audit scans join the input digest under every policy:
every ignored file in a built package (implicit `mod` modules included) and any
ignored file a scanned file names. Package Markdown, doctests,
fixtures and assets of any extension, build-script inputs, manifests, and
`Cargo.lock` always remain inputs. `verify` and other commands hash every file.
On this checkout, every computed reader is reviewed, so unrelated root pages
such as `docs/development.md` stay excluded. Removing the registry makes the
policy fall back.
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
