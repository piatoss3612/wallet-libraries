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
BUILD_COPY = 'fn main() {{ let text = {read}; std::fs::write(std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("page.md"), text).unwrap(); }}\n'

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
    # A reviewed run-time reader of a temporary file keeps the exclusion.
    "reviewed reader": {
        "pkg/src/lib.rs": '#[test]\nfn oracle() {\n    let path = std::env::temp_dir().join(format!("oracle-{}.txt", std::process::id()));\n    std::fs::write(&path, "x").unwrap();\n    assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");\n}\n',
        "scripts/audited-readers.toml": "".join(
            f'[[reader]]\nfile = "pkg/src/lib.rs"\nfunction = "fn oracle() {{"\nline = {json.dumps(line)}\nreason = "file in the system temporary directory"\n\n'
            for line in ('std::fs::write(&path, "x").unwrap();', 'assert_eq!(std::fs::read_to_string(&path).unwrap(), "x");')
        ),
    },
    # Control: nothing reads the pages, so the policy should keep excluding them.
    "unrelated control": {"pkg/src/lib.rs": TEST.format(body="assert_eq!(1 + 1, 2);")},
}


def cargo_test(root: Path, target: Path) -> bool:
    env = dict(os.environ, CARGO_TARGET_DIR=str(target), CARGO_NET_OFFLINE="true")
    return subprocess.run(["cargo", "test", "--quiet", "--offline", "--workspace"], cwd=root, env=env, capture_output=True).returncode == 0


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
                if case in {"unrelated control", "reviewed reader"}:
                    self.assertIsNone(policy["fallback_reason"])
                    self.assertIn("docs/guide.md", policy["excluded"])

    def test_consumed_ignored_edits_change_the_digest_when_cargo_results_change(self):
        # An ignored generated module whose value a test checks (review case 42 -> 43).
        root = self.fixture({
            ".gitignore": "/target/\n/pkg/src/generated.rs\n",
            "pkg/src/generated.rs": "pub const VALUE: u32 = 42;\n",
            "pkg/src/lib.rs": 'include!("generated.rs");\n' + TEST.format(body="assert_eq!(VALUE, 42);"),
        })
        with tempfile.TemporaryDirectory() as targets, patch.object(dev, "ROOT", root):
            self.assertTrue(cargo_test(root, Path(targets) / "before"))
            before = {command: dev.source_state(command) for command in ("test", "verify")}
            (root / "pkg/src/generated.rs").write_text("pub const VALUE: u32 = 43;\n")
            self.assertFalse(cargo_test(root, Path(targets) / "after"))
            for command, state in before.items():
                with self.subTest(command=command):
                    self.assertEqual(dev.compare(state, dev.source_state(command))[0], ["rust-source"])


if __name__ == "__main__":
    unittest.main()
