"""Behavioral regressions for leases, filtered checks, source attribution, and input policies."""
import argparse
import importlib.util
import json
import multiprocessing
import os
from pathlib import Path
import subprocess
import tempfile
import sys
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("dev", Path(__file__).parents[1] / "dev.py")
dev = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dev)


def state(sha="a", files=None, command="test"):
    """A synthetic source state for execute() tests that do not need a checkout."""
    policy = {"name": "rust-check" if command in dev.RUST_CHECK_COMMANDS else "full", "version": dev.POLICY_VERSION, "fallback_reason": None, "protected": [], "excluded": []}
    return {"sha": sha, "inputs": "b", "policy": policy, "files": files or {"src/lib.rs": "b"}}


def acquire(root, ready, release):
    with dev.Lease(Path(root), "test") as lease:
        ready.put(str(lease.path))
        release.get(timeout=10)


class WorkflowTests(unittest.TestCase):
    def test_custom_target_stays_under_build_root_and_tracks_json_changes(self):
        with tempfile.TemporaryDirectory() as root:
            target = Path(root) / "target.json"
            target.write_text('{"arch":"x86_64"}')
            with patch.dict(os.environ, {"CARGO_BUILD_TARGET": str(target)}), patch.object(dev, "capture", return_value="host: x86_64-unknown-linux-gnu"):
                first = dev.build_identity("default", "test")
                self.assertFalse(Path(first).is_absolute())
                self.assertEqual(Path(first).parts[0], "target.json")
                target.write_text('{"arch":"aarch64"}')
                self.assertNotEqual(first, dev.build_identity("default", "test"))

    def test_parallel_owners_reuse_released_directory(self):
        with tempfile.TemporaryDirectory() as root:
            context = multiprocessing.get_context("fork")
            ready, release = context.Queue(), context.Queue()
            child = context.Process(target=acquire, args=(root, ready, release))
            child.start()
            first = Path(ready.get(timeout=10))
            try:
                with dev.Lease(Path(root), "test") as second:
                    self.assertNotEqual(first, second.path)
                release.put(True)
                child.join(timeout=10)
                self.assertEqual(child.exitcode, 0)
                with dev.Lease(Path(root), "test") as reused:
                    self.assertEqual(first, reused.path)
            finally:
                if child.is_alive():
                    child.kill()
                    child.join()

    def test_killed_owner_releases_lease(self):
        with tempfile.TemporaryDirectory() as root:
            context = multiprocessing.get_context("fork")
            ready, release = context.Queue(), context.Queue()
            child = context.Process(target=acquire, args=(root, ready, release))
            child.start()
            first = Path(ready.get(timeout=10))
            child.kill()
            child.join()
            with dev.Lease(Path(root), "test") as reused:
                self.assertEqual(first, reused.path)
                self.assertEqual(json.loads((reused.path / "owner.json").read_text())["pid"], os.getpid())

    def test_child_retains_lease_after_wrapper_exit(self):
        with tempfile.TemporaryDirectory() as root:
            with dev.Lease(Path(root), "test") as owner:
                first = owner.path
                child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(30)"], pass_fds=(owner.lock.fileno(),))
            try:
                with dev.Lease(Path(root), "test") as second:
                    self.assertNotEqual(first, second.path)
            finally:
                child.terminate()
                child.wait(timeout=10)
            with dev.Lease(Path(root), "test") as reused:
                self.assertEqual(first, reused.path)

    def run_filtered(self, listing, states=None):
        args = argparse.Namespace(command="test", config="transparent", package=["zakura-client-sqlite"], profile=None, only=None, filter="ledger", exact=False)
        with tempfile.TemporaryDirectory() as root, patch.dict(os.environ, {"WALLET_LIB_BUILD_ROOT": root}), patch.object(dev, "build_identity", return_value="test"), patch.object(dev, "source_state", side_effect=states or [state()] * 3), patch.object(dev, "run", side_effect=[subprocess.CompletedProcess([], 0, listing), subprocess.CompletedProcess([], 0)]) as run:
            code = dev.execute(args)
            result = json.loads(next(Path(root).glob("**/last-result.json")).read_text())
            return code, result, run.call_args_list

    def test_empty_selection_fails_without_running_tests(self):
        code, result, calls = self.run_filtered("0 tests, 0 benchmarks\n")
        self.assertEqual(code, 2)
        self.assertEqual(result["status"], "fail")
        self.assertEqual(len(calls), 1)

    def test_selected_tests_run_with_configuration_and_no_dependency_lint(self):
        code, result, calls = self.run_filtered("wallet::ledger: test\n")
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 2)
        command = calls[1].args[0]
        self.assertIn("orchard,transparent-inputs,test-dependencies,unstable", command)
        self.assertIn("zakura-client-sqlite", command)
        self.assertEqual(result["status"], "pass")

    def test_changed_inputs_invalidate_success(self):
        old, new = state(), state(files={"src/lib.rs": "c"})
        code, result, _ = self.run_filtered("ledger: test\n", [old, new])
        self.assertEqual(code, 3)
        self.assertEqual(result["status"], "invalidated")
        self.assertEqual(result["changed_inputs"], ["rust-source"])
        self.assertNotIn("files", result["source"])

    def test_facade_requires_explicit_verification(self):
        with self.assertRaisesRegex(ValueError, "exclusive backends"):
            dev.cargo_args(argparse.Namespace(package=["zakura-wallet-lib"], config="default"))


FIXTURE = {
    "Cargo.toml": '[workspace]\nmembers = ["pkg"]\n',
    "Cargo.lock": "version = 4\n",
    "CHANGELOG.md": "# Changelog\n",
    "README.md": "# Root guidance\n",
    "docs/guide.md": "# Unrelated guide\n",
    "docs/nested/notes.md": "# Unrelated notes\n",
    "docs/included.md": "# Literal reference\n",
    "docs/diagram.svg": "<svg/>\n",
    "pkg/Cargo.toml": '[package]\nname = "pkg"\nreadme = "README.md"\n',
    "pkg/README.md": "```\nassert!(true);\n```\n",
    "pkg/build.rs": 'fn main() { println!("cargo:rerun-if-changed=proto/a.proto"); }\n',
    "pkg/proto/a.proto": 'syntax = "proto3";\n',
    "pkg/src/lib.rs": '#![doc = include_str!("../README.md")]\n//! Design: see `docs/included.md`.\nconst Q: char = \'"\';\n/// ```\n/// assert_eq!(pkg::ANSWER, 42);\n/// ```\npub const ANSWER: u8 = 42;\n',
    "pkg/tests/fixtures/record.bin": "\x00\x01",
    "pkg/tests/fixtures/notes.md": "fixture markdown\n",
}


class InputPolicyTests(unittest.TestCase):
    """Disposable checkouts covering what a Cargo result may and may not ignore."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()
        for name, text in FIXTURE.items():
            self.write(name, text)
        self.git("init", "-q")
        self.commit()
        patcher = patch.object(dev, "ROOT", self.root)
        patcher.start()
        self.addCleanup(patcher.stop)

    def git(self, *argv):
        subprocess.run(["git", "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", *argv], cwd=self.root, check=True, capture_output=True)

    def commit(self):
        self.git("add", ".")
        self.git("commit", "-q", "--allow-empty", "-m", "fixture")

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def edit(self, name, text="edited\n"):
        self.write(name, text)

    def change(self, mutate, command="test"):
        before = dev.source_state(command)
        mutate()
        after = dev.source_state(command)
        changed, ignored = dev.compare(before, after)
        return changed, ignored, before, after

    def test_audited_prose_is_excluded_and_literal_references_are_protected(self):
        policy = dev.source_state("test")["policy"]
        self.assertEqual((policy["name"], policy["version"], policy["fallback_reason"]), ("rust-check", dev.POLICY_VERSION, None))
        self.assertEqual(policy["excluded"], ["CHANGELOG.md", "docs/guide.md", "docs/nested/notes.md"])
        self.assertEqual(policy["protected"], ["docs/included.md"])

    def test_unrelated_documentation_and_changelog_edits_keep_result_valid(self):
        def mutate():
            self.edit("docs/guide.md")
            self.edit("docs/nested/notes.md")
            self.edit("CHANGELOG.md")
        changed, ignored, before, after = self.change(mutate)
        self.assertEqual((changed, ignored), ([], ["documentation-prose"]))
        self.assertEqual(before["inputs"], after["inputs"])

    def test_documentation_add_and_delete_keep_result_valid(self):
        def mutate():
            self.edit("docs/new-page.md")
            (self.root / "docs/guide.md").unlink()
        self.assertEqual(self.change(mutate)[:2], ([], ["documentation-prose"]))

    def test_included_markdown_fixtures_and_assets_invalidate(self):
        cases = {
            "docs/included.md": "included-markdown",
            "pkg/README.md": "included-markdown",
            "README.md": "included-markdown",
            "pkg/tests/fixtures/notes.md": "included-markdown",
            "pkg/tests/fixtures/record.bin": "fixture-or-asset",
            "docs/diagram.svg": "other",
        }
        for name, category in cases.items():
            with self.subTest(name=name):
                changed, ignored, _, _ = self.change(lambda: self.edit(name, f"changed {name}\n"))
                self.assertEqual((changed, ignored), ([category], []))

    def test_rust_doctest_lockfile_manifest_and_build_inputs_invalidate(self):
        cases = {
            "pkg/src/lib.rs": "rust-source",
            "Cargo.lock": "lockfile",
            "pkg/Cargo.toml": "cargo-manifest-or-config",
            "pkg/build.rs": "build-script-input",
            "pkg/proto/a.proto": "build-script-input",
        }
        for name, category in cases.items():
            with self.subTest(name=name):
                text = (self.root / name).read_text()
                changed, _, _, _ = self.change(lambda: self.edit(name, text + "\n// changed\n" if name.endswith(".rs") else text + "\n"))
                self.assertEqual(changed, [category])

    def test_added_and_deleted_sources_invalidate(self):
        self.assertEqual(self.change(lambda: self.edit("pkg/src/extra.rs", "pub fn extra() {}\n"))[0], ["rust-source"])
        self.assertEqual(self.change(lambda: (self.root / "pkg/tests/fixtures/record.bin").unlink())[0], ["fixture-or-asset"])

    def test_head_change_invalidates_even_for_documentation_commits(self):
        def mutate():
            self.edit("docs/guide.md")
            self.commit()
        changed, ignored, _, _ = self.change(mutate)
        self.assertEqual((changed, ignored), (["head"], ["documentation-prose"]))

    def test_full_policies_notice_documentation_edits(self):
        for command in ("verify", "doctor", "unknown"):
            with self.subTest(command=command):
                changed, ignored, before, _ = self.change(lambda: self.edit("docs/guide.md", f"{command}\n"), command)
                self.assertEqual((changed, ignored), (["documentation-prose"], []))
                self.assertEqual(before["policy"]["name"], "full")
                self.assertIn(command, before["policy"]["fallback_reason"])

    def test_computed_and_unresolved_references_fall_back_to_full_inputs(self):
        sources = {
            "computed include": 'const D: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/guide.md"));\n',
            "unresolved documentation reference": 'fn d(p: &std::path::Path) -> std::path::PathBuf { p.join("docs") }\n',
            "unresolved documentation reference ": 'fn d(n: &str) -> String { format!("../docs/{n}.md") }\n',
            "computed directory traversal": 'fn d() { std::fs::read_dir(env!("CARGO_MANIFEST_DIR")).unwrap(); }\n',
        }
        for reason, source in sources.items():
            with self.subTest(reason=reason):
                self.edit("pkg/src/computed.rs", source)
                changed, ignored, before, _ = self.change(lambda: self.edit("docs/guide.md", reason))
                self.assertIn(reason.strip(), before["policy"]["fallback_reason"])
                self.assertEqual((changed, ignored), (["documentation-prose"], []))

    def test_basename_references_and_unresolved_path_dependencies_are_conservative(self):
        self.edit("pkg/src/name.rs", 'const N: &str = "guide.md";\n')
        self.assertIn("docs/guide.md", dev.source_state("test")["policy"]["protected"])
        self.edit("pkg/Cargo.toml", FIXTURE["pkg/Cargo.toml"] + '\n[dependencies]\nmissing = { path = "../missing" }\n')
        self.assertIn("unresolved path dependency", dev.source_state("test")["policy"]["fallback_reason"])

    def scenario(self, files: dict[str, str], executable=(), symlinks=(), command="test"):
        """Policy for the fixture plus files, then a clean fixture again."""
        for name, text in files.items():
            self.write(name, text)
        for name, target in symlinks:
            (self.root / name).parent.mkdir(parents=True, exist_ok=True)
            (self.root / name).symlink_to(target)
        for name in executable:
            (self.root / name).chmod(0o755)
        try:
            return dev.source_state(command)["policy"]
        finally:
            self.git("checkout", "-q", "--", ".")
            self.git("clean", "-qfdx")

    def test_files_cargo_reaches_are_audited_transitively(self):
        readme = 'Root guide.\n\n```rust\nlet page = std::fs::read_to_string("{}").unwrap();\n```\n'
        protected = {
            "root Markdown doctest": {"pkg/src/root.rs": '#![doc = include_str!("../../README.md")]\n', "README.md": readme.format("../docs/guide.md")},
            "included documentation doctest": {"pkg/src/page.rs": '#![doc = include_str!("../../docs/included.md")]\n', "docs/included.md": readme.format("../docs/guide.md")},
            "path with spaces": {"pkg/src/space.rs": 'const P: &str = include_str!("../../docs/my guide.md");\n', "docs/my guide.md": "# Spaced\n"},
            "escaped separators": {"pkg/src/escape.rs": 'const P: &str = include_str!("..\\\\..\\\\docs\\\\guide.md");\n'},
            "escaped directory name": {"pkg/src/escape.rs": 'const P: &str = "../../\\x64ocs/gu\\u{69}de.md";\n'},
            "raw string": {"pkg/src/raw.rs": 'const P: &str = include_str!(r#"..\\..\\docs\\guide.md"#);\n'},
            "ignored include module": {".gitignore": "/pkg/src/generated.rs\n", "pkg/src/module.rs": 'include!("generated.rs");\n', "pkg/src/generated.rs": 'const G: &str = include_str!("../../docs/guide.md");\n'},
            "root package": {"Cargo.toml": '[package]\nname = "app"\n\n[workspace]\nmembers = ["pkg"]\n', "src/lib.rs": '#![doc = include_str!("../docs/guide.md")]\n'},
            "metadata read by a build script": {"pkg/Cargo.toml": FIXTURE["pkg/Cargo.toml"] + '\n[package.metadata.embed]\npages = ["../docs/guide.md"]\n'},
            "case-insensitive file system": {"pkg/src/case.rs": 'const G: &str = include_str!("../../Docs/Guide.md");\n'},
            "ignored symlink": {".gitignore": "/pkg/assets/\n", "pkg/src/link.rs": 'const P: &str = include_str!("../assets/page.md");\n'},
            "extensionless page name": {"pkg/tests/cl.rs": 'const CHANGELOG: &str = "../CHANGELOG";\n'},
            "implicit module outside the package": {"pkg/src/shared.rs": '#[path = "../../shared/mod.rs"]\nmod shared;\n', "shared/mod.rs": "mod pages;\n", "shared/pages.rs": 'const G: &str = include_str!("../docs/guide.md");\n'},
            "metadata key": {"pkg/Cargo.toml": FIXTURE["pkg/Cargo.toml"] + '\n[package.metadata.embed-pages]\n"../docs/guide.md" = "guide"\n'},
        }
        for case, files in protected.items():
            with self.subTest(case=case):
                policy = self.scenario(files, ["scripts/gen.py"] if "scripts/gen.py" in files else (), [("pkg/assets/page.md", "../../docs/guide.md")] if case == "ignored symlink" else ())
                self.assertIsNone(policy["fallback_reason"])
                page = {"path with spaces": "docs/my guide.md", "extensionless page name": "CHANGELOG.md"}.get(case, "docs/guide.md")
                self.assertIn(page, policy["protected"])
        fallback = {
            "computed include": {"README.md": readme.format("x").replace("std::fs::read_to_string(\"x\")", 'include_str!(concat!("../docs/", "guide.md"))'), "pkg/src/root.rs": '#![doc = include_str!("../../README.md")]\n'},
            "documentation path fragments": {"pkg/src/split.rs": 'fn p(n: &str) -> String { format!("{}{}/{n}.md", "../../do", "cs") }\n'},
            "documentation path fragments ": {"pkg/src/head.rs": 'pub const HEAD: &str = "../../doc";\n', "pkg/src/tail.rs": 'fn p() -> String { format!("{}{}", crate::HEAD, "s/guide") }\n'},
            "unresolved documentation reference": {"pkg/src/prefix.rs": 'fn p() -> String { ["../../docs/gui", "de.md"].concat() }\n'},
            "process launch": {"pkg/build.rs": 'fn main() { std::process::Command::new("python3").arg("../scripts/gen.py").status().unwrap(); }\n'},
            "computed directory traversal": {"pkg/build.rs": 'fn main() { std::process::Command::new("../scripts/gen.py").status().unwrap(); }\n', "scripts/gen.py": "#!/usr/bin/env python3\nfrom pathlib import Path\nprint((Path(__file__).parents[1] / 'docs' / 'guide.md').read_text())\n"},
            "unresolved documentation reference ": {".gitignore": "/pkg/src/generated.rs\n", "pkg/src/generated.rs": 'fn p(n: &str) -> String { format!("../../docs/{n}.md") }\n'},
            "unresolved documentation reference  ": {"pkg/src/quote.rs": '/// Strips a leading " from the page name.\nfn p(n: &str) -> String { format!("../../docs/{n}.md") }\n'},
            "unresolved documentation reference   ": {"pkg/src/sep.rs": 'use std::path::MAIN_SEPARATOR as S;\nfn p(n: &str) -> String { format!("..{S}..{S}docs{S}{n}.md") }\n'},
            "process launch  ": {"pkg/build.rs": 'fn main() { std::process::Command::new("../scripts/gen.py").status().unwrap(); }\n', "scripts/gen.py": "#!/usr/bin/env python3\nprint(open('../docs/guide.md').read())\n"},
            "process launch ": {"pkg/build.rs": 'fn main() { std::process::Command::new("git").args(["status", "--porcelain"]).status().unwrap(); }\n'},
            "reaches the checkout root": {"pkg/tests/links.rs": '#[test]\nfn links() { for entry in ignore::Walk::new("..") { drop(entry); } }\n'},
            "computed file name": {"pkg/tests/top.rs": '#[test]\nfn notes() { for page in ["README", "CHANGELOG"] { std::fs::read_to_string(format!("../{page}.md")).unwrap(); } }\n'},
            "process launch   ": {"pkg/build.rs": 'use std::process::Command as Cmd;\nfn main() { Cmd::new("git").status().unwrap(); }\n'},
            "process launch    ": {"pkg/build.rs": 'include!("build/git.rs.in");\n', "pkg/build/git.rs.in": 'fn main() { std::process::Command::new("git").status().unwrap(); }\n'},
            "process launch     ": {"pkg/build.rs": 'fn main() { cmake::build("native"); }\n', "pkg/native/CMakeLists.txt": "file(GLOB PAGES ${PROJECT_SOURCE_DIR}/../../docs/*.md)\n"},
            "symlinked directory": {".gitignore": "/pkg/assets/\n", "pkg/assets/keep": "", "pkg/src/embed.rs": '#[derive(rust_embed::Embed)]\n#[folder = "assets/pages"]\npub struct Pages;\n'},
            "checkout-relative Cargo environment": {".cargo/config.toml": '[env]\nREPO_ROOT = { value = "", relative = true }\n'},
            "unresolvable symlink": {},
            "parent directory navigation ": {"pkg/tests/up.rs": 'use std::path::{Component, PathBuf};\n#[test]\nfn up() { let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR")); root.push(Component::ParentDir); drop(root); }\n'},
            "process launch      ": {"pkg/tests/sql.rs": '#[test]\nfn sql() { std::process::Command::new("sqlite3").arg(":memory:").arg("select readfile(\'../do\'||\'cs/guide.md\')").status().unwrap(); }\n'},
            "documentation path fragments  ": {"pkg/src/parts.json": '["../../do", "cs/", "guide", ".md"]\n', "pkg/src/lib.rs": FIXTURE["pkg/src/lib.rs"] + 'pub const PARTS: [&str; 4] = include!("parts.json");\n'},
            "run-time file access with a computed path": {"pkg/src/snippets.inc": 'pub fn page(p: &[&str]) -> String { std::fs::read_to_string(p.concat()).unwrap() }\n', "pkg/src/lib.rs": FIXTURE["pkg/src/lib.rs"] + 'include!("snippets.inc");\n'},
            "custom Cargo runner": {".cargo/config.toml": '[target.x86_64-unknown-linux-gnu]\nrunner = ["python3", "scripts/runner.py"]\n'},
            "computed directory traversal ": {"pkg/tests/top.rs": '#[test]\nfn notes() { let mut root = std::env::current_dir().unwrap(); root.pop(); std::fs::read_to_string(root.join("CHANGELOG").with_extension("md")).unwrap(); }\n'},
            "computed directory traversal  ": {"pkg/tests/links.rs": '#[test]\nfn links() { for entry in globwalk::GlobWalkerBuilder::new(".", "*.md").build().unwrap() { drop(entry); } }\n'},
            "parent directory navigation": {"crates/pkg/Cargo.toml": '[package]\nname = "nested"\n', "crates/pkg/tests/top.rs": '#[test]\nfn notes() { let root = std::path::Path::new("..").join(".."); std::fs::read_to_string(root.join(format!("{}.md", "CHANGELOG"))).unwrap(); }\n'},
            "work-tree reader": {"pkg/build.rs": 'fn main() { let repo = git2::Repository::discover(".").unwrap(); drop(repo); }\n'},
            "checkout-relative Cargo environment ": {".cargo/config": '[env]\nREPO_ROOT = { value = ".", relative = true }\n'},
            "unresolved path dependency": {"pkg/Cargo.toml": FIXTURE["pkg/Cargo.toml"] + '\n[dependencies]\nx = { path = "a\\u0000b" }\n'},
        }
        links = {"symlinked directory": [("pkg/assets/pages", "../../docs")], "unresolvable symlink": [("pkg/loop", "loop")]}
        for reason, files in fallback.items():
            with self.subTest(reason=reason):
                self.assertIn(reason.strip(), self.scenario(files, ["scripts/gen.py"] if "scripts/gen.py" in files else (), links.get(reason, ()))["fallback_reason"] or "")

    def test_root_package_keeps_unreferenced_documentation_excluded(self):
        policy = self.scenario({"Cargo.toml": '[package]\nname = "app"\n\n[workspace]\nmembers = ["pkg"]\n', "src/lib.rs": "pub fn f() {}\n"})
        self.assertIsNone(policy["fallback_reason"])
        self.assertIn("docs/guide.md", policy["excluded"])
        self.assertIn("docs/included.md", policy["protected"])

    def test_consumed_ignored_inputs_invalidate_and_unconsumed_do_not(self):
        # Ignored files in a built package may be read by any reader there, and
        # `mod implicit;` loads an ignored module without naming it in a literal.
        self.write(".gitignore", "/pkg/src/generated.rs\n/pkg/src/implicit.rs\n/pkg/tests/fixtures/local.hex\n/pkg/scratch.txt\n/notes/\n")
        self.write("pkg/src/generated.rs", "pub const G: u8 = 1;\n")
        self.write("pkg/src/implicit.rs", "pub const I: u8 = 1;\n")
        self.write("pkg/tests/fixtures/local.hex", "00\n")
        self.write("pkg/scratch.txt", "notes\n")
        self.write("notes/scratch.txt", "outside every package\n")
        self.write("pkg/src/lib.rs", FIXTURE["pkg/src/lib.rs"] + 'include!("generated.rs");\nmod implicit;\npub const H: &str = include_str!("../tests/fixtures/local.hex");\n')
        self.commit()
        cases = {"pkg/src/generated.rs": ["rust-source"], "pkg/src/implicit.rs": ["rust-source"], "pkg/tests/fixtures/local.hex": ["fixture-or-asset"], "pkg/scratch.txt": ["other"], "notes/scratch.txt": []}
        for name, expected in cases.items():
            with self.subTest(name=name):
                for command in ("test", "verify"):
                    self.assertEqual(self.change(lambda: self.edit(name, f"changed {command}\n"), command)[0], expected)

    def test_hidden_file_access_falls_back(self):
        """File access behind an alias, an import, a value, or a computed
        argument is an unproven reader (review of 9beb56a)."""
        read = '#[test]\nfn t() {{ {} }}\n'
        cases = {
            "aliased file access import": {"pkg/tests/a.rs": "use std::fs::read_to_string as load;\n" + read.format('load(concat!("../CHANGE", "LOG.md")).unwrap();')},
            "aliased file access import ": {"pkg/tests/a.rs": "use std::fs as f;\n" + read.format('f::read(concat!("../CHANGE", "LOG.md")).unwrap();')},
            "aliased file access import  ": {"pkg/tests/a.rs": "use std::{\n    fs::{self, File as Handle},\n    io,\n};\n" + read.format("drop(Handle::create);")},
            "aliased file access import   ": {"pkg/tests/a.rs": "use std::fs::*;\n" + read.format('read(concat!("../CHANGE", "LOG.md")).unwrap();')},
            "imported file access function": {"pkg/tests/a.rs": "use std::fs::read;\n" + read.format('read(concat!("../CHANGE", "LOG.md")).unwrap();')},
            "file access used as a value": {"pkg/tests/a.rs": read.format('let r = std::fs::read_to_string; r(concat!("../CHANGE", "LOG.md")).unwrap();')},
            "file access used as a value ": {"pkg/tests/a.rs": read.format('let pages: Vec<_> = [concat!("../CHANGE", "LOG.md")].iter().map(std::fs::File::open).collect();')},
            "run-time file access with a computed path": {"pkg/tests/a.rs": read.format('std::fs::read_to_string::<&str>(concat!("../CHANGE", "LOG.md")).unwrap();')},
            "run-time file access with a computed path ": {"pkg/tests/a.rs": read.format('std::fs::read_to_string("../CHANGE".to_owned() + "LOG.md").unwrap();')},
            "run-time file access with a computed path  ": {"pkg/tests/a.rs": read.format('let p: std::path::PathBuf = ("../CHANGE".to_owned() + "LOG.md").into(); assert!(p.exists());')},
            "run-time file access with a computed path   ": {"pkg/build.rs": "mod helper;\nfn main() { helper::check(); }\n", "pkg/helper.rs": 'pub fn check() { std::fs::read_to_string(concat!("../CHANGE", "LOG.md")).unwrap(); }\n'},
            "aliased include_str macro": {"pkg/src/inc.rs": 'use std::include_str as inc;\npub const P: &str = inc!(concat!("../../CHANGE", "LOG.md"));\n'},
        }
        for reason, files in cases.items():
            with self.subTest(reason=reason):
                command = "check" if "pkg/build.rs" in files or "pkg/src/inc.rs" in files else "test"
                self.assertIn(reason.strip(), self.scenario(files, command=command)["fallback_reason"] or "")
        # Prose that merely says "use", and code spans in documentation, are not code.
        safe = {
            "pkg/src/notes.rs": "// Count each use to the seeded address.\npub fn count(conn: &rusqlite::Connection) -> u8 { drop(conn); 1 }\n/// Like [`File::open`] or `std::fs::read`.\npub fn doc() {}\n",
            "pkg/tests/plain.rs": "use std::fs::{self, File};\nuse std::io::Read as _;\n" + read.format('drop(File::open("Cargo.toml").unwrap()); fs::read("Cargo.toml").unwrap();'),
        }
        self.assertIsNone(self.scenario(safe)["fallback_reason"])

    def test_only_the_reviewed_sqlite3_invocation_is_exempt(self):
        name, function, _ = dev.AUDITED_SQLITE
        call = '    let output = Command::new("sqlite3")\n        .arg(db_path)\n        .arg("-safe")\n        .arg("-readonly")\n        .arg(command)\n        .output()\n        .expect("failed");\n'
        source = "use std::{ffi::OsStr, process::Command};\n" + function + "\nCALL}\n"
        cases = {
            "reviewed invocation": ({"pkg/Cargo.toml": FIXTURE["pkg/Cargo.toml"], name: source.replace("CALL", call)}, None),
            "nonce re-enables file access": ({name: source.replace("CALL", call.replace('.arg("-readonly")', '.arg("-readonly")\n        .arg("--nonce")'))}, "process launch"),
            "same call in another file": ({"pkg/src/sql.rs": source.replace("CALL", call)}, "process launch"),
            "same call in another function": ({name: source.replace("run_sqlite3", "dump").replace("CALL", call)}, "process launch"),
        }
        package = str(Path(name).parents[1])
        for case, (files, reason) in cases.items():
            with self.subTest(case=case):
                files = {f"{package}/Cargo.toml": '[package]\nname = "sqlite"\n', **files}
                fallback = self.scenario(files)["fallback_reason"]
                self.assertIn(reason, fallback) if reason else self.assertIsNone(fallback)

    def test_reader_registry_binds_reviewed_sites(self):
        reader = 'pub fn open(path: &std::path::Path) -> String {\n    std::fs::read_to_string(path).unwrap()\n}\n'
        caller = '#[test]\nfn opens() {\n    let file = tempfile::NamedTempFile::new().unwrap();\n    crate::io::open(file.path());\n}\n'
        registry = (
            '[[wrapper]]\ncall = "io::open"\n\n'
            '[[reader]]\nfile = "pkg/src/io.rs"\nfunction = "pub fn open(path: &std::path::Path) -> String {"\nline = "std::fs::read_to_string(path).unwrap()"\nreason = "caller path; every io::open call is reviewed"\n\n'
            '[[reader]]\nfile = "pkg/tests/io.rs"\nfunction = "fn opens() {"\nline = "crate::io::open(file.path());"\nreason = "NamedTempFile"\n'
        )
        base = {"pkg/src/io.rs": reader, "pkg/tests/io.rs": caller, dev.READER_REGISTRY: registry}
        cases = {
            "reviewed": (base, None),
            "unreviewed registry absent": ({**base, dev.READER_REGISTRY: ""}, "run-time file access with a computed path in pkg/src/io.rs"),
            "edited reviewed call": ({**base, "pkg/src/io.rs": reader.replace("(path)", "(path.with_extension(\"md\"))")}, "run-time file access with a computed path in pkg/src/io.rs"),
            "new wrapper call": ({**base, "pkg/tests/io.rs": caller + '#[test]\nfn more() {\n    crate::io::open(std::path::Path::new(&std::env::var("PAGE").unwrap()));\n}\n'}, "run-time file access with a computed path in pkg/tests/io.rs"),
            "moved to another function": ({**base, "pkg/src/io.rs": reader.replace("pub fn open", "pub fn load")}, "run-time file access with a computed path in pkg/src/io.rs"),
            "qualified wrapper call": ({**base, "pkg/tests/io.rs": caller + '#[test]\nfn more() {\n    pkg::io::open(std::path::Path::new(&std::env::var("PAGE").unwrap()));\n}\n'}, "run-time file access with a computed path in pkg/tests/io.rs"),
            "wrapper as a value": ({**base, "pkg/tests/io.rs": caller + '#[test]\nfn more() {\n    let f = crate::io::open;\n    drop(f);\n}\n'}, "file access used as a value in pkg/tests/io.rs"),
            "aliased wrapper": ({**base, "pkg/tests/io.rs": "use crate::io::open as load;\n" + caller}, "aliased file access import in pkg/tests/io.rs"),
            "stale entry": ({**base, dev.READER_REGISTRY: registry + '\n[[reader]]\nfile = "pkg/src/gone.rs"\nfunction = "fn gone() {"\nline = "std::fs::read(p)"\nreason = "removed"\n'}, "stale audited reader pkg/src/gone.rs"),
            "malformed registry": ({**base, dev.READER_REGISTRY: "[[reader]]\nfile = 1\n"}, "unreadable reader registry"),
        }
        for case, (files, reason) in cases.items():
            with self.subTest(case=case):
                fallback = self.scenario(files)["fallback_reason"]
                self.assertIn(reason, fallback) if reason else self.assertIsNone(fallback)
        # The registry is a tracked input: editing it invalidates every policy.
        self.write(dev.READER_REGISTRY, registry)
        self.commit()
        for command in ("test", "verify"):
            self.assertEqual(self.change(lambda: self.edit(dev.READER_REGISTRY, registry + f"# reviewed again for {command}\n"), command)[0], ["reader-registry"])

    def test_run_time_readers_count_only_where_the_operation_executes_them(self):
        reader = 'pub fn page(p: &str) -> String { std::fs::read_to_string(p).unwrap() }\n'
        cases = {
            ("check", "pkg/src/io.rs"): None,
            ("lint", "pkg/src/io.rs"): None,
            ("test", "pkg/src/io.rs"): "run-time file access",
            ("check", "pkg/build.rs"): "run-time file access",
            ("test", "pkg/examples/tool.rs"): None,
        }
        for (command, name), reason in cases.items():
            with self.subTest(command=command, name=name):
                self.write(name, reader)
                try:
                    fallback = dev.source_state(command)["policy"]["fallback_reason"]
                finally:
                    (self.root / name).unlink()
                self.assertIn(reason, fallback) if reason else self.assertIsNone(fallback)

    def test_selection_limits_the_audit_to_packages_cargo_builds(self):
        files = {
            "Cargo.toml": '[workspace]\nmembers = ["pkg", "other", "dep"]\n',
            "pkg/Cargo.toml": FIXTURE["pkg/Cargo.toml"] + '\n[dependencies]\ndep = { path = "../dep" }\n',
            "dep/Cargo.toml": '[package]\nname = "dep"\n\n[dev-dependencies]\nother = { path = "../other" }\n',
            "dep/src/lib.rs": "pub fn f() {}\n",
            "other/Cargo.toml": '[package]\nname = "other"\n',
            "other/src/lib.rs": 'pub fn page(p: &str) -> String { std::fs::read_to_string(p).unwrap() }\n',
        }
        for name, text in files.items():
            self.write(name, text)
        policy = dev.source_state("test", ["pkg"])["policy"]
        # A dependency's dev-dependencies are not built, so `other` is not reached.
        self.assertEqual(policy["packages"], ["dep", "pkg"])
        self.assertIsNone(policy["fallback_reason"])
        self.assertIn("other/src/lib.rs", dev.source_state("test", ["other"])["policy"]["fallback_reason"])
        self.assertIn("other/src/lib.rs", dev.source_state("test")["policy"]["fallback_reason"])
        args = argparse.Namespace(command="test", config="transparent", package=None)
        self.assertEqual(dev.selected_packages(args), ["zakura-client-backend", "zakura-client-sqlite"])
        self.assertIsNone(dev.selected_packages(argparse.Namespace(command="test", config="default", package=None)))

    def test_untracked_nested_repository_falls_back(self):
        self.write("pkg/vendor/helper/src/lib.rs", 'const G: &str = include_str!("../../../../docs/guide.md");\n')
        subprocess.run(["git", "init", "-q"], cwd=self.root / "pkg/vendor/helper", check=True)
        self.assertIn("nested repository or submodule pkg/vendor/helper/", dev.source_state("test")["policy"]["fallback_reason"])

    def test_malformed_or_unreadable_inputs_do_not_crash(self):
        cases = {
            "invalid TOML fixture": ({"pkg/tests/fixtures/invalid.toml": 'key = = "broken"\n'}, None),
            "env is not a table": ({".cargo/config.toml": 'env = "x"\n'}, None),
            "env value is not a string": ({".cargo/config.toml": "[env]\nX = { value = 1 }\n"}, None),
            "rustfmt-wrapped literal include": ({"pkg/src/wrapped.rs": '#[doc = include_str!(\n    "../README.md"\n)]\npub fn f() {}\n'}, None),
            "package binary launch": ({"pkg/tests/cli.rs": '#[test]\nfn cli() { std::process::Command::new(env!("CARGO_BIN_EXE_pkg")).status().unwrap(); }\n'}, None),
        }
        for case, (files, reason) in cases.items():
            with self.subTest(case=case):
                fallback = self.scenario(files)["fallback_reason"]
                self.assertIn(reason, fallback) if reason else self.assertIsNone(fallback)
        try:
            (self.root / os.fsdecode(b"pkg/caf\xe9.txt")).write_text("x")
        except OSError:
            pass  # APFS rejects non-UTF-8 names (EILSEQ); nothing to digest there
        else:
            self.assertIsNone(dev.source_state("test")["policy"]["fallback_reason"])
        self.write("scratch.log", "private\n")
        (self.root / "scratch.log").chmod(0)
        self.addCleanup((self.root / "scratch.log").chmod, 0o644)
        self.assertTrue(dev.source_state("verify")["files"]["scratch.log"].startswith("<unreadable"))

    def test_unreadable_ignored_package_file_falls_back(self):
        self.write(".gitignore", "/pkg/.data/\n")
        self.write("pkg/.data/volume", "private\n")
        (self.root / "pkg/.data/volume").chmod(0)
        self.addCleanup((self.root / "pkg/.data/volume").chmod, 0o644)
        self.assertIn("unreadable reference source pkg/.data/volume", dev.source_state("test")["policy"]["fallback_reason"])

    def test_unreached_files_and_data_do_not_force_fallback(self):
        policy = self.scenario({
            "scripts/tool.py": "from pathlib import Path\nprint(Path('docs').glob('*.md'))\n",
            "pkg/tests/fixtures/data.json": '{"scripts": ["d", "s/x"]}\n',
            "pkg/src/letters.rs": 'const L: [&str; 2] = ["d", "x"];\n',
        })
        self.assertIsNone(policy["fallback_reason"])
        self.assertIn("docs/guide.md", policy["excluded"])

    def test_manifest_references_including_metadata_protect_pages(self):
        self.assertIn("CHANGELOG.md", dev.source_state("test")["policy"]["excluded"])
        for manifest in ('include = ["../CHANGELOG.md"]\n', '\n[package.metadata.pages]\nchangelog = "../CHANGELOG.md"\n'):
            with self.subTest(manifest=manifest):
                self.assertIn("CHANGELOG.md", self.scenario({"pkg/Cargo.toml": FIXTURE["pkg/Cargo.toml"] + manifest})["protected"])

    def test_execute_receipt_records_policy_and_input_categories(self):
        args = argparse.Namespace(command="test", config="transparent", package=None, profile=None, only=None, filter=None, exact=False)
        for name, expected in (("docs/guide.md", (0, "pass", [], ["documentation-prose"])), ("pkg/src/lib.rs", (3, "invalidated", ["rust-source"], []))):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as build, patch.dict(os.environ, {"WALLET_LIB_BUILD_ROOT": build}), patch.object(dev, "build_identity", return_value="test"), patch.object(dev, "run", side_effect=lambda *a, **k: (self.edit(name, "during run\n"), subprocess.CompletedProcess([], 0))[1]):
                code = dev.execute(args)
                result = json.loads(next(Path(build).glob("**/last-result.json")).read_text())
                self.assertEqual((code, result["status"], result["changed_inputs"], result["ignored_changes"]), expected)
                self.assertEqual(result["source"]["policy"]["version"], dev.POLICY_VERSION)
                self.assertIsNone(result["source"]["policy"]["fallback_reason"])

    def test_repository_audit_relies_on_reviewed_readers(self):
        root = Path(__file__).resolve().parents[2]
        with patch.object(dev, "ROOT", root):
            for selection in (["zakura-client-sqlite"], None):
                with self.subTest(selection=selection):
                    policy = dev.input_policy("test", dev.repository_files(), selection)
                    self.assertIsNone(policy["fallback_reason"])
                    self.assertIn("docs/development.md", policy["excluded"])
                    self.assertGreater(policy["reviewed_readers"], 0)
            # Without the reviewed registry, computed run-time paths force full inputs.
            with patch.object(dev, "READER_REGISTRY", "scripts/missing-registry.toml"):
                policy = dev.input_policy("test", dev.repository_files(), ["zakura-client-sqlite"])
            self.assertIn("run-time file access with a computed path in librustzcash/zcash_client_sqlite/src/lib.rs", policy["fallback_reason"])


if __name__ == "__main__":
    unittest.main()
