#!/usr/bin/env python3
"""Pure, offline release gate fixtures; never mutates the real repository or DB."""
import copy
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import release
import benchmark


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        for name in release.FIXED + (
            "crates/web/dist/assets/app.js", "crates/web/dist/assets/app_bg.wasm",
            "crates/web/dist/assets/app_bg.wasm.gz", "crates/web/dist/assets/font.woff2",
            "crates/server/migrations/0001_test.sql",
        ):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(name.encode())
        self.saved = release.manifest(self.root)
        self.manifest_path = self.root / ".tools/release/manifest.json"
        self.evidence_path = self.root / ".tools/release/evidence.json"
        self.manifest_path.parent.mkdir(parents=True)
        self.manifest_path.write_text(json.dumps(self.saved))
        self.evidence = {
            "version": 1, "artifact_fingerprint": self.saved["fingerprint"],
            "public_origin": release.ORIGIN, "contact_email": "operator@attendance.edu",
            "attestations": {name: {"passed": True, "reviewer": "operator-1",
                                    "reviewed_at": "2026-01-01T00:00:00Z", "reference": "OPS-123"}
                             for name in release.ATTESTATIONS},
        }
        self.write_evidence()

    def write_evidence(self):
        self.evidence_path.write_text(json.dumps(self.evidence))

    def check(self, dirty=False):
        result = subprocess.CompletedProcess([], 0, " M private-filename.env\n" if dirty else "", "")
        with patch.object(release.subprocess, "run", return_value=result) as run:
            report = release.check(self.manifest_path, self.evidence_path, self.root)
            self.assertIn("--porcelain=v1", run.call_args.args[0])
            self.assertNotIn("diff", run.call_args.args[0])
            return report

    def test_deterministic_manifest_includes_only_declared_paths(self):
        (self.root / "production.env").write_text("not an input")
        self.assertEqual(self.saved, release.manifest(self.root))
        names = {i["path"] for i in self.saved["inputs"]}
        self.assertIn("crates/web/dist/assets/app_bg.wasm.gz", names)
        self.assertIn("Cargo.lock", names)
        self.assertNotIn("production.env", names)

    def test_every_input_change_and_asset_add_remove_changes_fingerprint(self):
        for item in self.saved["inputs"]:
            path = self.root / item["path"]
            old = path.read_bytes()
            path.write_bytes(old + b"changed")
            self.assertNotEqual(self.saved["fingerprint"], release.manifest(self.root)["fingerprint"])
            path.write_bytes(old)
        path = self.root / "crates/web/dist/assets/extra.txt"
        path.write_text("extra")
        self.assertNotEqual(self.saved, release.manifest(self.root))
        path.unlink()
        self.assertEqual(self.saved, release.manifest(self.root))
        (self.root / "crates/web/dist/assets/font.woff2").unlink()
        self.assertNotEqual(self.saved, release.manifest(self.root))

    def test_ready_means_hashes_plus_attestations_not_remote_tests(self):
        report = self.check()
        self.assertEqual(report["status"], "READY")
        self.assertFalse(report["deployment_performed"])
        self.assertTrue(all(report["manual_attestations_not_automated_tests"].values()))

    def test_missing_evidence_blocks(self):
        self.evidence_path.unlink()
        self.assertEqual(self.check()["status"], "BLOCKED")

    def test_stale_evidence_and_stale_manifest_block(self):
        path = self.root / release.FIXED[1]
        path.write_text("rebuilt")
        report = self.check()
        self.assertIn("artifact_manifest_stale_or_invalid", report["failures"])
        self.assertIn("manual_pending_artifact_binding", report["failures"])
        self.manifest_path.write_text(json.dumps(release.manifest(self.root)))
        report = self.check()
        self.assertTrue(report["automated"]["artifact_hashes_and_fingerprint"]["passed"])
        self.assertEqual(report["status"], "BLOCKED")

    def test_dirty_tree_blocks_without_disclosing_names(self):
        report = self.check(dirty=True)
        self.assertEqual(report["status"], "BLOCKED")
        self.assertNotIn("private-filename", json.dumps(report))
        self.assertEqual(report["automated"]["clean_git_tree"]["status_counts_names_redacted"], {" M": 1})

    def test_every_manual_attestation_is_mandatory(self):
        for name in release.ATTESTATIONS:
            evidence = copy.deepcopy(self.evidence)
            evidence["attestations"][name]["passed"] = False
            self.assertFalse(release.evidence_checks(evidence, self.saved["fingerprint"])[name])

    def test_origin_contact_and_narrow_schema(self):
        for origin in ("http://vitattendance.rasmalai.tech", release.ORIGIN + "/", "https://other.test"):
            evidence = dict(self.evidence, public_origin=origin)
            self.assertFalse(release.evidence_checks(evidence, self.saved["fingerprint"])["production_origin"])
        for email in ("", "contact@example.com", "dev@host.test", "not-an-email"):
            evidence = dict(self.evidence, contact_email=email)
            self.assertFalse(release.evidence_checks(evidence, self.saved["fingerprint"])["contact_email_supplied"])
        evidence = dict(self.evidence, unexpected="not allowed")
        self.assertEqual(release.evidence_checks(evidence, self.saved["fingerprint"]), {"evidence_schema": False})
        self.assertFalse(release.valid_attestation({"passed": True}))
        bad = copy.deepcopy(self.evidence["attestations"][release.ATTESTATIONS[0]])
        bad["reference"] = "https://private.example/secret"
        self.assertFalse(release.valid_attestation(bad))

    def test_operator_paths_are_explicit_tools_json_only(self):
        for name in ("production.env", "../outside.json", ".tools/../../outside.json"):
            with self.assertRaises(release.GateError):
                release.operator_path(name, self.root)
        link = self.root / ".tools/link.json"
        link.symlink_to(self.evidence_path)
        with self.assertRaises(release.GateError):
            release.operator_path(link, self.root)
        self.assertEqual(release.operator_path(".tools/release/evidence.json", self.root), self.evidence_path)

    def test_symlink_and_private_asset_rejected(self):
        path = self.root / "crates/web/dist/assets/link"
        path.symlink_to(self.root / "Cargo.lock")
        with self.assertRaises(release.GateError):
            release.manifest(self.root)
        path.unlink()
        path = path.with_name("production.env")
        path.write_text("private")
        with self.assertRaises(release.GateError):
            release.manifest(self.root)

    def test_git_unavailable_blocks(self):
        with patch.object(release.subprocess, "run", side_effect=OSError):
            self.assertFalse(release.git_status(self.root)["passed"])

    def test_numeric_loopback_benchmark_validation(self):
        for value in ("postgresql://user@127.0.0.1:6543/bench", "postgres://user@[::1]/bench"):
            self.assertEqual(benchmark.validate_database(value), value)
        for value in ("", "postgres://u@localhost/b", "postgres://u@db.example.com/b",
                      "postgres://u@127.0.0.1/b?host=remote", "postgres://u@127.0.0.1/b?sslmode=disable",
                      "postgres://u@127.0.0.1/b#remote", "postgres://u@127.0.0.1:0/b"):
            with self.assertRaises(benchmark.LocalError):
                benchmark.validate_database(value)


if __name__ == "__main__":
    unittest.main()
