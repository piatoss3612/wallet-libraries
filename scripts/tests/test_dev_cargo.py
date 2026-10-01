"""Real Cargo negative oracles for the rust-check input policy.

Each fixture is a std-only workspace whose tests, doctests, or build scripts
consume documentation. For every page the policy excludes, the oracle edits the
page and runs a fresh `cargo test` (new target directory): the outcome must not
change. An excluded page that changes the result is a false pass.

Opt in with WALLET_LIB_CARGO_ORACLES=1; each fixture compiles from scratch.
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("dev", Path(__file__).parents[1] / "dev.py")
dev = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dev)

ENABLED = os.environ.get("WALLET_LIB_CARGO_ORACLES") == "1" and shutil.which("cargo")
WORKSPACE = '[workspace]\nmembers = ["pkg"]\nresolver = "2"\n'
PACKAGE = '[package]\nname = "pkg"\nversion = "0.1.0"\nedition = "2021"\n'
ROOT_PACKAGE = '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n'
PAGES = {
    "docs/guide.md": "ORIGINAL guide\n",
    "docs/user guide.md": "ORIGINAL user guide\n",
    "docs/unrelated.md": "ORIGINAL unrelated\n",
    "CHANGELOG.md": "ORIGINAL changelog\n",
    "README.md": "# Root\n",
}
CHECK = 'assert!(PAGE.contains("ORIGINAL"));'
TEST = "#[test]\nfn oracle() {{ {body} }}\n"
OUT = 'include_str!(concat!(env!("OUT_DIR"), "/page.md"))'
# The root changelog by a path no literal names whole.
CHANGELOG = 'concat!("../CHANGE", "LOG.md")'
BUILD_COPY = 'fn main() {{ let text = {read}; std::fs::write(std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("page.md"), text).unwrap(); }}\n'

# A reviewed reader of the test's own executable, bound to its source context
# (earlier policy versions without context binding ignore the digest).
CONTEXT = getattr(dev, "source_context", lambda text, position: ("", ""))
REVIEWED = '#[test]\nfn oracle() {\n    let path = std::env::current_exe().unwrap();\n    assert!(!String::from_utf8_lossy(&std::fs::read(&path).unwrap()).contains(&["MUT", "ATED"].concat()));\n}\n'
REVIEWED_LINE = 'assert!(!String::from_utf8_lossy(&std::fs::read(&path).unwrap()).contains(&["MUT", "ATED"].concat()));'



def reviewed(files: dict[str, str], *readers: tuple[str, str]) -> str:
    """Registry entries for `readers` (file, needle) bound to the given sources,
    with whatever context binding the policy under test implements."""
    sources = {name: text for name, text in files.items() if name.endswith(".rs")}
    text = ""
    for name, needle in readers:
        position = files[name].index(needle)
        _, function, line = dev.site(name, files[name], position)
        if hasattr(dev, "definition_index"):
            digest = dev.source_context(files[name], position, name, sources, dev.definition_index(sources))[1]
        else:
            digest = CONTEXT(files[name], position)[1]
        text += f'[[reader]]\nfile = "{name}"\nfunction = {json.dumps(function)}\nline = {json.dumps(line)}\nreason = "reviewed"\ncontext = "{digest}"\n\n'
    return text


REVIEWED_REGISTRY = reviewed({"pkg/src/lib.rs": REVIEWED}, ("pkg/src/lib.rs", "std::fs::read"))

# Reviewed readers whose sources change after review in ways the call line does not show.
LITERAL = f'const KIND: &str = "temporary  file";\n#[test]\nfn oracle() {{\n    let path: std::path::PathBuf = if KIND == "temporary  file" {{ std::env::current_exe().unwrap() }} else {{ {CHANGELOG}.into() }};\n    {REVIEWED_LINE}\n}}\n'
HELPER = "pub mod helper;\n#[test]\nfn oracle() {\n    let path = crate::helper::page_path();\n    " + REVIEWED_LINE + "\n}\n"
HELPER_SAFE = "pub fn page_path() -> std::path::PathBuf { std::env::current_exe().unwrap() }\n"
UPPER = f'#[allow(non_snake_case)]\npub fn load(P: String) -> String {{\n    std::fs::read_to_string(&P).unwrap()\n}}\n#[test]\nfn oracle() {{ assert!(load({CHANGELOG}.to_string()).contains("ORIGINAL")); }}\n'
DESTRUCTURED = f'pub fn load((p,): (String,)) -> String {{\n    std::fs::read_to_string(&p).unwrap()\n}}\n#[test]\nfn oracle() {{ assert!(load(({CHANGELOG}.to_string(),)).contains("ORIGINAL")); }}\n'

CASES = {
    # Literal compile-time references, including spaces, raw strings and escapes.
    "spaced include": {"pkg/src/lib.rs": f'pub const PAGE: &str = include_str!("../../docs/user guide.md");\n{TEST.format(body=CHECK)}'},
    "raw spaced include": {"pkg/src/lib.rs": f'pub const PAGE: &str = include_str!(r"../../docs/user guide.md");\n{TEST.format(body=CHECK)}'},
    "hex escaped include": {"pkg/src/lib.rs": f'pub const PAGE: &str = include_str!("../../\\x64ocs/user\\x20guide.md");\n{TEST.format(body=CHECK)}'},
    # A root Markdown doctest that reads another root documentation page.
    "root Markdown doctest": {
        "pkg/src/lib.rs": '#![doc = include_str!("../../README.md")]\n',
        "README.md": '# Root\n\n```\nlet page = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/guide.md")).unwrap();\nassert!(page.contains("ORIGINAL"));\n```\n',
    },
    # Build scripts computing paths, reading metadata, or running a generator.
    "computed build path": {
        "pkg/build.rs": BUILD_COPY.format(read='std::fs::read_to_string(["../do", "cs/gu", "ide.md"].concat()).unwrap()'),
        "pkg/src/lib.rs": f"pub const PAGE: &str = {OUT};\n{TEST.format(body=CHECK)}",
    },
    "consumed metadata": {
        "pkg/Cargo.toml": PACKAGE + '\n[package.metadata.generator]\ninput = ["../do", "cs/gu", "ide.md"]\n',
        "pkg/build.rs": BUILD_COPY.format(read='{ let manifest = std::fs::read_to_string("Cargo.toml").unwrap(); let line = manifest.lines().find(|l| l.starts_with("input")).unwrap(); std::fs::read_to_string(line.split(\'"\').skip(1).step_by(2).collect::<String>()).unwrap() }'),
        "pkg/src/lib.rs": f"pub const PAGE: &str = {OUT};\n{TEST.format(body=CHECK)}",
    },
    "Python generator": {
        "gen.py": "import os, sys\nopen(os.path.join(sys.argv[1], 'page.md'), 'w').write(open('../do' + 'cs/gu' + 'ide.md').read())\n",
        "pkg/build.rs": 'fn main() { assert!(std::process::Command::new("python3").arg("../gen.py").arg(std::env::var("OUT_DIR").unwrap()).status().unwrap().success()); }\n',
        "pkg/src/lib.rs": f"pub const PAGE: &str = {OUT};\n{TEST.format(body=CHECK)}",
    },
    # Rust compiled from non-.rs or Git-ignored files.
    "included snippet": {
        "pkg/src/snippets.inc": 'pub fn page() -> String { std::fs::read_to_string(["../do", "cs/gu", "ide.md"].concat()).unwrap() }\n',
        "pkg/src/lib.rs": 'include!("snippets.inc");\n' + TEST.format(body='assert!(page().contains("ORIGINAL"));'),
    },
    "included JSON expression": {
        "pkg/src/parts.json": '["../do", "cs/", "gu", "ide.md"]\n',
        "pkg/src/lib.rs": 'pub const PARTS: [&str; 4] = include!("parts.json");\n' + TEST.format(body='assert!(std::fs::read_to_string(PARTS.concat()).unwrap().contains("ORIGINAL"));'),
    },
    "ignored module": {
        ".gitignore": "/pkg/src/generated.rs\n/target/\n",
        "pkg/src/generated.rs": 'pub const PAGE: &str = include_str!("../../docs/guide.md");\n',
        "pkg/src/lib.rs": 'include!("generated.rs");\n' + TEST.format(body=CHECK),
    },
    # Run-time navigation to the checkout root.
    "manifest pop navigation": {"pkg/src/lib.rs": TEST.format(body='let mut root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")); root.pop(); assert!(std::fs::read_to_string(root.join(["CHANGE", "LOG"].concat()).with_extension("md")).unwrap().contains("ORIGINAL"));')},
    "root package": {
        "Cargo.toml": ROOT_PACKAGE + '\n[workspace]\nmembers = ["pkg"]\n',
        "src/lib.rs": f'pub const PAGE: &str = include_str!("../docs/guide.md");\n{TEST.format(body=CHECK)}',
        "pkg/src/lib.rs": "",
    },
    "nested workspace navigation": {
        "Cargo.toml": '[workspace]\nmembers = ["crates/pkg"]\nresolver = "2"\n',
        "crates/pkg/Cargo.toml": PACKAGE,
        "crates/pkg/src/lib.rs": TEST.format(body='let root = std::path::Path::new("..").join(".."); assert!(std::fs::read_to_string(root.join(format!("{}{}.md", "CHANGE", "LOG"))).unwrap().contains("ORIGINAL"));'),
    },
    "sqlite readfile": {
        "pkg/src/lib.rs": TEST.format(body='let out = std::process::Command::new("sqlite3").arg(":memory:").arg("select readfile(\'../do\'||\'cs/\'||\'gu\'||\'ide.md\')").output().unwrap(); assert!(String::from_utf8_lossy(&out.stdout).contains("ORIGINAL"));'),
    },
    # File access hidden behind an alias, an import, a value, or a computed
    # argument that starts with a literal (review of 9beb56a).
    "aliased reader import": {"pkg/src/lib.rs": "use std::fs::read_to_string as load;\n" + TEST.format(body=f'assert!(load({CHANGELOG}).unwrap().contains("ORIGINAL"));')},
    "aliased reader module": {"pkg/src/lib.rs": "use std::fs as f;\n" + TEST.format(body=f'assert!(String::from_utf8(f::read({CHANGELOG}).unwrap()).unwrap().contains("ORIGINAL"));')},
    "imported reader function": {"pkg/src/lib.rs": "use std::fs::read;\n" + TEST.format(body=f'assert!(String::from_utf8(read({CHANGELOG}).unwrap()).unwrap().contains("ORIGINAL"));')},
    "reader as a value": {"pkg/src/lib.rs": TEST.format(body=f'let load = std::fs::read_to_string; assert!(load({CHANGELOG}).unwrap().contains("ORIGINAL"));')},
    "reader with turbofish": {"pkg/src/lib.rs": TEST.format(body=f'assert!(std::fs::read_to_string::<&str>({CHANGELOG}).unwrap().contains("ORIGINAL"));')},
    "literal-prefixed path": {"pkg/src/lib.rs": TEST.format(body='assert!(std::fs::read_to_string("../CHANGE".to_owned() + "LOG.md").unwrap().contains("ORIGINAL"));')},
    "spaced path tokens": {"pkg/src/lib.rs": TEST.format(body=f'assert!(String::from_utf8(std :: fs :: read({CHANGELOG}).unwrap()).unwrap().contains("ORIGINAL"));')},
    "comment in a path": {"pkg/src/lib.rs": TEST.format(body=f'assert!(String::from_utf8(std::fs::/* x */read({CHANGELOG}).unwrap()).unwrap().contains("ORIGINAL"));')},
    "aliased include macro": {"pkg/src/lib.rs": f'use std::include_str as inc;\npub const PAGE: &str = inc!(concat!("../../CHANGE", "LOG.md"));\n{TEST.format(body=CHECK)}'},
    "build script helper module": {"pkg/build.rs": "mod helper;\nfn main() { helper::check(); }\n", "pkg/helper.rs": f'pub fn check() {{ assert!(std::fs::read_to_string({CHANGELOG}).unwrap().contains("ORIGINAL")); }}\n', "pkg/src/lib.rs": ""},
    # A reviewed run-time reader of a temporary file keeps the exclusion.
    "reviewed reader": {"pkg/src/lib.rs": REVIEWED, "scripts/audited-readers.toml": REVIEWED_REGISTRY},
    # The reviewed reader's path initializer now names the changelog: the
    # registry entry still matches its call line but not its source context.
    "edited reviewed reader": {"pkg/src/lib.rs": REVIEWED.replace("let path = std::env::current_exe().unwrap();", f"let path: std::path::PathBuf = {CHANGELOG}.into();"), "scripts/audited-readers.toml": REVIEWED_REGISTRY},
    # Lexical forms that hid readers from v12 (comments, raw identifiers, macro templates).
    "comment-separated use": {"pkg/src/lib.rs": "use/*audit*/std::fs as f;\n" + TEST.format(body=f'assert!(String::from_utf8(f::read({CHANGELOG}).unwrap()).unwrap().contains("ORIGINAL"));')},
    "raw identifier import": {"pkg/src/lib.rs": "use std::r#fs as f;\n" + TEST.format(body=f'assert!(String::from_utf8(f::read({CHANGELOG}).unwrap()).unwrap().contains("ORIGINAL"));')},
    "raw identifier include alias": {"pkg/src/lib.rs": f'use std::r#include_str as load;\npub const PAGE: &str = load!(concat!("../../CHANGE", "LOG.md"));\n{TEST.format(body=CHECK)}'},
    "macro template reader": {"pkg/src/lib.rs": "macro_rules! via { ($reader:ident, $path:expr) => { std::fs::$reader($path) }; }\n" + TEST.format(body=f'assert!(via!(read_to_string, {CHANGELOG}).unwrap().contains("ORIGINAL"));')},
    # Reviewed readers whose provenance changed after review (v12 kept them reviewed).
    "literal whitespace in a reviewed reader": {"pkg/src/lib.rs": LITERAL.replace('const KIND: &str = "temporary  file";', 'const KIND: &str = "temporary file";'), "scripts/audited-readers.toml": reviewed({"pkg/src/lib.rs": LITERAL}, ("pkg/src/lib.rs", "std::fs::read"))},
    "helper in another file": {"pkg/src/lib.rs": HELPER, "pkg/src/helper.rs": f"pub fn page_path() -> std::path::PathBuf {{ {CHANGELOG}.into() }}\n", "scripts/audited-readers.toml": reviewed({"pkg/src/lib.rs": HELPER, "pkg/src/helper.rs": HELPER_SAFE}, ("pkg/src/lib.rs", "std::fs::read"))},
    # Controls: the same reviewed readers before the change keep the exclusion.
    "reviewed literal control": {"pkg/src/lib.rs": LITERAL, "scripts/audited-readers.toml": reviewed({"pkg/src/lib.rs": LITERAL}, ("pkg/src/lib.rs", "std::fs::read"))},
    "reviewed helper control": {"pkg/src/lib.rs": HELPER, "pkg/src/helper.rs": HELPER_SAFE, "scripts/audited-readers.toml": reviewed({"pkg/src/lib.rs": HELPER, "pkg/src/helper.rs": HELPER_SAFE}, ("pkg/src/lib.rs", "std::fs::read"))},
    "uppercase reviewed parameter": {"pkg/src/lib.rs": UPPER, "scripts/audited-readers.toml": reviewed({"pkg/src/lib.rs": UPPER}, ("pkg/src/lib.rs", "std::fs::read"))},
    "destructured reviewed parameter": {"pkg/src/lib.rs": DESTRUCTURED, "scripts/audited-readers.toml": reviewed({"pkg/src/lib.rs": DESTRUCTURED}, ("pkg/src/lib.rs", "std::fs::read"))},
    # An example with `test = true` runs under `cargo test`.
    "tested example": {
        "pkg/Cargo.toml": PACKAGE + '\n[[example]]\nname = "tool"\ntest = true\n',
        "pkg/examples/tool.rs": "fn main() {}\n" + TEST.format(body=f'assert!(std::fs::read_to_string({CHANGELOG}).unwrap().contains("ORIGINAL"));'),
        "pkg/src/lib.rs": "",
    },
    # Control: nothing reads the pages, so the policy should keep excluding them.
    "unrelated control": {"pkg/src/lib.rs": TEST.format(body="assert_eq!(1 + 1, 2);")},
}


# Code `cargo check` runs: build-script modules, proc macros, and build dependencies.
READ_PAGE = 'assert!(std::fs::read_to_string(std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join(concat!("../CHANGE", "LOG.md"))).unwrap().contains("ORIGINAL"));'
CHECK_CASES = {
    "build script helper module": {"pkg/build.rs": "mod helper;\nfn main() { helper::check(); }\n", "pkg/helper.rs": f"pub fn check() {{ {READ_PAGE} }}\n", "pkg/src/lib.rs": ""},
    "comment-separated build module": {"pkg/build.rs": "mod/*audit*/helper;\nfn main() { helper::check(); }\n", "pkg/helper.rs": f"pub fn check() {{ {READ_PAGE} }}\n", "pkg/src/lib.rs": ""},
    "proc macro": {
        "Cargo.toml": '[workspace]\nmembers = ["pkg", "mac"]\nresolver = "2"\n',
        "mac/Cargo.toml": '[package]\nname = "mac"\nversion = "0.1.0"\nedition = "2021"\n\n[lib]\nproc-macro = true\n',
        "mac/src/lib.rs": f'use proc_macro::TokenStream;\n#[proc_macro]\npub fn page(_: TokenStream) -> TokenStream {{ {READ_PAGE} "1".parse().unwrap() }}\n',
        "pkg/Cargo.toml": PACKAGE + '\n[dependencies]\nmac = { path = "../mac" }\n',
        "pkg/src/lib.rs": "pub const ONE: u8 = mac::page!();\n",
    },
    "spaced build-script reader": {"pkg/build.rs": f"fn main() {{ assert!(String::from_utf8(std :: fs :: read(std::path::Path::new(&std::env::var(\"CARGO_MANIFEST_DIR\").unwrap()).join({CHANGELOG})).unwrap()).unwrap().contains(\"ORIGINAL\")); }}\n", "pkg/src/lib.rs": ""},
    "build dependency": {
        "Cargo.toml": '[workspace]\nmembers = ["pkg", "gen"]\nresolver = "2"\n',
        "gen/Cargo.toml": '[package]\nname = "gen"\nversion = "0.1.0"\nedition = "2021"\n',
        "gen/src/lib.rs": f"pub fn check() {{ {READ_PAGE} }}\n",
        "pkg/Cargo.toml": PACKAGE + '\n[build-dependencies]\ngen = { path = "../gen" }\n',
        "pkg/build.rs": "fn main() { gen::check(); }\n",
        "pkg/src/lib.rs": "",
    },
}


def cargo_test(root: Path, target: Path, command: str = "test") -> bool:
    env = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_NET_OFFLINE="true")
    return subprocess.run(["cargo", command, "--quiet", "--offline", "--workspace"], cwd=root, env=env, capture_output=True).returncode == 0


@unittest.skipUnless(ENABLED, "set WALLET_LIB_CARGO_ORACLES=1 to run real Cargo oracles")
class CargoOracleTests(unittest.TestCase):
    def fixture(self, files: dict[str, str]) -> Path:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name).resolve()
        for name, text in {"Cargo.toml": WORKSPACE, "pkg/Cargo.toml": PACKAGE, ".gitignore": "/target/\n", **PAGES, **files}.items():
            (root / name).parent.mkdir(parents=True, exist_ok=True)
            (root / name).write_text(text)
        git = ["git", "-c", "user.name=oracle", "-c", "user.email=oracle@example.invalid", "-c", "commit.gpgsign=false"]
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        subprocess.run(git + ["add", "."], cwd=root, check=True)
        subprocess.run(git + ["commit", "-qm", "fixture"], cwd=root, check=True)
        return root

    def test_excluded_pages_never_change_a_fresh_cargo_result(self):
        for case, files in CASES.items():
            if case == "sqlite readfile" and not shutil.which("sqlite3"):
                continue
            with self.subTest(case=case), tempfile.TemporaryDirectory() as targets:
                root = self.fixture(files)
                with patch.object(dev, "ROOT", root):
                    policy = dev.input_policy("test", dev.repository_files())
                baseline = cargo_test(root, Path(targets) / "baseline")
                self.assertTrue(baseline, f"{case}: fixture must pass before any edit")
                for page in policy["excluded"]:
                    original = (root / page).read_text()
                    (root / page).write_text("MUTATED\n")
                    try:
                        self.assertEqual(cargo_test(root, Path(targets) / page.replace("/", "_")), baseline, f"{case}: excluded {page} changed the result ({policy})")
                    finally:
                        (root / page).write_text(original)
                if case in {"unrelated control", "reviewed reader", "reviewed literal control", "reviewed helper control"} and hasattr(dev, "definition_index"):
                    self.assertIsNone(policy["fallback_reason"])
                    self.assertIn("docs/guide.md", policy["excluded"])

    def test_excluded_pages_never_change_a_fresh_cargo_check(self):
        for case, files in CHECK_CASES.items():
            with self.subTest(case=case), tempfile.TemporaryDirectory() as targets:
                root = self.fixture(files)
                with patch.object(dev, "ROOT", root):
                    policy = dev.input_policy("check", dev.repository_files())
                baseline = cargo_test(root, Path(targets) / "baseline", "check")
                self.assertTrue(baseline, f"{case}: fixture must pass before any edit")
                for page in policy["excluded"]:
                    original = (root / page).read_text()
                    (root / page).write_text("MUTATED\n")
                    try:
                        self.assertEqual(cargo_test(root, Path(targets) / page.replace("/", "_"), "check"), baseline, f"{case}: excluded {page} changed the result ({policy})")
                    finally:
                        (root / page).write_text(original)

    def test_selected_packages_keep_dev_dependencies_in_any_order(self):
        # `scan` depends on `helper`; helper's tests call its dev-dependency
        # `reader`, which reads the changelog. Selecting helper first must not
        # lose helper's dev-dependencies when scan reaches it as a dependency.
        workspace = '[workspace]\nmembers = ["helper", "scan", "reader"]\nresolver = "2"\n'
        manifest = '[package]\nname = "{}"\nversion = "0.1.0"\nedition = "2021"\n'
        root = self.fixture({
            "Cargo.toml": workspace,
            "reader/Cargo.toml": manifest.format("reader"),
            "reader/src/lib.rs": f'pub fn page() -> String {{ std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join({CHANGELOG})).unwrap() }}\n',
            "helper/Cargo.toml": manifest.format("helper") + '\n[dev-dependencies]\nreader = { path = "../reader" }\n',
            "helper/src/lib.rs": TEST.format(body='assert!(reader::page().contains("ORIGINAL"));'),
            "scan/Cargo.toml": manifest.format("scan") + '\n[dependencies]\nhelper = { path = "../helper" }\n',
            "scan/src/lib.rs": "",
        })
        env = dict(os.environ, CARGO_NET_OFFLINE="true")
        run = lambda target: subprocess.run(["cargo", "test", "--quiet", "--offline", "-p", "helper", "-p", "scan"], cwd=root, env=dict(env, CARGO_TARGET_DIR=str(target)), capture_output=True).returncode == 0
        with tempfile.TemporaryDirectory() as targets:
            self.assertTrue(run(Path(targets) / "baseline"))
            for selection in (["helper", "scan"], ["scan", "helper"]):
                with self.subTest(selection=selection), patch.object(dev, "ROOT", root):
                    policy = dev.input_policy("test", dev.repository_files(), selection)
                    self.assertIn("reader", policy["packages"])
                    for page in policy["excluded"]:
                        original = (root / page).read_text()
                        (root / page).write_text("MUTATED\n")
                        try:
                            self.assertTrue(run(Path(targets) / page.replace("/", "_")), f"excluded {page} changed the result ({policy})")
                        finally:
                            (root / page).write_text(original)

    def test_consumed_ignored_edits_change_the_digest_when_cargo_results_change(self):
        # An ignored generated module whose value a test checks (review case
        # 42 -> 43), loaded by `include!` or by an implicit `mod` declaration.
        # An unknown reader (full inputs) loading an ignored file outside every
        # package that nothing names must still see the edit.
        root = self.fixture({
            ".gitignore": "/target/\n/notes/\n",
            "notes/reader.input": "42\n",
            "pkg/src/lib.rs": TEST.format(body='assert_eq!(std::fs::read_to_string(["../notes/reader", ".input"].concat()).unwrap().trim(), "42");'),
        })
        with tempfile.TemporaryDirectory() as targets, patch.object(dev, "ROOT", root):
            self.assertTrue(cargo_test(root, Path(targets) / "before"))
            before = dev.source_state("test")
            self.assertIsNotNone(before["policy"]["fallback_reason"])
            (root / "notes/reader.input").write_text("43\n")
            self.assertFalse(cargo_test(root, Path(targets) / "after"))
            self.assertEqual(dev.compare(before, dev.source_state("test"))[0], ["other"])
        for case, lib in {"include": 'include!("generated.rs");\n', "implicit module": "mod generated;\nuse generated::VALUE;\n"}.items():
            with self.subTest(case=case), tempfile.TemporaryDirectory() as targets:
                root = self.fixture({
                    ".gitignore": "/target/\n/pkg/src/generated.rs\n",
                    "pkg/src/generated.rs": "pub const VALUE: u32 = 42;\n",
                    "pkg/src/lib.rs": lib + TEST.format(body="assert_eq!(VALUE, 42);"),
                })
                with patch.object(dev, "ROOT", root):
                    self.assertTrue(cargo_test(root, Path(targets) / "before"))
                    before = {command: dev.source_state(command) for command in ("test", "verify")}
                    (root / "pkg/src/generated.rs").write_text("pub const VALUE: u32 = 43;\n")
                    self.assertFalse(cargo_test(root, Path(targets) / "after"))
                    for command, state in before.items():
                        with self.subTest(command=command):
                            self.assertEqual(dev.compare(state, dev.source_state(command))[0], ["rust-source"])


if __name__ == "__main__":
    unittest.main()
