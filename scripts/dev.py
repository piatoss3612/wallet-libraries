#!/usr/bin/env python3
"""Focused wallet checks with reusable, exclusively owned Cargo build directories."""
from __future__ import annotations

import argparse
import bisect
import fcntl
import functools
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
POLICY_VERSION = 13
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
# Reviewed run-time readers whose computed paths cannot reach documentation,
# each bound to its file, enclosing function, and exact source line.
READER_REGISTRY = "scripts/audited-readers.toml"
FUNCTION = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+\"\w+\"\s+)?fn\s+\w+.*$", re.M)
# Run-time file access whose path is not one whole string literal (a literal
# followed by `+ "…"` or `.to_owned()` is computed) may reach any file, so it
# is an unknown reader (the policy cannot prove where it points).
WHOLE_LITERAL = r'(?:[bc]?"(?:[^"\\]|\\.)*"|[bc]?r(#*)".*?"\1)\s*[,)]'
# Whitespace may separate path tokens (`std :: fs :: read`); patterns run on
# the code view, where comments (`fs::/* */read`) are already blanks.
GAP = r"\s*"
PATH_SEP = GAP + "::" + GAP
TURBOFISH = r"(?:" + PATH_SEP + r"<[^()]*>)?"
RUNTIME_READER = re.compile(r'(?:\b(?:fs|fs_err|File|Connection|OpenOptions|Path|PathBuf|Dir)' + PATH_SEP + r'(?:r#)?\w+|\.' + GAP + r'open(?:_with_flags)?|\bread_to_string|(?<!fn )\b(?:f?open(?:at)?(?:64)?))' + TURBOFISH + GAP + r'\(\s*(?!' + WHOLE_LITERAL + r')(?!\))', re.S)
# Path methods that probe the file system: their receiver may be any path.
PATH_PROBE = re.compile(r"\.\s*(?:exists|try_exists|is_file|is_dir|is_symlink|metadata|symlink_metadata|canonicalize|read_link)\s*\(")
# File access hidden behind another name: a file-system function or opener
# used as a value (`let r = fs::read;`, `.map(File::open)`), or imported by
# `use` under an alias, as a glob, or as a bare function (`use std::fs::read;`).
FILE_ROOTS = {"fs", "fs_err", "File", "OpenOptions", "DirBuilder", "Path", "PathBuf", "Connection", "Mmap", "MmapOptions", "Command"}
FILE_VALUE = re.compile(r"\b(?:fs|fs_err)" + PATH_SEP + r"(?:r#)?[a-z_]\w*\b(?!" + GAP + r"(?:\(|::|!))|\b(?:File|OpenOptions|DirBuilder|Connection|Mmap|MmapOptions|Path|PathBuf)" + PATH_SEP + r"(?:open\w*|create\w*|new|from|options|map\w*)\b(?!" + GAP + r"(?:\(|::))", re.S)
INCLUDE_MACROS = {"include", "include_str", "include_bytes"}
USE_TOKEN = re.compile(r"::|[{},*]|r#\w+|[A-Za-z_]\w*")
USE_DELIMITER = re.compile(r"[{};]")
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
# Established build output (Python bytecode included); never a source input.
BUILD_OUTPUT = {"target", ".vscode", ".git", "__pycache__"}
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


@functools.lru_cache(maxsize=None)
def function_headers(text: str) -> list[tuple[int, int, str]]:
    """Start, end, and normalized text of every function header line."""
    return [(m.start(), m.end(), " ".join(m[0].split())) for m in FUNCTION.finditer(text)]


def site(name: str, text: str, position: int) -> tuple[str, str, str]:
    """A source-bound reader identity: file, enclosing function, and the call.

    The call runs from the start of its line through the balanced parentheses
    of its arguments, so a changed argument on a later line is a new site.
    """
    headers = function_headers(text)
    index = bisect.bisect_right(headers, (position,)) - 1
    while index >= 0 and headers[index][1] > position:
        index -= 1
    function = headers[index][2] if index >= 0 else ""
    start = text.rfind("\n", 0, position) + 1
    opening = text.find("(", position)
    end = text.find("\n", position)
    if opening >= 0 and (end < 0 or opening < end):
        depth = 0
        for index in range(opening, min(len(text), opening + 4000)):
            depth += {"(": 1, ")": -1}.get(text[index], 0)
            if depth == 0:
                end = max(end, index + 1)
                break
    return name, function, " ".join(text[start:end if end >= 0 else len(text)].split())


# Rust lexing. The code view blanks comments (doctest content in `///`,
# `//!`, `/** */`, and `/*! */` stays code) and turns raw identifiers `r#fs`
# into `  fs`; the skeleton also blanks string and character contents. Both
# keep every offset, so matches map back to the source unchanged.
RUST_LEX = re.compile(r'''(?P<doc>///(?!/)[^\n]*|//![^\n]*)|(?P<line>//[^\n]*)|(?P<blockdoc>/\*[*!](?![*/]).*?\*/)|(?P<block>/\*.*?\*/)|(?P<raw>(?<!\w)[bc]?r(?P<hashes>\#*)".*?"(?P=hashes))|(?P<string>(?<!\w)[bc]?"(?:[^"\\]|\\.)*")|(?P<char>'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]{1,8}\}|.)|[^'\\\n])')|(?P<rawid>(?<![\w#])r\#(?=[A-Za-z_]))''', re.S)


def blank(text: str) -> str:
    return re.sub(r"[^\n]", " ", text)


@functools.lru_cache(maxsize=None)
def rust_views(text: str) -> tuple[str, str]:
    """The code view and the skeleton of Rust source (same length as `text`)."""
    code, skeleton, cursor = [], [], 0
    for match in RUST_LEX.finditer(text):
        code.append(text[cursor:match.start()])
        skeleton.append(text[cursor:match.start()])
        token = match[0]
        if match["doc"] is not None or match["blockdoc"] is not None:
            tail = 2 if match["blockdoc"] is not None else 0
            inner_code, inner_skeleton = rust_views(token[3:len(token) - tail])
            code.append("   " + inner_code + " " * tail)
            skeleton.append("   " + inner_skeleton + " " * tail)
        elif match["line"] is not None or match["block"] is not None:
            code.append(blank(token))
            skeleton.append(blank(token))
        elif match["rawid"] is not None:
            code.append("  ")
            skeleton.append("  ")
        else:
            code.append(token)
            skeleton.append(token[0] + blank(token[1:-1]) + token[-1] if len(token) > 1 else token)
        cursor = match.end()
    code.append(text[cursor:])
    skeleton.append(text[cursor:])
    return "".join(code), "".join(skeleton)


@functools.lru_cache(maxsize=None)
def use_statements(text: str) -> list[tuple[int, int]]:
    """Spans of `use …;` declarations (in the skeleton), ending at the first `;` outside braces."""
    skeleton = rust_views(text)[1]
    spans = []
    for match in re.finditer(r"(?<![\w:])use(?=[\s{:])", skeleton):
        depth, end = 0, len(skeleton)
        for delimiter in USE_DELIMITER.finditer(skeleton, match.end()):
            depth += {"{": 1, "}": -1}.get(delimiter[0], 0)
            if delimiter[0] == ";" and depth == 0 or depth < 0:
                end = delimiter.start()
                break
        spans.append((match.start(), end))
    return spans


def use_paths(tree: str) -> list[tuple[list[str], str | None]]:
    """Expand a use tree into imported paths and their aliases.

    `std::{fs::{self, read as r}, io}` gives `std::fs::self`, `std::fs::read`
    (alias `r`), and `std::io`. A glob ends its path with `*`.
    """
    tokens = USE_TOKEN.findall(tree)
    def parse(position: int, prefix: list[str]):
        segments = list(prefix)
        while position < len(tokens):
            token = tokens[position]
            if token == "{":
                position, found = position + 1, []
                while tokens[position] != "}":
                    items, position = parse(position, segments)
                    found += items
                    if tokens[position] == ",":
                        position += 1
                return found, position + 1
            if token == "*":
                return [(segments + ["*"], None)], position + 1
            if token == "as":
                return [(segments, tokens[position + 1])], position + 2
            if token in {",", "}"}:
                break
            if token != "::":
                segments.append(token)
            position += 1
        return [(segments, None)], position
    return parse(0, [])[0]


def code_span(text: str, match: re.Match) -> bool:
    """Whether a path is a Markdown code span such as [`Client::create`] in prose."""
    start = match.start()
    while start > 0 and (text[start - 1].isalnum() or text[start - 1] in "_:"):
        start -= 1
    return text[start - 1:start] == "`" and text[match.end():match.end() + 1] == "`"


def use_tree(declaration: str) -> str | None:
    """The tree of a syntactically valid `use` declaration, else None.

    Prose such as "use to the seeded address." is not Rust and is skipped;
    code that does not parse as a use tree cannot compile either.
    """
    tree = declaration.split(None, 1)[1] if len(declaration.split(None, 1)) > 1 else ""
    if not re.fullmatch(r"[\w\s:{},*#]*", tree):
        return None
    tokens = USE_TOKEN.findall(tree)
    words = [token for token in tokens if token not in {"::", "{", "}", ",", "*"}]
    if any(re.fullmatch(r"(?:r#)?\w+", a) and re.fullmatch(r"(?:r#)?\w+", b) and "as" not in {a, b} for a, b in zip(tokens, tokens[1:])):
        return None
    return tree if words or "*" in tokens else None


def hidden_access(text: str, heads: set[str]) -> list[str]:
    """Imports that rename, glob, or bare-import file access or `include` macros."""
    reasons = []
    skeleton = rust_views(text)[1]
    for start, end in use_statements(text):
        tree = use_tree(skeleton[start:end])
        if tree is None:
            continue
        try:
            paths = use_paths(tree)
        except IndexError:
            reasons.append("unparsed use declaration")
            continue
        for segments, alias in paths:
            if segments and segments[-1] == "self":
                segments = segments[:-1]
            if not segments:
                continue
            last = segments[-1]
            if last in INCLUDE_MACROS and alias:
                reasons.append(f"aliased {last} macro")
            roots = [i for i, segment in enumerate(segments) if segment in FILE_ROOTS | heads]
            if not roots:
                continue
            if alias and alias != "_" or last == "*":
                reasons.append("aliased file access import")
            elif roots[-1] < len(segments) - 1 and last[:1].islower():
                reasons.append("imported file access function")
    return reasons


@functools.lru_cache(maxsize=None)
def function_spans(text: str) -> list[tuple[int, int]]:
    """Each Rust function from its header to its closing brace (or `;`)."""
    skeleton = rust_views(text)[1]
    delimiters = [(m.start(), m[0]) for m in re.finditer(r"[{};]", skeleton)]
    closing, stack = {}, []
    for position, character in delimiters:
        if character == "{":
            stack.append(position)
        elif character == "}" and stack:
            closing[stack.pop()] = position
    positions = [position for position, _ in delimiters]
    spans = []
    for header in FUNCTION.finditer(skeleton):
        index = bisect.bisect_left(positions, header.start())
        while index < len(delimiters) and delimiters[index][1] == "}":
            index += 1
        if index == len(delimiters):
            spans.append((header.start(), len(text)))
        elif delimiters[index][1] == ";":
            spans.append((header.start(), delimiters[index][0] + 1))
        else:
            spans.append((header.start(), closing.get(delimiters[index][0], len(text) - 1) + 1))
    return spans


@functools.lru_cache(maxsize=None)
def macro_spans(text: str) -> list[tuple[str, int, int]]:
    """Each `macro_rules!` definition: name and span."""
    skeleton = rust_views(text)[1]
    found = []
    for match in re.finditer(r"\bmacro_rules!\s*(\w+)\s*([{(\[])", skeleton):
        depth, opening = 0, match[2]
        pair = {"{": "}", "(": ")", "[": "]"}[opening]
        for index in range(match.end() - 1, len(skeleton)):
            depth += 1 if skeleton[index] == opening else -1 if skeleton[index] == pair else 0
            if depth == 0:
                break
        found.append((match[1], match.start(), index + 1))
    return found


def canonical(text: str) -> str:
    """Rust source with only meaningless differences removed.

    String, character, and raw literals and doc comments (doctests) keep their
    exact bytes; other comments are dropped and whitespace runs elsewhere
    become one space, so formatting does not stale a reviewed reader but any
    change inside a literal does.
    """
    pieces, cursor = [], 0
    def code(part: str):
        part = re.sub(r"\s+", " ", part)
        if pieces and pieces[-1].endswith(" ") and part.startswith(" "):
            part = part[1:]
        if part:
            pieces.append(part)
    for match in RUST_LEX.finditer(text):
        code(text[cursor:match.start()])
        if match["line"] is not None or match["block"] is not None:
            code(" ")
        elif match["rawid"] is not None:
            code("r#")
        else:
            pieces.append(match[0])
        cursor = match.end()
    code(text[cursor:])
    return "".join(pieces).strip()


def outside_functions(text: str) -> str:
    """The exact bytes of a file outside its outermost function bodies."""
    spans = function_spans(text)
    outside, cursor = [], 0
    for span_start, span_end in sorted(spans):
        if span_start >= cursor:
            outside.append(text[cursor:span_start])
            cursor = span_end
    outside.append(text[cursor:])
    return "".join(outside)


def definition_index(sources: dict[str, str]) -> dict[str, list[tuple[str, int, int]]]:
    """Every Rust function and `macro_rules!` in the given sources, by name."""
    index: dict[str, list[tuple[str, int, int]]] = {}
    for name, text in sources.items():
        skeleton = rust_views(text)[1]
        for start, end in function_spans(text):
            named = re.search(r"\bfn\s+(\w+)", skeleton[start:end])
            if named:
                index.setdefault(named[1], []).append((name, start, end))
        for macro, start, end in macro_spans(text):
            index.setdefault(macro + "!", []).append((name, start, end))
    return index


CALLED = re.compile(r"(?<!fn )\b([A-Za-z_]\w*)\s*(?:::\s*<[^()]*>\s*)?\(|\b(\w+!)")


def source_context(text: str, position: int, name: str = "", sources: dict[str, str] | None = None, index: dict | None = None, cache: dict | None = None) -> tuple[str, str]:
    """A reviewed reader's enclosing function and a digest of its provenance.

    The digest covers exact bytes (no normalization, so string literals are
    bound byte for byte): the enclosing function, its file outside function
    bodies (imports, constants, statics, macros), and every function or macro
    it calls that is defined in the scanned sources, transitively and in any
    file, with those files' own outside-function bytes. Calls resolve by name,
    so every same-named definition is included. Without an enclosing function
    the whole file is bound.
    """
    sources = dict(sources or {})
    sources[name] = text
    index = index if index is not None else definition_index({name: text})
    cache = cache if cache is not None else {}
    enclosing = [span for span in function_spans(text) if span[0] <= position < span[1]]
    if not enclosing:
        return "", hashlib.sha256(canonical(text).encode()).hexdigest()
    start, end = max(enclosing)
    calls = cache.setdefault("calls", {})
    def callees(definition):
        if definition not in calls:
            file, a, b = definition
            skeleton = rust_views(sources[file])[1]
            calls[definition] = {callee for call in CALLED.finditer(skeleton, a, b) for callee in index.get(call[1] or call[2], ()) if callee[0] in sources}
        return calls[definition]
    included, frontier = {(name, start, end)}, [(name, start, end)]
    while frontier:
        for callee in callees(frontier.pop()) - included:
            included.add(callee)
            frontier.append(callee)
    hashes = cache.setdefault("hashes", {})
    def hashed(key, data):
        if key not in hashes:
            hashes[key] = hashlib.sha256(data().encode()).hexdigest()
        return hashes[key]
    parts = sorted({f"{file}\0outside\0" + hashed((file, "outside"), lambda file=file: canonical(outside_functions(sources[file]))) for file, _, _ in included})
    parts += sorted(f"{file}\0" + hashed((file, a, b), lambda file=file, a=a, b=b: canonical(sources[file][a:b])) for file, a, b in included)
    header = text[start:text.find("\n", start) if "\n" in text[start:] else len(text)]
    return header, hashlib.sha256("\n".join(parts).encode()).hexdigest()


RUST_KEYWORDS = set("as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return static struct super trait true type unsafe use where while".split())
IDENTIFIERS = re.compile(r"(?<![\w.:])[A-Za-z_]\w*")


def caller_input(text: str, position: int) -> str | None:
    """The enclosing function's name when a parameter may flow into the reader.

    Every identifier in every parameter pattern (`P: String`, `(p,): (String,)`,
    `self`) is tainted; taint flows to bindings, assignments, receivers of
    mutating calls, and match arms of a tainted scrutinee. The reader is
    tainted when its statement mentions a tainted identifier. A parameter list
    that cannot be parsed counts as a caller input.
    """
    spans = [span for span in function_spans(text) if span[0] <= position < span[1]]
    if not spans:
        return None
    start, end = max(spans)
    body = rust_views(text)[1][start:end]
    match = re.search(r"\bfn\s+(\w+)\s*(?:<[^{}]*?>)?\s*\(", body)
    if not match:
        return "<unparsed function>"
    depth, close = 0, None
    for index in range(match.end() - 1, len(body)):
        depth += {"(": 1, ")": -1}.get(body[index], 0)
        if depth == 0:
            close = index
            break
    if close is None:
        return match[1]
    parameters, depth, current = [], 0, ""
    for character in body[match.end():close]:
        depth += 1 if character in "(<[" else -1 if character in ")>]" else 0
        if character == "," and depth == 0:
            parameters.append(current)
            current = ""
        else:
            current += character
    parameters.append(current)
    tainted = set()
    for parameter in filter(str.strip, parameters):
        depth, pattern = 0, None
        for index, character in enumerate(parameter):
            depth += 1 if character in "(<[" else -1 if character in ")>]" else 0
            if character == ":" and depth == 0 and parameter[index:index + 2] != "::" and parameter[index - 1:index + 1] != "::":
                pattern = parameter[:index]
                break
        if pattern is None:
            if re.fullmatch(r"\s*(?:#\[[^\]]*\]\s*)*&?\s*(?:'\w+\s+)?(?:mut\s+)?self\s*", parameter):
                tainted.add("self")
                continue
            return match[1]
        tainted |= set(IDENTIFIERS.findall(re.sub(r"#\[[^\]]*\]", " ", pattern))) - {"mut", "ref"}
    if not tainted:
        return None
    rest = body[close + 1:]
    statements = re.split(r"[;{}]", rest)
    names = lambda part: set(IDENTIFIERS.findall(part)) - RUST_KEYWORDS
    changed = True
    while changed:
        changed = False
        arms = any(re.match(r"\s*match\b", statement) and names(statement) & tainted for statement in statements)
        for statement in statements:
            # `let P = E`, `if let P = E`, `P = E`, `for P in E`: P depends on E.
            binding = re.match(r"\s*(?:(?:if|while)\s+)?(?:let\s+)?(.+?)\s*(?<![=!<>+\-*/%&|^])=(?![=>])(.*)$", statement, re.S) or re.match(r"\s*for\s+(.+?)\s+in\b(.*)$", statement, re.S)
            # `recv.method(E)`: E may be stored into the receiver.
            mutation = re.match(r"\s*([A-Za-z_]\w*)\s*\.\s*\w+\s*\((.*)$", statement, re.S)
            if arms and "=>" in statement:
                target, source = statement.split("=>")[0], statement
            elif binding:
                target, source = binding[1], binding[2]
            elif mutation:
                target, source = mutation[1], mutation[2]
            else:
                continue
            if names(source) & tainted and not names(target) <= tainted:
                tainted |= names(target)
                changed = True
    offset = position - start - close - 1
    cursor, reader = 0, ""
    for statement in statements:
        if cursor <= offset <= cursor + len(statement):
            reader = statement
            break
        cursor += len(statement) + 1
    return match[1] if names(reader) & tainted else None


def computed(text: str, found: set[str], name: str = "", runtime: bool = True, registry: dict | None = None, rust: bool | None = None) -> list[str]:
    """Constructs in code that may build a path this scanner cannot resolve.

    Run-time constructs count only in code the operation executes; a reviewed
    registry site (recorded in `registry["used"]`) is accepted. Aliased or
    value uses of file access are never accepted.
    """
    rust = name.endswith((".rs", ".md")) if rust is None else rust
    registry = registry if registry is not None else {"sites": {}, "used": set()}
    reasons = []
    # Patterns run on the code view: comments cannot split a path or a `use`,
    # and `r#fs` reads as `fs`. Offsets still map to the source text.
    code = rust_views(text)[0] if rust else text
    if COMPUTED_INCLUDE.search(code):
        reasons.append("computed include")
    # A macro template that substitutes a path segment or callee
    # (`std::fs::$reader($path)`, `$m::read(p)`) can name any reader.
    template = re.compile(r"\b(?:" + "|".join(sorted(FILE_ROOTS | INCLUDE_MACROS | registry.get("heads", set()))) + r")\s*::\s*\$\w+|\$(?!crate\b)\w+\s*::|(?:::\s*)?\$\w+\s*!?\s*\(")
    if any(template.search(code, a, b) for _, a, b in (macro_spans(text) if rust else ())):
        reasons.append("macro template with a substituted path or callee")
    def unreviewed(pattern: re.Pattern, accept=lambda match: False) -> bool:
        for match in pattern.finditer(code):
            identity = site(name, text, match.start())
            if accept(match):
                continue
            entry = registry["sites"].get(identity)
            if entry is not None:
                header, digest = source_context(text, match.start(), name, registry.get("sources"), registry.get("index"), registry.setdefault("cache", {}))
                registry.setdefault("digests", {})[identity] = digest
                registry["used"].add(identity)
                if entry.get("context") != digest:
                    # Any change in the function or its file-level context
                    # (an earlier path initializer, a constant, an import).
                    registry.setdefault("stale", set()).add(identity)
                    return True
                function = caller_input(text, match.start())
                if function and not entry.get("caller_input") and not any(call == function or call.endswith("::" + function) for call in registry.get("calls", ())):
                    # A parameter may carry any caller's path: only a wrapper,
                    # whose every call site is reviewed, may accept it.
                    registry.setdefault("caller_inputs", set()).add(f"{name}: fn {function}")
                    return True
                continue
            return True
        return False
    if runtime:
        if unreviewed(TRAVERSAL) or BASE_DIRECTORY.search(code) and UPWARD.search(code):
            reasons.append("computed directory traversal")
        if unreviewed(WORKTREE_READERS):
            reasons.append("work-tree reader")
        # The package's own binaries (`CARGO_BIN_EXE_*`) are built from scanned
        # sources. Other programs, including unrestricted sqlite3 whose SQL can
        # read files, are exempt only as the exact audited invocation.
        if unreviewed(SPAWN, lambda spawn: bool(spawn[2]) or audited_sqlite(name, text, spawn)):
            reasons.append("process launch that may read any file")
        if unreviewed(RUNTIME_READER) or unreviewed(PATH_PROBE) or registry.get("wrappers") and unreviewed(registry["wrappers"]):
            reasons.append("run-time file access with a computed path")
        uses = use_statements(text) if rust else []
        values = [FILE_VALUE] + ([registry["wrapper_values"]] if registry.get("wrapper_values") else [])
        if any(not any(start <= match.start() < end for start, end in uses) and not code_span(code, match) for pattern in values for match in pattern.finditer(code)):
            reasons.append("file access used as a value")
        if re.search(r"\bParentDir\b", code):
            reasons.append("parent directory navigation")
    # A renamed `include!` reads its (possibly computed) argument at compile
    # time; hidden run-time access counts only in executed code.
    if rust:
        reasons += [reason for reason in hidden_access(text, registry.get("heads", set())) if runtime or "macro" in reason]
    # Pure upward navigation such as `Path::new("..").join("..")` can reach the root
    # from any package depth.
    if any(re.fullmatch(r"[./]*\.\.[./]*", literal) for literal in found):
        reasons.append("parent directory navigation")
    return reasons


def module_files(name: str, text: str, index: Index) -> set[str]:
    """Files `mod name;` declarations may load, from either module directory.

    A crate root or `mod.rs` keeps modules beside it; `foo.rs` keeps them in
    `foo/`. Both are taken, so a misjudged crate root only adds files.
    """
    folder = Path(name).parent
    found = set()
    for module in re.findall(r"(?<![\w:])mod\s+(\w+)\s*;", rust_views(text)[1]):
        for base in (folder, folder / Path(name).stem):
            for candidate in (base / f"{module}.rs", base / module / "mod.rs"):
                found |= index.paths.get(candidate.as_posix().removeprefix("./").casefold(), set())
    return found


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


def dependency_dirs(manifest: str, document: dict, workspace: dict, development: bool, kinds: tuple[str, ...] = ()) -> set[str]:
    """Workspace path dependencies Cargo builds for a package (dev only when selected)."""
    kinds = kinds or ("dependencies", "build-dependencies") + (("dev-dependencies",) if development else ())
    tables = [document.get(kind, {}) for kind in kinds]
    tables += [target.get(kind, {}) for target in document.get("target", {}).values() if isinstance(target, dict) for kind in kinds]
    found = set()
    for table in tables:
        for key, value in table.items() if isinstance(table, dict) else ():
            if isinstance(value, dict) and value.get("workspace"):
                value = workspace.get(key, {})
                base = ""
            else:
                base = str(Path(manifest).parent)
            if isinstance(value, dict) and isinstance(value.get("path"), str):
                target = os.path.normpath(os.path.join(base, value["path"]))
                found.add("" if target == "." else target)
    return found


def input_policy(command: str, files: dict[str, str], selection: list[str] | None = None) -> dict:
    """Select excluded prose for Cargo commands after auditing what Cargo can reach.

    Every file in a package (including Git-ignored ones), every manifest, and
    Cargo configuration is scanned; any file a scanned literal may name is
    scanned in turn, so Markdown doctests, build-script generators, and
    included modules are audited transitively.
    """
    policy = {"name": "rust-check" if command in RUST_CHECK_COMMANDS else "full", "version": POLICY_VERSION, "fallback_reason": None, "protected": [], "excluded": [], "consumed_ignored": [], "packages": [], "reviewed_readers": 0}
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
    packages, documents = [], {}
    for manifest in manifests:
        try:
            documents[manifest] = tomllib.loads((ROOT / manifest).read_text())
            if {"package", "project"} & set(documents[manifest]):
                packages.append("" if manifest == "Cargo.toml" else str(Path(manifest).parent))
        except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError):
            pass  # reported when the manifest is scanned below
    packages.sort(key=len, reverse=True)
    # Only packages the selected command builds can read anything: the
    # selection with its dev-dependencies, then normal and build dependencies.
    manifest_of = {package: "Cargo.toml" if package == "" else f"{package}/Cargo.toml" for package in packages}
    names = {(documents[manifest_of[p]].get("package") or documents[manifest_of[p]].get("project") or {}).get("name"): p for p in packages}
    workspace = documents.get("Cargo.toml", {}).get("workspace", {}).get("dependencies", {})
    if selection is None or any(name not in names for name in selection):
        reachable = set(packages)
    else:
        # A selected package also reached first as another's dependency must
        # still contribute its dev-dependencies, whatever the visit order.
        visited: dict[str, bool] = {}
        frontier = [(names[name], True) for name in selection]
        while frontier:
            package, development = frontier.pop()
            if package not in manifest_of or package in visited and (visited[package] or not development):
                continue
            visited[package] = development
            frontier += [(dependency, False) for dependency in dependency_dirs(manifest_of[package], documents[manifest_of[package]], workspace, development)]
        reachable = set(visited)
    policy["packages"] = sorted(p or "." for p in reachable)
    # Proc macros and build dependencies (with what they depend on) run inside
    # the compiler or a build script, so `check` executes them too.
    compile_time: set[str] = set()
    frontier = [p for p in reachable if ((documents[manifest_of[p]].get("lib") or {}).get("proc-macro") or (documents[manifest_of[p]].get("lib") or {}).get("proc_macro"))]
    frontier += [d for p in reachable for d in dependency_dirs(manifest_of[p], documents[manifest_of[p]], workspace, False, ("build-dependencies",))]
    while frontier:
        package = frontier.pop()
        if package in compile_time or package not in manifest_of:
            continue
        compile_time.add(package)
        frontier += dependency_dirs(manifest_of[package], documents[manifest_of[package]], workspace, False)
    # Targets a manifest places by `path` (possibly outside the package) run
    # like the targets they declare: build scripts always, tests and benches
    # for `test`, and examples only when an `[[example]]` sets `test = true`.
    executed: set[str] = set()
    tested_examples: set[str] = set()
    for package in reachable:
        document = documents[manifest_of[package]]
        base = Path(manifest_of[package]).parent
        targets = [("build", (document.get("package") or {}).get("build"))]
        targets += [(kind, table.get("path")) for kind in ("lib", "bin", "test", "bench", "example") for table in ([document.get(kind)] if kind == "lib" else document.get(kind, [])) if isinstance(table, dict)]
        if any(isinstance(table, dict) and table.get("test") for table in document.get("example", [])):
            tested_examples.add(package)
        for kind, target in targets:
            if isinstance(target, str) and (kind == "build" or command not in {"check", "lint"} and (kind != "example" or package in tested_examples)):
                executed.add(os.path.normpath(base / target).replace(os.sep, "/").removeprefix("./"))
    def runtime_file(name: str) -> bool:
        """Code the operation executes: build scripts always, tests' code for
        `test`, and anything executed code reaches."""
        if name in executed:
            return True
        package = next((p for p in packages if p == "" or name.startswith(p + "/")), None)
        if package is None:
            return command not in {"check", "lint"}  # reached outside any package: assume executed
        if package not in reachable:
            return False  # not built for this selection unless executed code includes it
        relative = name[len(package) + 1:] if package else name
        example = relative.startswith("examples/") and package not in tested_examples
        if package in compile_time:
            return not example
        build = (documents[manifest_of[package]].get("package") or {}).get("build", "build.rs")
        if relative == build or relative.startswith("build/"):
            return True
        return command not in {"check", "lint"} and not example
    try:
        registry = tomllib.loads((ROOT / READER_REGISTRY).read_text()) if (ROOT / READER_REGISTRY).exists() else {}
        sites = {(entry["file"], entry["function"], entry["line"]): entry for entry in registry.get("reader", [])}
        # Reviewed readers that open a caller's path: every call site is a reader too.
        calls = [entry["call"] for entry in registry.get("wrapper", [])]
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError, KeyError, TypeError) as error:
        reasons.append(f"unreadable reader registry: {type(error).__name__}")
        sites, calls = {}, []
    # Qualified calls (`crate::tor::create_with_timeouts(…)`) are call sites too;
    # renaming a wrapper or passing it as a value hides its calls.
    wrappers = "|".join(PATH_SEP.join(map(re.escape, call.split("::"))) for call in calls)
    # Reviewed readers bind their path provenance across files: every Rust
    # source in the checkout (tracked, untracked, or ignored) is indexed.
    sources = {}
    for name in sorted(known) if sites else ():
        if name.endswith(".rs") and not BUILD_OUTPUT & set(name.split("/")):
            try:
                sources[name] = (ROOT / name).read_text()
            except (OSError, UnicodeDecodeError):
                pass
    reviewed = {
        "sources": sources,
        "index": definition_index(sources),
        "sites": sites,
        "used": set(),
        "wrappers": re.compile(r"(?<!fn )(?<!\w)(?:" + wrappers + r")" + TURBOFISH + GAP + r"\(", re.S) if calls else None,
        "wrapper_values": re.compile(r"(?<!fn )(?<!\w)(?:" + wrappers + r")\b(?!" + GAP + r"(?:\(|::|!))", re.S) if calls else None,
        "heads": {call.split("::")[0] for call in calls},
        "calls": calls,
    }
    def package_of(name: str) -> str | None:
        return next((p for p in packages if p == "" or name.startswith(p + "/")), None)
    cargo = manifests | {name for name in known if Path(name).name in {"rust-toolchain", "rust-toolchain.toml"} or name.startswith(".cargo/")}
    # Documentation is never a seed: a page is scanned only when something reaches it.
    pending = sorted((cargo | {name for name in known if package_of(name) in reachable}) - candidates)
    scanned = set(pending)
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
                    reasons.extend(f"{reason} in {name}" for reason in computed(text, found, name, runtime_file(name), reviewed, rust))
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
                pending.append(item)
            if runtime_file(name):
                # A build script's `mod helper;` runs with it: executed code
                # makes its declared modules executed too; rescan if needed.
                declared = module_files(name, text, index)
                pending.extend(sorted((declared - executed) & scanned))
                executed |= declared
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
            if literal in targets:
                # Rescan a file already read as data once it is known to be Rust.
                pending.extend(sorted((reached - rust_like) & scanned))
                rust_like |= reached
            if document is None and runtime_file(name):
                # Executed code makes what it reaches executed too; rescan if needed.
                pending.extend(sorted((reached - executed) & scanned))
                executed |= reached
            for item in reached - scanned:
                scanned.add(item)
                pending.append(item)
    if heads and tails:
        reasons.append(f"documentation path fragments in {min(heads)} and {min(tails)}")
    # A registry entry that no longer matches its source must be reviewed again.
    reasons += [f"stale audited reader {identity[0]}: {identity[2]!r}" for identity in sorted(reviewed.get("stale", ()))]
    reasons += [f"reviewed reader takes a caller input without a wrapper in {item}" for item in sorted(reviewed.get("caller_inputs", ()))]
    for identity in sorted(set(sites) - reviewed["used"]):
        if identity[0] in scanned and runtime_file(identity[0]) or identity[0] not in known:
            reasons.append(f"stale audited reader {identity[0]}: {identity[2]!r}")
    policy["reviewed_readers"] = len(reviewed["used"])
    # Ignored files Cargo may consume are inputs under every policy: everything
    # scanned, which covers every file in a built package (implicit `mod`
    # modules included) and every file a scanned file names.
    policy["consumed_ignored"] = sorted(scanned & ignored_names)
    # Full inputs mean every repository input: an unknown reader may load any
    # ignored file, including one outside every package that nothing names.
    if policy["name"] == "full":
        policy["fallback_reason"] = f"{command} validates every repository input"
        policy["consumed_ignored"] = sorted(ignored_names)
        return policy
    if reasons:
        policy["fallback_reason"] = "; ".join(sorted(set(reasons))[:5])
        policy["consumed_ignored"] = sorted(ignored_names)
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
    if name == READER_REGISTRY:
        return "reader-registry"
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


def source_state(command: str = "verify", selection: list[str] | None = None) -> dict:
    """Attribute results to HEAD and the source inputs selected by the command's policy."""
    files = repository_files()
    policy = input_policy(command, files, selection)
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


def selected_packages(args) -> list[str] | None:
    """Packages a Cargo command selects with `-p`; None means the whole workspace."""
    if args.command not in RUST_CHECK_COMMANDS or not args.config:
        return None
    argv = cargo_args(args)
    if "--workspace" in argv:
        return None
    return [argv[i + 1] for i, item in enumerate(argv) if item == "-p"]


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
    selection = selected_packages(args)
    before = source_state(args.command, selection)
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
            after = source_state(args.command, selection)
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
