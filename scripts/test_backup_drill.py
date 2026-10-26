#!/usr/bin/env python3
"""Offline fixtures: no actual databases, credentials, providers, or Git changes."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("backup_drill", Path(__file__).with_name("backup-drill.py"))
drill = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(drill)


class BackupDrillTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.directory = drill.safe_directory(self.root)
        self.connection = drill.endpoint("postgresql://fixture:FAKE-SECRET@127.0.0.1:6543/admin")
        self.tools = {name: "/fixture/" + name for name in ("psql", "createdb", "dropdb", "pg_dump", "pg_restore")}
        (self.root / "binary").write_bytes(b"synthetic executable fixture")
        self.executor = drill.Drill(self.connection, self.tools, self.root / "binary", self.directory, {})

    def test_endpoint_accepts_only_explicit_numeric_loopback(self):
        for value, host in (("postgresql://u@127.0.0.1:6543/admin", "127.0.0.1"),
                            ("postgres://u:p@[::1]/admin", "::1"),
                            ("postgres://u@127.23.4.5/admin", "127.23.4.5")):
            self.assertEqual(drill.endpoint(value)["host"], host)
        rejected = ("", None, "postgres://u@localhost/db", "postgres://u@db.example.com/db",
                    "postgres://u@192.168.1.1/db", "postgres://u@0.0.0.0/db", "postgres://u@[::]/db",
                    "postgres://u@127.0.0.1/db?host=remote", "postgres://u@127.0.0.1/db?hostaddr=8.8.8.8",
                    "postgres://u@127.0.0.1/db?options=remote", "postgres://u@127.0.0.1/db?",
                    "postgres://u@127.0.0.1/db#", "postgres://u@127.0.0.1:0/db",
                    "postgres://u@[::1%25en0]/db", "postgres://u@%31%32%37.0.0.1/db",
                    "postgres://u@127.0.0.1/db/other", "postgres://u@127.0.0.1/db%3Fhost=x",
                    "postgres://u@127.0.0.1/db\n", "postgres://u%0A@127.0.0.1/db",
                    "postgres://u:p%00@127.0.0.1/db", "postgres://127.0.0.1/db",
                    "postgres://u@/db", "postgres://u@127.1/db", "postgres://u@2130706433/db")
        for value in rejected:
            with self.subTest(value=value), self.assertRaises(drill.DrillError):
                drill.endpoint(value)

    def test_credentials_only_in_sanitized_environment_not_argv(self):
        with patch.object(drill.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, b"", b"")) as run:
            source = self.executor.create("source")
        args = run.call_args.args[0]
        self.assertNotIn("FAKE-SECRET", " ".join(args))
        self.assertFalse(any("postgresql://" in a for a in args))
        env = run.call_args.kwargs["env"]
        self.assertEqual(env["PGPASSWORD"], "FAKE-SECRET")
        self.assertEqual(env["PGPASSFILE"], os.devnull)
        self.assertEqual(env["PGHOST"], "127.0.0.1")
        self.assertNotIn("DATABASE_URL", env)
        self.assertIn(source, self.executor.created)

    def test_failed_create_is_not_tracked_or_dropped(self):
        failure = subprocess.CompletedProcess([], 1, b"", b"sensitive data")
        with patch.object(drill.subprocess, "run", return_value=failure):
            with self.assertRaises(drill.DrillError):
                self.executor.create("source")
        self.assertEqual(self.executor.created, [])
        with patch.object(drill.subprocess, "run") as run:
            self.assertTrue(self.executor.cleanup())
            run.assert_not_called()

    def test_drop_only_created_current_invocation_databases(self):
        source = self.executor.names["source"]
        foreign = "vitattendance_drill_" + "a" * 32 + "_restore"
        for name in (self.connection["admin"], "postgres", foreign, "vitattendance_drill_bad_source", source):
            with self.assertRaises(drill.DrillError):
                self.executor.assert_owned(name)
        self.executor.created.append(source)
        self.executor.assert_owned(source)
        with patch.object(drill.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, b"", b"")) as run:
            self.assertTrue(self.executor.cleanup())
            self.assertEqual(run.call_count, 1)
            self.assertEqual(run.call_args.args[0][-1], source)
            self.assertIn("dropdb", run.call_args.args[0][0])
            self.assertLessEqual(run.call_args.kwargs["timeout"], 15)
        self.assertEqual(self.executor.created, [])

    def test_cleanup_rejects_foreign_names_even_if_list_corrupted(self):
        self.executor.created.extend(["postgres", "vitattendance_drill_" + "a" * 32 + "_source"])
        with patch.object(drill.subprocess, "run") as run:
            self.assertFalse(self.executor.cleanup())
            run.assert_not_called()

    def test_cleanup_retries_other_owned_database_after_failure(self):
        self.executor.created.extend(self.executor.names.values())
        with patch.object(self.executor, "run", side_effect=[drill.DrillError("failure"), b""]) as run:
            self.assertFalse(self.executor.cleanup())
            self.assertEqual(run.call_count, 2)
        self.assertEqual(len(self.executor.created), 1)

    def test_subprocess_errors_and_timeouts_are_redacted_and_bounded(self):
        for result in (subprocess.CompletedProcess([], 1, b"private rows", b"FAKE-SECRET"),
                       subprocess.TimeoutExpired(["private-url"], 1, output=b"private rows", stderr=b"FAKE-SECRET")):
            options = {"side_effect": result} if isinstance(result, Exception) else {"return_value": result}
            with patch.object(drill.subprocess, "run", **options) as run:
                with self.assertRaises(drill.DrillError) as raised:
                    self.executor.run(["fixture"])
                self.assertNotIn("FAKE-SECRET", str(raised.exception))
                self.assertNotIn("private", str(raised.exception))
                self.assertLessEqual(run.call_args.kwargs["timeout"], 60)
        self.executor.deadline = 0
        with patch.object(drill.subprocess, "run") as run:
            with self.assertRaises(drill.DrillError):
                self.executor.run(["fixture"])
            run.assert_not_called()

    def test_archive_and_report_permissions_and_symlink_refusal(self):
        archive = self.directory / "fixture.dump"
        with drill.private_file(archive) as output:
            output.write(b"synthetic")
        self.assertEqual(stat.S_IMODE(archive.stat().st_mode), 0o600)
        with self.assertRaises(FileExistsError):
            drill.private_file(archive)
        report = drill.new_report()
        drill.write_report(self.directory, report)
        output = self.directory / "backup-drill.json"
        self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o600)
        self.assertEqual(json.loads(output.read_text()), report)
        output.chmod(0o644)
        drill.write_report(self.directory, report)
        self.assertEqual(stat.S_IMODE(output.stat().st_mode), 0o600)
        output.unlink()
        output.symlink_to(archive)
        with self.assertRaises(drill.DrillError):
            drill.write_report(self.directory, report)
        self.assertEqual(archive.read_bytes(), b"synthetic")

    def test_output_directory_rejects_symlinks(self):
        (self.directory / "unused").touch()
        with tempfile.TemporaryDirectory() as parent:
            root = Path(parent)
            (root / ".tools").symlink_to(self.directory, target_is_directory=True)
            with self.assertRaises(drill.DrillError):
                drill.safe_directory(root)

    def test_binary_is_explicit_workspace_release_executable(self):
        path = self.root / "target/release/server"
        path.parent.mkdir(parents=True)
        path.write_bytes(b"fixture")
        path.chmod(0o700)
        self.assertEqual(drill.release_binary("target/release/server", self.root), path)
        for value in ("/bin/sh", "target/debug/server", "target/release/../../outside"):
            with self.assertRaises(drill.DrillError):
                drill.release_binary(value, self.root)
        link = path.with_name("link")
        link.symlink_to(path)
        with self.assertRaises(drill.DrillError):
            drill.release_binary(link, self.root)

    def test_report_failure_redaction_and_no_database_url_fallback(self):
        with patch.dict(os.environ, {"DATABASE_URL": "postgresql://FAKE-SECRET@remote/db"}, clear=True), \
                patch.object(sys, "argv", ["backup-drill.py"]), \
                patch.object(drill, "safe_directory") as directory, \
                contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertEqual(drill.main(), 1)
            directory.assert_not_called()
        report = json.loads(output.getvalue())
        self.assertEqual(report["failures"], ["invalid_explicit_numeric_loopback_admin_endpoint"])
        self.assertNotIn("FAKE-SECRET", output.getvalue())
        self.assertNotIn("remote", output.getvalue())
        self.assertTrue(report["synthetic"])
        self.assertFalse(report["key_decryption_tested"])
        self.assertFalse(report["local_supabase_tested"])
        self.assertFalse(report["production_backup_tested"])

    def test_security_expectations_fail_closed(self):
        metadata = self.security_fixture()
        self.assertTrue(drill.security_pass(metadata))
        changes = (
            lambda m: m["tables"][0].update(rls=False),
            lambda m: m.update(revokes_pass=False),
            lambda m: m.update(effective_api_privileges_pass=False),
            lambda m: m["functions"][0].update(security_definer=False),
            lambda m: m["functions"][0].update(config=["search_path=public"]),
            lambda m: m["triggers"][0].update(enabled="D"),
        )
        for change in changes:
            altered = self.security_fixture()
            change(altered)
            self.assertFalse(drill.security_pass(altered))

    @staticmethod
    def security_fixture():
        return {"tables": [{"name": name, "rls": True} for name in sorted(drill.PRIVATE_TABLES)],
                "functions": [{"name": "lock_account_write", "security_definer": True,
                               "config": ["search_path=pg_catalog, app_private"]},
                              {"name": "guard_account_write"}],
                "triggers": [{"name": name + "_account_guard", "table": name, "enabled": "O"}
                             for name in ("student_plans", "auth_sessions", "ai_usage")],
                "revokes_pass": True, "effective_api_privileges_pass": True}

    def test_archive_keeps_acls_restore_is_empty_database_and_report_has_no_rows(self):
        fingerprints = {"auth.users": {"count": 2, "sha256": "0" * 64}}
        metadata = self.security_fixture()
        metadata["private_fixture_sentinel"] = "NEVER-EMIT-RAW-METADATA"
        commands = []

        def run(args, **kwargs):
            commands.append([str(a) for a in args])
            if kwargs.get("output") is not None:
                kwargs["output"].write(b"synthetic archive")
            return b""

        with patch.object(self.executor, "run", side_effect=run), \
                patch.object(self.executor, "migrate"), \
                patch.object(self.executor, "seed", return_value=("active", "pending")), \
                patch.object(self.executor, "snapshot", return_value=(fingerprints, metadata)), \
                patch.object(self.executor, "pending_guard", return_value=True):
            report = drill.new_report()
            self.executor.execute(report)
        dumped = next(c for c in commands if c[0].endswith("pg_dump"))
        restored = next(c for c in commands if c[0].endswith("pg_restore"))
        self.assertIn("--format=custom", dumped)
        self.assertNotIn("--no-acl", dumped + restored)
        self.assertNotIn("--no-privileges", dumped + restored)
        self.assertIn("--no-owner", restored)
        self.assertNotIn("--clean", restored)
        self.assertEqual(dumped[-1], self.executor.names["source"])
        self.assertIn(self.executor.names["restore"], restored)
        self.assertNotIn(self.connection["admin"], dumped + restored)
        self.assertNotIn("NEVER-EMIT-RAW-METADATA", json.dumps(report))
        self.assertTrue(report["data_pass"])
        self.assertTrue(report["security_pass"])
        self.assertEqual(report["archive"]["mode"], 0o600)

    def test_fingerprints_are_ordered_and_storage_json_compatible(self):
        self.assertEqual(drill.digest({"b": 2, "a": 1}), drill.digest({"a": 1, "b": 2}))
        self.assertNotEqual(drill.digest([1, 2]), drill.digest([2, 1]))
        self.assertEqual(json.loads(drill.canonical(drill.PLAN)), drill.PLAN)


if __name__ == "__main__":
    unittest.main()
