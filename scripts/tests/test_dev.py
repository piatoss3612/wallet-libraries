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
        code, result, _ = self.run_filtered("ledger: test\n", [old, old, new])
        self.assertEqual(code, 3)
        self.assertEqual(result["status"], "invalidated")
        self.assertEqual(result["changed_inputs"], ["rust-source"])
        self.assertNotIn("files", result["source"])

    def test_facade_requires_explicit_verification(self):
        with self.assertRaisesRegex(ValueError, "exclusive backends"):
            dev.cargo_args(argparse.Namespace(package=["zakura-wallet-lib"], config="default"))


FIXTURE = {
    "Cargo.toml": '[workspace]\nmembers = ["pkg"]\n\n[workspace.metadata.release]\npre-release-replacements = [{file="CHANGELOG.md", search="x", replace="y"}]\n',
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

    def scenario(self, files: dict[str, str], executable=()):
        """Policy for the fixture plus files, then a clean fixture again."""
        for name, text in files.items():
            self.write(name, text)
        for name in executable:
            (self.root / name).chmod(0o755)
        try:
            return dev.source_state("test")["policy"]
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
            "executable generator": {"pkg/build.rs": 'fn main() { std::process::Command::new("../scripts/gen.py").status().unwrap(); }\n', "scripts/gen.py": "#!/usr/bin/env python3\nprint(open('../docs/guide.md').read())\n"},
            "ignored include module": {".gitignore": "/pkg/src/generated.rs\n", "pkg/src/module.rs": 'include!("generated.rs");\n', "pkg/src/generated.rs": 'const G: &str = include_str!("../../docs/guide.md");\n'},
        }
        for case, files in protected.items():
            with self.subTest(case=case):
                policy = self.scenario(files, ["scripts/gen.py"] if "scripts/gen.py" in files else ())
                self.assertIsNone(policy["fallback_reason"])
                self.assertIn("docs/my guide.md" if case == "path with spaces" else "docs/guide.md", policy["protected"])
        fallback = {
            "computed include": {"README.md": readme.format("x").replace("std::fs::read_to_string(\"x\")", 'include_str!(concat!("../docs/", "guide.md"))'), "pkg/src/root.rs": '#![doc = include_str!("../../README.md")]\n'},
            "documentation path fragments": {"pkg/src/split.rs": 'fn p(n: &str) -> String { format!("{}{}/{n}.md", "../../do", "cs") }\n'},
            "documentation path fragments ": {"pkg/src/head.rs": 'pub const HEAD: &str = "../../doc";\n', "pkg/src/tail.rs": 'fn p() -> String { format!("{}{}", crate::HEAD, "s/guide") }\n'},
            "unresolved documentation reference": {"pkg/src/prefix.rs": 'fn p() -> String { ["../../docs/gui", "de.md"].concat() }\n'},
            "process invocation": {"pkg/build.rs": 'fn main() { std::process::Command::new("python3").arg("../scripts/gen.py").status().unwrap(); }\n'},
            "computed directory traversal": {"pkg/build.rs": 'fn main() { std::process::Command::new("../scripts/gen.py").status().unwrap(); }\n', "scripts/gen.py": "#!/usr/bin/env python3\nfrom pathlib import Path\nprint((Path(__file__).parents[1] / 'docs' / 'guide.md').read_text())\n"},
            "unresolved documentation reference ": {".gitignore": "/pkg/src/generated.rs\n", "pkg/src/generated.rs": 'fn p(n: &str) -> String { format!("../../docs/{n}.md") }\n'},
        }
        for reason, files in fallback.items():
            with self.subTest(reason=reason):
                self.assertIn(reason.strip(), self.scenario(files, ["scripts/gen.py"] if "scripts/gen.py" in files else ())["fallback_reason"] or "")

    def test_unreached_files_and_data_do_not_force_fallback(self):
        policy = self.scenario({
            "scripts/tool.py": "from pathlib import Path\nprint(Path('docs').glob('*.md'))\n",
            "pkg/tests/fixtures/data.json": '{"scripts": ["d", "s/x"]}\n',
            "pkg/src/letters.rs": 'const L: [&str; 2] = ["d", "x"];\n',
        })
        self.assertIsNone(policy["fallback_reason"])
        self.assertIn("docs/guide.md", policy["excluded"])

    def test_metadata_only_changelog_reference_is_not_a_cargo_input(self):
        self.assertIn("CHANGELOG.md", dev.source_state("test")["policy"]["excluded"])
        self.edit("pkg/Cargo.toml", FIXTURE["pkg/Cargo.toml"] + 'include = ["../CHANGELOG.md"]\n')
        self.assertIn("CHANGELOG.md", dev.source_state("test")["policy"]["protected"])

    def test_execute_receipt_records_policy_and_input_categories(self):
        args = argparse.Namespace(command="test", config="transparent", package=None, profile=None, only=None, filter=None, exact=False)
        for name, expected in (("docs/guide.md", (0, "pass", [], ["documentation-prose"])), ("pkg/src/lib.rs", (3, "invalidated", ["rust-source"], []))):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as build, patch.dict(os.environ, {"WALLET_LIB_BUILD_ROOT": build}), patch.object(dev, "build_identity", return_value="test"), patch.object(dev, "run", side_effect=lambda *a, **k: (self.edit(name, "during run\n"), subprocess.CompletedProcess([], 0))[1]):
                code = dev.execute(args)
                result = json.loads(next(Path(build).glob("**/last-result.json")).read_text())
                self.assertEqual((code, result["status"], result["changed_inputs"], result["ignored_changes"]), expected)
                self.assertEqual(result["source"]["policy"]["version"], dev.POLICY_VERSION)
                self.assertIsNone(result["source"]["policy"]["fallback_reason"])

    def test_repository_audit_has_no_unresolved_references(self):
        with patch.object(dev, "ROOT", Path(__file__).resolve().parents[2]):
            policy = dev.input_policy("test", dev.repository_files())
        self.assertIsNone(policy["fallback_reason"])
        self.assertTrue(policy["excluded"])


if __name__ == "__main__":
    unittest.main()
