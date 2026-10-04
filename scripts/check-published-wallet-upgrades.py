#!/usr/bin/env python3
"""Verify upgrades from disposable published rc5/rc7 wallets.

Published consumers ingest transaction/UTXO fixtures before the current library upgrades them.
The probe verifies preserved data, classification values, legacy provenance and repeat initialization.
Separate consumers retain published dependency families; no user wallet is accepted.
"""
import argparse
import fcntl
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
    (dest / "src/main.rs").write_text((ROOT / "scripts/probes/published_wallet_upgrades.rs").read_text())
    manifest = f'[package]\nname = "legacy-writers-probe-{version}"\nversion = "0.0.0"\nedition = "2024"\n[workspace]\n[features]\ncurrent = []\n[dependencies]\nhex = "0.4"\nsecrecy = "0.8"\n'
    for alias, package, directory in [
        ("zcash_client_sqlite", "zakura-client-sqlite", "zcash_client_sqlite"),
        ("zcash_client_backend", "zakura-client-backend", "zcash_client_backend"),
    ]:
        source = f'path = {json.dumps(str(ROOT / "librustzcash" / directory))}' if version == "current" else f'version = "=0.1.0-{version}"'
        manifest += f'{alias} = {{ package = "{package}", {source}, features = ["orchard", "transparent-inputs", "test-dependencies"] }}\n'
    manifest += 'zcash_primitives = { package = "zakura-primitives", version = "=1.2.0" }\nzcash_protocol = "=0.10.4"\n' if version == "rc5" else 'zcash_primitives = { package = "zakura-primitives", version = "=2.0.0" }\nzcash_protocol = { package = "zakura-protocol", version = "=2.0.0" }\n'
    manifest += 'transparent = { package = "zcash_transparent", version = "=0.10.0" }\n' if version == "rc5" else 'transparent = { package = "zakura-transparent", version = "=2.0.0" }\n'
    # Pin the PCZT prerelease used by Vizor: caret prerelease resolution otherwise selects
    # rc4's newer dependency family while testing rc5's published writer.
    if version == "rc5":
        manifest += 'pczt = { package = "zakura-pczt", version = "=0.1.0-rc3", default-features = false, features = ["io-finalizer"] }\n'
    elif version == "rc7":
        manifest += 'pczt = { package = "zakura-pczt", version = "=0.1.0-rc4", features = ["io-finalizer"] }\n'
    (dest / "Cargo.toml").write_text(manifest)
    # Retain this repository's locked common dependency versions instead of floating to a
    # newer minor release. Cargo adjusts only the legacy families absent from this lockfile.
    (dest / "Cargo.lock").write_text((ROOT / "Cargo.lock").read_text())
    return dest


def has_column(conn, relation, column):
    return any(row[1] == column for row in conn.execute(f"PRAGMA table_info({relation})"))


def snapshot(path):
    with sqlite3.connect(path) as conn:
        return conn.execute("SELECT id FROM schemer_migrations ORDER BY id").fetchall()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-dir", type=Path, default=Path.home() / ".cache/wallet-libraries/legacy-writers")
    args = parser.parse_args()
    args.target_dir.mkdir(parents=True, exist_ok=True)
    lease = (args.target_dir / "owner.lock").open("w")
    fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
    with tempfile.TemporaryDirectory(prefix="legacy-writers-") as tmp:
        root = Path(tmp)
        consumers = {v: consumer(root, v) for v in ("current", "rc5", "rc7")}
        def run(version, command, db):
            argv = ["cargo", "run", "--manifest-path", str(consumers[version] / "Cargo.toml"), "--target-dir", str(args.target_dir)]
            if version == "current":
                argv += ["--features", "current"]
            subprocess.run(argv + ["--", command, str(db), str(FIXTURE)], cwd=ROOT, check=True)
        for version in ("rc5", "rc7"):
            db = root / f"{version}.sqlite"
            run(version, "init", db)
            run(version, "ingest", db)
            with sqlite3.connect(db) as conn:
                accounts = conn.execute("SELECT * FROM accounts ORDER BY id").fetchall()
                old_rows = conn.execute("SELECT txid, raw, min_observed_height, mined_height, zip318_kind FROM transactions ORDER BY txid").fetchall()
                old_outputs = conn.execute("SELECT * FROM transparent_received_outputs ORDER BY id").fetchall()
                assert len(accounts) == 1
                assert len(old_rows) == 2
                assert len(old_outputs) == 2
            run("current", "init", db)
            after_upgrade = snapshot(db)
            run("current", "init", db)
            with sqlite3.connect(db) as conn:
                assert conn.execute("SELECT * FROM accounts ORDER BY id").fetchall() == accounts
                assert conn.execute("SELECT txid, raw, min_observed_height, mined_height, zip318_kind FROM transactions ORDER BY txid").fetchall() == old_rows
                assert conn.execute("SELECT * FROM transparent_received_outputs ORDER BY id").fetchall() == old_outputs
                assert conn.execute("SELECT output_id, origin FROM tpir_output_origins ORDER BY output_id,origin").fetchall() == [(row[0], 0) for row in old_outputs]
                assert has_column(conn, "transactions", "zip318_kind")
                assert has_column(conn, "v_transactions", "zip318_kind")
                assert conn.execute("PRAGMA foreign_key_check").fetchall() == []
                assert conn.execute("SELECT count(*) FROM tpir_coverage").fetchone()[0] == 0
            assert snapshot(db) == after_upgrade
            print(f"PASS {version}: published ingestion before upgrade; current upgrade preserved accounts, transactions, classification values and UTXOs; legacy provenance grants no coverage; repeat initialization preserved journal", flush=True)



if __name__ == "__main__":
    main()
