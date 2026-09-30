import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("ci", Path(__file__).parents[1] / "ci.py")
ci = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ci)


class SelectionTests(unittest.TestCase):
    def test_guidance_only_is_lightweight(self):
        self.assertFalse(ci.needs_rust(["AGENTS.md", "CLAUDE.md", ".cursor/rules/development.mdc", "docs/development.md"]))

    def test_source_mixed_and_unknown_paths_run_full_checks(self):
        for name in ["Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml", ".github/workflows/verify.yml", "scripts/dev.py", "zakura/pir-enhance/src/lib.rs", "new-unknown-file", "zakura/pir-enhance/README.md"]:
            self.assertTrue(ci.needs_rust(["docs/development.md", name]), name)

    def test_rust_sources_in_docs_still_require_checks(self):
        self.assertTrue(ci.needs_rust(["docs/example.rs"]))
