#!/usr/bin/env python3
"""Qualify old-writer transaction ingestion against disposable rc5/rc7 wallets.

Separate consumers retain the published dependency families without modifying the workspace
lockfile. Every run exclusively owns its temporary Cargo target; no user wallet is accepted.
"""
import json
from pathlib import Path
import sqlite3
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "librustzcash/zcash_client_backend/tests/fixtures/ironwood-fee-expiry.hex"


def consumer(root, version):
    dest = root / version
    (dest / "src").mkdir(parents=True)
    (dest / "src/main.rs").write_text((ROOT / "scripts/probes/legacy_rollback.rs").read_text())
    manifest = '[package]\nname = "legacy-rollback-probe"\nversion = "0.0.0"\nedition = "2024"\n[workspace]\n[features]\ncurrent = []\n[dependencies]\nhex = "0.4"\n'
    for alias, package, directory in [
        ("zcash_client_sqlite", "zakura-client-sqlite", "zcash_client_sqlite"),
        ("zcash_client_backend", "zakura-client-backend", "zcash_client_backend"),
    ]:
        source = f'path = {json.dumps(str(ROOT / "librustzcash" / directory))}' if version == "current" else f'version = "=0.1.0-{version}"'
        manifest += f'{alias} = {{ package = "{package}", {source}, features = ["orchard", "transparent-inputs", "test-dependencies"] }}\n'
    manifest += 'zcash_primitives = { package = "zakura-primitives", version = "=1.2.0" }\nzcash_protocol = "=0.10.4"\n' if version == "rc5" else 'zcash_primitives = { package = "zakura-primitives", version = "=2.0.0" }\nzcash_protocol = { package = "zakura-protocol", version = "=2.0.0" }\n'
    (dest / "Cargo.toml").write_text(manifest)
    return dest


def snapshot(path):
    with sqlite3.connect(path) as conn:
        return conn.execute("SELECT id FROM schemer_migrations ORDER BY id").fetchall()


def main():
    with tempfile.TemporaryDirectory(prefix="legacy-rollback-") as tmp:
        root = Path(tmp)
        consumers = {v: consumer(root, v) for v in ("current", "rc5", "rc7")}
        def run(version, command, db):
            argv = ["cargo", "run", "--manifest-path", str(consumers[version] / "Cargo.toml"), "--target-dir", str(root / "target")]
            if version == "current":
                argv += ["--features", "current"]
            subprocess.run(argv + ["--", command, str(db), str(FIXTURE)], cwd=ROOT, check=True)
        for version in ("rc5", "rc7"):
            db = root / f"{version}.sqlite"
            run(version, "init", db)
            run("current", "init", db)
            before = snapshot(db)
            run(version, "expect-failure", db)
            run("current", "prepare", db)
            assert snapshot(db) == before
            run(version, "ingest", db)
            with sqlite3.connect(db) as conn:
                old_rows = conn.execute("SELECT txid, raw, min_observed_height, mined_height FROM transactions ORDER BY txid").fetchall()
                assert len(old_rows) == 1
            run("current", "init", db)
            with sqlite3.connect(db) as conn:
                assert conn.execute("SELECT txid, raw, min_observed_height, mined_height FROM transactions ORDER BY txid").fetchall() == old_rows
                assert not any(row[1] == "zip318_kind" for row in conn.execute("PRAGMA table_info(transactions)"))
                assert conn.execute("PRAGMA foreign_key_check").fetchall() == []
                assert conn.execute("SELECT count(*) FROM tpir_coverage").fetchone()[0] == 0
            assert snapshot(db) == before
            print(f"PASS {version}: failed before preparation; ingested twice after preparation; current round trip preserved raw transactions and journal", flush=True)


if __name__ == "__main__":
    main()
