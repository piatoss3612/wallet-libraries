#!/usr/bin/env python3
"""Focused wallet checks with reusable, exclusively owned Cargo build directories."""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import tomllib

ROOT = Path(__file__).resolve().parents[1]
CONFIGS = {
    "default": (["--workspace", "--exclude", "zakura-wallet-lib", "--exclude", "zakura-pir-enhance"], []),
    "orchard": (["-p", "zakura-client-backend", "-p", "zakura-client-sqlite"], ["orchard", "test-dependencies"]),
    "transparent": (["-p", "zakura-client-backend", "-p", "zakura-client-sqlite"], ["orchard", "transparent-inputs", "test-dependencies", "unstable"]),
    "transparent-import": (["-p", "zakura-client-backend", "-p", "zakura-client-sqlite"], ["orchard", "transparent-inputs", "transparent-key-import", "test-dependencies", "unstable"]),
    "sqlite": (["-p", "zakura-client-sqlite"], ["test-dependencies"]),
    "enhance-wallet": (["-p", "zakura-pir-enhance"], ["wallet"]),
    "enhance": (["-p", "zakura-pir-enhance"], []),
    "transparent-pir": (["-p", "zakura-pir-transparent"], ["wallet"]),
}
VERIFY = ("zakura-graph", "wallet-lib-modes", "vendor-ancestry")


def capture(argv: list[str], **kwargs) -> str:
    return subprocess.check_output(argv, cwd=ROOT, text=True, **kwargs).strip()


# Input policies decide which repository files attribute a result. Cargo-only
# commands may ignore audited root documentation prose and the root changelog;
# every other command, and any reference this scanner cannot resolve, hashes
# every file. Bump the version whenever the exclusion or scanning rules change.
POLICY_VERSION = 8
RUST_CHECK_COMMANDS = {"check", "test", "lint"}
COMPUTED_INCLUDE = re.compile(r"\binclude(?:_str|_bytes)?!\s*[(\[{](?!\s*[bc]?r?#*\")")
# Directory walks and manifest-relative parent paths are computed references.
TRAVERSAL = re.compile(r"\b(?:read_dir|iterdir|listdir|scandir|os\.walk|walkdir|jwalk|glob|globwalk|globset|rglob|GlobWalker\w*|WalkBuilder|WalkDir|CARGO_WORKSPACE_DIR|workspace_root)\b|\b(?:ignore|Walk|walk)::|\.(?:parents?|ancestors)\b")
# A run-time base directory combined with any upward step may reach the root.
BASE_DIRECTORY = re.compile(r"\bcurrent_dir\b|\bCARGO_MANIFEST_DIR\b|__file__|\$0\b|BASH_SOURCE")
UPWARD = re.compile(r"\.pop\(\)|\.\.|\bdirname\b|\bParentDir\b")
# Any launched program may read the checkout; only audited programs are exempt.
SPAWN = re.compile(r'\bCommand\s*::\s*new\s*\(\s*(?:"([^"]*)"\s*\)|(env!\(\s*"CARGO_BIN_EXE_\w+"\s*\)))?|\bcmd!\s*[(\[{]|\b(?:duct|xshell|cmake|cc|autotools|meson|subprocess)::|\bCommand\s+as\b|\b(?:libc|unistd)::(?:system|exec\w*|posix_spawn\w*|fork)\b|\bsubprocess\.|\bos\.(?:system|exec\w*|spawn\w*|popen)\b')
# Libraries that read the whole work tree, such as Git status for build info.
WORKTREE_READERS = re.compile(r"\b(?:git2|gix|vergen\w*|built)::")
# The one audited sqlite3 launch: `-safe` disables readfile(), ATTACH, `.read`
# and other external file access except the named database, and `-readonly`
# forbids writes; no `--nonce` (which re-enables them), `-init`, or other
# argument may appear. Bound to its file and function; any other sqlite3
# launch, or any change to this one, falls back until reviewed again.
AUDITED_SQLITE = ("librustzcash/zcash_client_sqlite/src/testing/db.rs", "unsafe fn run_sqlite3<S: AsRef<OsStr>>(db_path: S, command: &str) {", 'Command::new("sqlite3") .arg(db_path) .arg("-safe") .arg("-readonly") .arg(command) .output()')
# Run-time file access whose path is not one string literal may reach any
# file, so it is an unknown reader (the policy cannot prove where it points).
RUNTIME_READER = re.compile(r'(?:\b(?:fs|File|Connection|OpenOptions|Path|PathBuf|Dir|tokio::fs)::(?:\w+)|\.open(?:_with_flags)?|\bread_to_string|(?<!fn )\bopen)\s*\(\s*(?![bc]?r?#*")(?!\))')
# Literal targets of `include!`, `#[path]`, and `#[doc = include_str!]` are
# compiled as Rust (doctests for Markdown), whatever their extension.
RUST_TARGET = re.compile(r'\binclude!\s*[(\[{]\s*[bc]?r?(#*)"(.*?)"\1|#\s*\[\s*path\s*=\s*r?(#*)"(.*?)"\3|\bdoc\s*=\s*include_str!\s*[(\[{]\s*r?(#*)"(.*?)"\5', re.S)
PATH_TOKEN = re.compile(r"[\w.:/\\-]+")
WORD = re.compile(r"""[^\s'"`<>|;&()]+""")
# Rust and doctest literals: raw strings, escaped strings, and character
# literals (skipped so '"' does not pair quotes wrongly). Other files also
# use single-quoted strings.
RUST_LITERAL = re.compile(r'(?<!\w)[bc]?r(#*)"(.*?)"\1|(?<!\w)[bc]?"((?:[^"\\]|\\.)*)"|\'(?:\\.|[^\'\\\n])\'', re.S)
OTHER_LITERAL = re.compile(r'"((?:[^"\\\n]|\\.)*)"|\'((?:[^\'\\\n]|\\.)*)\'')
ESCAPE = re.compile(r"\\(?:x([0-9a-fA-F]{2})|u\{([0-9a-fA-F_]{1,8})\}|u([0-9a-fA-F]{4})|U([0-9a-fA-F]{8})|([0-7]{1,3})|(\r?\n\s*)|(.))", re.S)
# `docs` as a path segment in any case: case-insensitive file systems resolve it.
DOCS_WORD = re.compile(r"(?<![\w.-])docs(?![\w.-])", re.I)
TERMINATOR = re.compile(r"[\s.,;:)\]`'\"#?]")
# Pieces that concatenation could join into `docs`: code anywhere Cargo reaches
# with a literal ending in a leading piece and one starting with a trailing piece.
DOCS_HEAD = re.compile(r"(?:^|/)(?:d|do|doc)$", re.I)
DOCS_TAIL = re.compile(r"^(?:ocs|cs|s)(?:/|$)", re.I)
PARENT = re.compile(r"(?:^|/)\.\.(?:/|$)")
BUILD_OUTPUT = {"target", ".vscode", ".git"}
# Files that can compute a path: Rust and doctests, scripts, and executables.
# Everything else, including `include!` targets of any suffix and build-tool
# inputs such as CMakeLists.txt, is treated as code that may compute a path.
DATA_SUFFIXES = {".json", ".hex", ".lock", ".csv", ".svg", ".proto", ".bin", ".sql", ".yml", ".yaml", ".png", ".jpg"}
# Literals whose file name is a placeholder may name any file in their folder.
PLACEHOLDER = set("{}$*?%<>")


def excludable(name: str) -> bool:
    """Root `docs/**/*.md` prose and the root changelog; nothing else."""
    return name == "CHANGELOG.md" or (name.startswith("docs/") and name.endswith(".md"))


def repository_files() -> dict[str, str]:
    """Digest every tracked/untracked file except editor preferences and output."""
    names = subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=ROOT).split(b"\0")
    files = {}
    for name in sorted(set(names) - {b""}):
        path = ROOT / os.fsdecode(name)
        if path.parts[len(ROOT.parts)] in {".vscode", "target"}:
            continue
        try:
            files[os.fsdecode(name)] = hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else "<missing>"
        except OSError as error:
            files[os.fsdecode(name)] = f"<unreadable {type(error).__name__}>"
    return files


def ignored_files() -> set[str]:
    """Git-ignored files outside build output; Cargo can still read them."""
    entries = subprocess.check_output(["git", "ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory"], cwd=ROOT).split(b"\0")
    found = set()
    for entry in map(os.fsdecode, set(entries) - {b""}):
        if BUILD_OUTPUT & set(entry.rstrip("/").split("/")):
            continue
        if not entry.endswith("/"):
            found.add(entry)
            continue
        for directory, subdirectories, names in os.walk(ROOT / entry):
            # Directory symlinks are not descended; record them for the symlink audit.
            names += [d for d in subdirectories if (Path(directory) / d).is_symlink()]
            subdirectories[:] = [d for d in subdirectories if d not in BUILD_OUTPUT and not (Path(directory) / d).is_symlink()]
            found.update((Path(directory) / name).relative_to(ROOT).as_posix() for name in names)
    return found


def toml_strings(value):
    """Every string key and value, including `metadata` tables build scripts read."""
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for key, item in value.items():
            # Path-shaped keys only: `[package.metadata.docs.rs]` is not a path.
            if re.search(r"[/\\]|\.\w+$", key):
                yield key
            yield from toml_strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from toml_strings(item)


def toml_paths(value):
    """Values of `path` keys, such as path dependencies and patches."""
    if isinstance(value, dict):
        for key, item in value.items():
            if key == "path" and isinstance(item, str):
                yield item
            else:
                yield from toml_paths(item)
    elif isinstance(value, list):
        for item in value:
            yield from toml_paths(item)


def unescape(literal: str) -> str:
    """Decode Rust and Python string escapes so `\\x64ocs` still names `docs`."""
    def replace(match):
        digits = match[1] or (match[2] or "").replace("_", "") or match[3] or match[4]
        if digits:
            return chr(int(digits, 16)) if int(digits, 16) <= 0x10FFFF else "\ufffd"
        if match[5]:
            return chr(int(match[5], 8))
        if match[6]:
            return ""
        return {"n": "\n", "t": "\t", "r": "\r"}.get(match[7], match[7])
    return ESCAPE.sub(replace, literal)


def literals(name: str, text: str) -> set[str]:
    """Raw and decoded string literals, with `/` separators.

    Each line is also lexed alone, so one stray quote in a comment cannot
    turn the rest of a file's literals into apparent code.
    """
    pattern = RUST_LITERAL if name.endswith((".rs", ".md")) else OTHER_LITERAL
    found = []
    for chunk in [text, *text.splitlines()]:
        for match in pattern.finditer(chunk):
            literal = next((group for group in match.groups()[1:] if group is not None), None) if pattern is RUST_LITERAL else match[1] if match[1] is not None else match[2]
            if literal is not None:
                found.append(literal)
    return {re.sub(r"/+", "/", variant.replace("\\", "/")) for literal in found for variant in (literal, unescape(literal))}


class Index:
    """Repository paths compared case-insensitively, as macOS resolves them."""

    def __init__(self, known: set[str]):
        self.known = known
        self.paths: dict[str, set[str]] = {}
        self.names: dict[str, set[str]] = {}
        self.directories: dict[str, set[str]] = {}
        for name in known:
            self.paths.setdefault(name.casefold(), set()).add(name)
            self.names.setdefault(name.rsplit("/", 1)[-1].casefold(), set()).add(name)
            for parent in Path(name).parents[:-1]:
                self.directories.setdefault(parent.as_posix().casefold(), set()).add(name)
        self.docs = sorted((k[5:] for k in self.paths if k.startswith("docs/")), key=len, reverse=True)

    def under(self, target: str) -> set[str]:
        return self.paths.get(target.casefold(), set()) | self.directories.get(target.casefold(), set())


def documentation(literal: str, index: Index) -> tuple[set[str], str | None]:
    """Pages a literal names under `docs/`, or why it may name an unknown one."""
    if "://" in literal:
        return set(), None
    named = set()
    for match in DOCS_WORD.finditer(literal):
        rest = literal[match.end():]
        if not rest.startswith("/"):
            return named, f"unresolved documentation reference {literal!r}"
        folded = rest[1:].casefold()
        # Prose such as "see docs/guide.md#setup" names the longest existing path.
        page = next((p for p in index.docs if folded == p or folded.startswith(p) and TERMINATOR.match(folded, len(p))), None)
        if page is None:
            return named, f"unresolved documentation reference {literal!r}"
        named |= index.paths[f"docs/{page}"]
    return named, None


def audited_sqlite(name: str, text: str, spawn: re.Match) -> bool:
    """Whether a launch is exactly the reviewed `-safe -readonly` sqlite3 call."""
    path, function, chain = AUDITED_SQLITE
    if name != path or text.count("Command::new") != 1:
        return False
    start = text.rfind(function, 0, spawn.start())
    following = " ".join(text[spawn.start():].split())
    return start >= 0 and "\nfn " not in text[start:spawn.start()] and following.startswith(chain + " ")


def computed(text: str, found: set[str], name: str = "") -> list[str]:
    """Constructs in code that may build a path this scanner cannot resolve."""
    reasons = []
    if COMPUTED_INCLUDE.search(text):
        reasons.append("computed include")
    if TRAVERSAL.search(text) or BASE_DIRECTORY.search(text) and UPWARD.search(text):
        reasons.append("computed directory traversal")
    if WORKTREE_READERS.search(text):
        reasons.append("work-tree reader")
    # The package's own binaries (`CARGO_BIN_EXE_*`) are built from scanned
    # sources. Other programs, including unrestricted sqlite3 whose SQL can
    # read files, are exempt only as the exact audited invocation.
    if any(not spawn[2] and not audited_sqlite(name, text, spawn) for spawn in SPAWN.finditer(text)):
        reasons.append("process launch that may read any file")
    if RUNTIME_READER.search(text):
        reasons.append("run-time file access with a computed path")
    # Pure upward navigation such as `Path::new("..").join("..")` can reach the root
    # from any package depth.
    if any(re.fullmatch(r"[./]*\.\.[./]*", literal) for literal in found) or re.search(r"\bParentDir\b", text):
        reasons.append("parent directory navigation")
    return reasons


def follow(name: str, literal: str, package: str, index: Index) -> tuple[set[str], str | None]:
    """Repository files a literal may name relative to its file or package.

    Cargo runs build scripts and tests in the package directory; outside a
    package the root stands in. A parent path that names the checkout root or
    one of its ancestors may be walked or joined to anything. A bare file name that resolves nowhere may be joined to
    a computed directory, so every file with that name is reached.
    """
    literal = literal.strip()
    if "://" in literal or "\n" in literal or not literal:
        return set(), None
    reached = set()
    for base in {str(Path(name).parent), package}:
        target = os.path.normpath(os.path.join(base, literal.lstrip("/")))
        folder, _, leaf = target.rpartition("/")
        # Only a parent path, or a root package's own code, runs from the root.
        from_root = PARENT.search(literal) or package == "" and not name.endswith(".toml")
        path_like = "/" in literal or re.search(r"\.\w+$", literal)
        if from_root and path_like and not re.search(r"\s", literal) and PLACEHOLDER & set(leaf) and (not folder.strip("./") or PLACEHOLDER & set(folder)):
            return reached, f"computed file name {literal!r} at the checkout root"
        if target in {".", ""} or not target.strip("./"):
            if PARENT.search(literal):
                return reached, f"parent path {literal!r} reaches the checkout root"
            continue
        if target.startswith(".."):
            continue
        reached |= index.under(target)
    leaf = literal.rsplit("/", 1)[-1]
    if not reached and "." in leaf.strip("."):
        reached = set(index.names.get(leaf.casefold(), ()))
    return reached, None


def input_policy(command: str, files: dict[str, str]) -> dict:
    """Select excluded prose for Cargo commands after auditing what Cargo can reach.

    Every file in a package (including Git-ignored ones), every manifest, and
    Cargo configuration is scanned; any file a scanned literal may name is
    scanned in turn, so Markdown doctests, build-script generators, and
    included modules are audited transitively.
    """
    policy = {"name": "rust-check" if command in RUST_CHECK_COMMANDS else "full", "version": POLICY_VERSION, "fallback_reason": None, "protected": [], "excluded": [], "consumed_ignored": []}
    candidates = {name for name in files if excludable(name)}
    ignored_names = ignored_files()
    known = set(files) | ignored_names
    index = Index(known)
    folded_candidates = {name.casefold(): name for name in candidates}
    protected: set[str] = set()
    reasons = []
    heads, tails = set(), set()
    root, docs = ROOT.resolve(), (ROOT / "docs").resolve()
    # Git lists an untracked nested checkout as one directory it does not hash.
    reasons += [f"nested repository or submodule {name}" for name in sorted(files) if name.endswith("/") or (ROOT / name).is_dir()]
    for name in sorted(known - candidates):
        path = ROOT / name
        if path.is_symlink():
            try:
                # Strict resolution reports loops on every platform.
                target = path.resolve(strict=True)
            except FileNotFoundError:
                target = path.resolve(strict=False)  # dangling: check where it would point
            except (OSError, RuntimeError):
                reasons.append(f"unresolvable symlink {name}")
                continue
            relative = target.relative_to(root).as_posix() if target.is_relative_to(root) else None
            if relative and relative.casefold() in folded_candidates:
                protected.add(folded_candidates[relative.casefold()])
            elif target.is_dir() and (docs.is_relative_to(target) or target.is_relative_to(docs)):
                reasons.append(f"symlinked directory {name} reaches documentation")
    manifests = {name for name in known if Path(name).name == "Cargo.toml"}
    packages = []
    for manifest in manifests:
        try:
            if {"package", "project"} & set(tomllib.loads((ROOT / manifest).read_text())):
                packages.append("" if manifest == "Cargo.toml" else str(Path(manifest).parent))
        except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError):
            pass  # reported when the manifest is scanned below
    packages.sort(key=len, reverse=True)
    def package_of(name: str) -> str | None:
        return next((p for p in packages if p == "" or name.startswith(p + "/")), None)
    cargo = manifests | {name for name in known if Path(name).name in {"rust-toolchain", "rust-toolchain.toml"} or name.startswith(".cargo/")}
    # Documentation is never a seed: a page is scanned only when something reaches it.
    pending = sorted((cargo | {name for name in known if package_of(name) is not None}) - candidates)
    scanned = set(pending)
    # Files Cargo consumes because a scanned file names or includes them.
    consumed: set[str] = set()
    rust_like: set[str] = set()
    while pending:
        name = pending.pop()
        path = ROOT / name
        if not path.is_file():
            continue
        try:
            rust = path.suffix == ".rs" or name in rust_like
            text = path.read_bytes().decode("utf-8", errors="strict" if rust or path.suffix == ".toml" else "replace")
            cargo_file = path.name == "Cargo.toml" or path.parent.name == ".cargo"
            try:
                document = tomllib.loads(text) if path.suffix == ".toml" or cargo_file else None
            except tomllib.TOMLDecodeError:
                # Cargo rejects its own invalid files; other TOML is scanned as text.
                if cargo_file:
                    raise
                document = None
            if document is not None:
                found = {s.replace("\\", "/") for s in toml_strings(document)}
                # Only Cargo reads path dependencies; generator inputs may name other roots.
                for dependency in toml_paths(document) if cargo_file else ():
                    try:
                        target = (path.parent / dependency).resolve()
                        missing = not target.is_relative_to(root) or not target.exists()
                    except (OSError, RuntimeError, ValueError):
                        missing = True
                    if missing:
                        reasons.append(f"unresolved path dependency {dependency!r} in {name}")
                # Relative `[env]` values let tests walk from the checkout root.
                if path.parent.name == ".cargo" and re.search(r"(?m)^\s*runner\s*=", text):
                    reasons.append(f"custom Cargo runner in {name}")
                environment = document.get("env", {}) if path.parent.name == ".cargo" else {}
                for key, value in environment.items() if isinstance(environment, dict) else [("env", environment)]:
                    text_value = str(value.get("value", "")) if isinstance(value, dict) else str(value)
                    if isinstance(value, dict) and value.get("relative") or not text_value.strip("./") or PARENT.search(text_value):
                        reasons.append(f"checkout-relative Cargo environment {key!r} in {name}")
            else:
                found = literals(f"{name}.rs" if rust else name, text)
                if rust or path.suffix not in DATA_SUFFIXES:
                    reasons.extend(f"{reason} in {name}" for reason in computed(text, found, name))
                    heads.update(name for literal in found if DOCS_HEAD.search(literal))
                    tails.update(name for literal in found if DOCS_TAIL.search(literal))
                    if not rust and path.suffix != ".md":
                        # Scripts may name paths without quotes.
                        found |= set(WORD.findall(text))
        except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
            reasons.append(f"unreadable reference source {name}: {type(error).__name__}")
            continue
        # Comments and prose can only protect a file; literals may also be
        # computed or unresolved references that force full inputs.
        tokens = {t for t in PATH_TOKEN.findall(text if document is None else "\n".join(found)) if "://" not in t} | found
        for token in tokens:
            parts = token.replace("\\", "/").strip().rstrip(".:").casefold().split("/")
            protected.update(index.names.get(parts[-1], set()) & candidates)
        # A literal naming a page without its extension (`"../CHANGELOG"` plus
        # `.with_extension("md")`) protects it too; comments do not.
        for literal in found:
            stem = literal.strip().rstrip("/").rsplit("/", 1)[-1].casefold()
            protected.update(page for page in candidates if stem and page.rsplit("/", 1)[-1].casefold().removesuffix(".md") == stem)
        # `mod name;` reaches sibling and child modules without a literal.
        if rust:
            folder = str(Path(name).parent)
            modules = {other for other in known if other.endswith(".rs")} if folder == "." else index.under(folder)
            for item in {m for m in modules if m.endswith(".rs")} - scanned:
                scanned.add(item)
                consumed.add(item)
                pending.append(item)
            targets = {unescape(m[2] or m[4] or m[6] or "").replace("\\", "/") for m in RUST_TARGET.finditer(text)}
        else:
            targets = set()
        package = package_of(name) or ""
        for literal in found:
            named, reason = documentation(literal, index)
            protected |= named & candidates
            reached, escape = follow(name, literal, package, index)
            for problem in (reason, escape):
                if problem:
                    reasons.append(f"{problem} in {name}")
            # A manifest naming a package directory does not consume every file in it.
            consumed |= reached if document is None or not cargo_file else {r for r in reached if r.rsplit("/", 1)[-1] == literal.rstrip("/").rsplit("/", 1)[-1]}
            if literal in targets:
                # Rescan a file already read as data once it is known to be Rust.
                pending.extend(sorted((reached - rust_like) & scanned))
                rust_like |= reached
            for item in reached - scanned:
                scanned.add(item)
                pending.append(item)
    if heads and tails:
        reasons.append(f"documentation path fragments in {min(heads)} and {min(tails)}")
    # Ignored files that Cargo consumes are inputs under every policy.
    policy["consumed_ignored"] = sorted(consumed & ignored_names)
    if policy["name"] == "full":
        policy["fallback_reason"] = f"{command} validates every repository input"
        return policy
    if reasons:
        policy["fallback_reason"] = "; ".join(sorted(set(reasons))[:5])
        return policy
    # A reached documentation page is an input and its doctests were scanned above.
    protected |= scanned & candidates
    policy["protected"] = sorted(protected)
    policy["excluded"] = sorted(candidates - protected)
    return policy


def ignored(policy: dict, name: str) -> bool:
    """Whether a policy proves the path cannot affect the Cargo result."""
    return policy["name"] == "rust-check" and not policy["fallback_reason"] and excludable(name) and name not in policy["protected"]


def category(name: str) -> str:
    path = Path(name)
    if excludable(name):
        return "documentation-prose"
    if path.name == "Cargo.lock":
        return "lockfile"
    if path.name in {"Cargo.toml", "rust-toolchain.toml"} or name.startswith(".cargo/"):
        return "cargo-manifest-or-config"
    if path.name == "build.rs" or path.suffix == ".proto":
        return "build-script-input"
    if path.suffix == ".rs":
        return "rust-source"
    if path.suffix in {".md", ".mdc"}:
        return "included-markdown"
    if {"tests", "fixtures", "assets", "testdata"} & set(path.parts):
        return "fixture-or-asset"
    return "other"


def source_state(command: str = "verify") -> dict:
    """Attribute results to HEAD and the source inputs selected by the command's policy."""
    files = repository_files()
    policy = input_policy(command, files)
    for name in policy["consumed_ignored"]:
        try:
            files[name] = hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
        except OSError as error:
            files[name] = f"<unreadable {type(error).__name__}>"
    files = dict(sorted(files.items()))
    digest = hashlib.sha256()
    for name, value in files.items():
        if not ignored(policy, name):
            digest.update(os.fsencode(f"{name}\0{value}\0"))
    return {"sha": capture(["git", "rev-parse", "HEAD"]), "inputs": digest.hexdigest(), "policy": policy, "files": files}


def receipt(state: dict) -> dict:
    return {k: v for k, v in state.items() if k != "files"}


def compare(before: dict, after: dict) -> tuple[list[str], list[str]]:
    """Changed and ignored input categories; HEAD always requires a fresh result."""
    changed, skipped = set(), set()
    if before["sha"] != after["sha"]:
        changed.add("head")
    protected = set(before["policy"]["protected"]) | set(after["policy"]["protected"])
    for name in set(before["files"]) | set(after["files"]):
        if before["files"].get(name) != after["files"].get(name):
            if ignored(before["policy"], name) and ignored(after["policy"], name):
                skipped.add(category(name))
            else:
                changed.add("included-markdown" if name in protected else category(name))
    return sorted(changed), sorted(skipped)


def build_identity(config: str, profile: str) -> str:
    rust = capture(["rustc", "-vV"])
    host = re.search(r"^host: (.+)$", rust, re.M).group(1)
    target = os.environ.get("CARGO_BUILD_TARGET", host)
    # Custom JSON targets can be absolute paths. Keep cache paths below the
    # configured root, and distinguish targets with identical file names.
    target_name = re.sub(r"[^A-Za-z0-9_.-]", "_", Path(target).name)
    if target_name in {"", ".", ".."}:
        target_name = "custom-target"
    target_contents = Path(target).read_bytes() if Path(target).is_file() else b""
    # Include flags that make artifacts incompatible. Revisions are provenance,
    # not cache keys: Cargo fingerprints decide what can be reused.
    flags = {k: os.environ.get(k, "") for k in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER")}
    custom_target = (target.encode() + target_contents) if target.endswith(".json") or Path(target).name != target else b""
    key = hashlib.sha256((rust + json.dumps(flags, sort_keys=True)).encode() + custom_target).hexdigest()[:12]
    return f"{target_name}/{key}/{config}/{profile}"


class Lease:
    """Nonblocking OS lease; killed owners release it without stale-PID recovery."""
    def __init__(self, root: Path, identity: str, source: dict | None = None):
        self.root = root / identity
        self.source = source
        self.lock = None
        self.path = None

    def __enter__(self):
        self.root.mkdir(parents=True, exist_ok=True)
        for slot in range(1024):
            path = self.root / str(slot)
            path.mkdir(exist_ok=True)
            lock = (path / ".lease").open("a+")
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                lock.close()
                continue
            self.lock, self.path = lock, path
            (path / "owner.json").write_text(json.dumps({"pid": os.getpid(), "checkout": str(ROOT), "source": self.source}, indent=2) + "\n")
            return self
        raise RuntimeError("all build directories are leased; finish an existing check")

    def __exit__(self, *exc):
        (self.path / "owner.json").unlink(missing_ok=True)
        self.lock.close()


def cargo_args(args) -> list[str]:
    packages, features = CONFIGS[args.config]
    if args.package:
        packages = [item for p in args.package for item in ("-p", p)]
        if "zakura-wallet-lib" in args.package:
            raise ValueError("the facade has exclusive backends; use verify --only wallet-lib-modes")
    return packages + (["--features", ",".join(features)] if features else [])


def run(argv: list[str], env: dict, *, output=False, pass_fds=()):
    print("+ " + " ".join(argv), flush=True)
    return subprocess.run(argv, cwd=ROOT, env=env, text=True, stdout=subprocess.PIPE if output else None, check=False, pass_fds=pass_fds)


def write_result(path: Path, result: dict):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(result, indent=2) + "\n")
    temporary.replace(path)


def execute(args) -> int:
    root = Path(os.environ.get("WALLET_LIB_BUILD_ROOT", str(Path.home() / ".cache/wallet-libraries/targets"))).expanduser().resolve()
    profile = args.profile or ("test" if args.command in {"test", "verify"} else "dev")
    config = (args.only or "verify") if args.command == "verify" else args.config
    identity = build_identity(config, profile)
    if args.command == "doctor":
        print(capture(["rustc", "-vV"]))
        print(capture(["cargo", "--version"]))
        print(f"configuration: {args.config}; profile: {profile}; build root: {root / identity}")
        ready = subprocess.run(["cargo", "metadata", "--locked", "--offline", "--format-version", "1"], cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        print("dependencies: ready offline" if ready.returncode == 0 else "dependencies: not ready offline; run cargo fetch --locked")
        for owner in sorted(root.glob("*/*/*/*/*/owner.json")):
            print(f"build owner: {owner}: {owner.read_text().strip()}")
        return ready.returncode
    before = source_state(args.command)
    with Lease(root, identity, receipt(before)) as lease:
        print(f"build directory: {lease.path}", flush=True)
        env = dict(os.environ, CARGO_TARGET_DIR=str(lease.path))
        started = time.monotonic()
        code = 1
        try:
            if args.command == "verify":
                groups = [args.only] if args.only else list(VERIFY)
                for group in groups:
                    code = run([str(ROOT / "scripts" / f"verify-{group}.sh")], env, pass_fds=(lease.lock.fileno(),)).returncode
                    if code:
                        break
                else:
                    code = 0
                if code == 0 and not args.only:
                    for name in CONFIGS:
                        nested = [sys.executable, str(Path(__file__).resolve()), "test", "--config", name]
                        code = run(nested, env, pass_fds=(lease.lock.fileno(),)).returncode
                        if code:
                            break
            else:
                command = "clippy" if args.command == "lint" else args.command
                argv = ["cargo", command, "--locked", "--profile", profile] + cargo_args(args)
                if args.command in {"check", "lint"}:
                    argv.append("--all-targets")
                if args.command == "lint":
                    argv.append("--no-deps")
                if args.filter:
                    argv.append(args.filter)
                    harness = ["--", "--list"] + (["--exact"] if args.exact else [])
                    listing = run(argv + harness, env, output=True, pass_fds=(lease.lock.fileno(),))
                    print(listing.stdout, end="")
                    if listing.returncode:
                        code = listing.returncode
                    elif not any(line.endswith(": test") for line in listing.stdout.splitlines()):
                        print(f"no tests selected by {args.filter!r}", file=sys.stderr)
                        code = 2
                    else:
                        if args.exact:
                            argv += ["--", "--exact"]
                        code = run(argv, env, pass_fds=(lease.lock.fileno(),)).returncode
                else:
                    code = run(argv, env, pass_fds=(lease.lock.fileno(),)).returncode
        finally:
            after = source_state(args.command)
            changed, skipped = compare(before, after)
            if changed:
                code = 3
                print(f"source inputs changed during validation ({', '.join(changed)}); result invalidated", file=sys.stderr)
            if before["policy"]["fallback_reason"]:
                print(f"input policy: full ({before['policy']['fallback_reason']})", flush=True)
            result = {"source": receipt(before), "source_after": receipt(after), "changed_inputs": changed, "ignored_changes": skipped, "config": config, "profile": profile, "pid": os.getpid(), "checkout": str(ROOT), "duration_seconds": time.monotonic() - started, "exit_code": code, "status": "pass" if code == 0 else "invalidated" if changed else "fail"}
            write_result(lease.path / "last-result.json", result)
            print(f"result: {lease.path / 'last-result.json'}", flush=True)
        return code


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("doctor", "check", "test", "lint", "verify"))
    parser.add_argument("filter", nargs="?", help="test-name substring")
    parser.add_argument("--config", choices=CONFIGS, default="default")
    parser.add_argument("-p", "--package", action="append")
    parser.add_argument("--profile", help="Cargo profile; final validation uses test")
    parser.add_argument("--exact", action="store_true")
    parser.add_argument("--only", choices=VERIFY, help="one repository verification script")
    args = parser.parse_args(argv)
    if args.profile and not re.fullmatch(r"[A-Za-z0-9_-]+", args.profile):
        parser.error("profile names must contain only letters, digits, underscores, or hyphens")
    if (args.filter or args.exact) and args.command != "test":
        parser.error("test filters require the test command")
    if args.exact and not args.filter:
        parser.error("--exact requires a test-name filter")
    if args.only and args.command != "verify":
        parser.error("--only requires verify")
    if args.package and args.command in {"doctor", "verify"}:
        parser.error("package filters require check, test, or lint")
    try:
        return execute(args)
    except (ValueError, RuntimeError, subprocess.CalledProcessError, OSError) as error:
        print(f"development check failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
