"""Safety regression tests; no Docker, network, Cargo, or servers required."""
import base64
import contextlib
import io
import json
import os
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch

import local


class EndpointTests(unittest.TestCase):
    def test_local_api_normalized(self):
        for host in ("localhost", "127.0.0.1", "[::1]"):
            self.assertEqual(local.validate_api(f"http://{host}:54321/"), local.API_URL)

    def test_api_rejects_remote_or_ambiguous(self):
        for value in (None, "", "https://project.supabase.co", "http://localhost:3000",
                      "http://localhost.evil:54321", "http://user:secret@localhost:54321",
                      "http://localhost:54321/auth", "http://localhost:54321?host=remote",
                      "http://localhost:54321#x", "http://localhost:bad", "http://[broken"):
            with self.subTest(value=value), self.assertRaises(local.LocalError):
                local.validate_api(value)

    def test_database_forces_numeric_loopback(self):
        for host in ("localhost", "127.0.0.1", "[::1]"):
            self.assertEqual(local.validate_database(f"postgres://postgres:p%40ss@{host}:54322/postgres"),
                             "postgresql://postgres:p%40ss@127.0.0.1:54322/postgres")

    def test_database_rejects_remote_overrides_and_bad_ports(self):
        for value in (None, "", "postgres://u:p@remote:54322/postgres",
                      "postgres://u:p@localhost:5432/postgres", "postgres://u:p@localhost/postgres",
                      "postgres://u:p@localhost:54322/postgres?host=remote",
                      "postgres://u:p@localhost:54322/postgres?hostaddr=8.8.8.8",
                      "postgres://u:p@localhost:54322/postgres?port=5432",
                      "postgres://u:p@localhost:54322/postgres?options=-hremote",
                      "postgres://u:p@localhost:54322/postgres?sslmode=disable",
                      "postgres://u:p@localhost:54322/postgres#fragment",
                      "postgres://u:p@localhost:54322/a/b", "postgres://localhost:54322/postgres"):
            with self.subTest(value=value), self.assertRaises(local.LocalError):
                local.validate_database(value)


class WorkdirTests(unittest.TestCase):
    def test_all_cli_commands_use_isolated_workdir_without_reading_production_env(self):
        payload = json.dumps({"API_URL": local.API_URL,
                              "DB_URL": "postgres://u:p@localhost:54322/postgres",
                              "ANON_KEY": "local-public"})
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "supabase").mkdir()
            config = root / "supabase/config.toml"
            config.write_text('project_id = "vitattendance-local"\n')
            # These are synthetic fixtures, never the real repository's files.
            forbidden = {root / ".env", root / "production.env", root / "supabase/.env"}
            for path in forbidden:
                path.write_text("MUST_NOT_BE_READ=sentinel\n")
            accesses = []
            builtin_open = open
            path_open = io.open

            def guard(opener):
                def checked(path, *args, **kwargs):
                    if not isinstance(path, int):
                        accessed = Path(path).absolute()
                        accesses.append(accessed)
                        if accessed in forbidden:
                            raise AssertionError("Production/dotenv fixture accessed")
                    return opener(path, *args, **kwargs)
                return checked

            workdir = root / ".tools/local/supabase-project"
            for command in ("start", "status", "stop"):
                with self.subTest(command=command), patch.object(local, "ROOT", root), \
                        patch.object(local.sys, "argv", ["local.py", command]), \
                        patch.object(local, "require_stack", return_value="/fake/supabase"), \
                        patch("builtins.open", side_effect=guard(builtin_open)), \
                        patch("io.open", side_effect=guard(path_open)), \
                        patch.object(local.subprocess, "run") as run, \
                        contextlib.redirect_stdout(io.StringIO()):
                    run.return_value.returncode = 0
                    run.return_value.stdout = payload
                    self.assertEqual(local.main(), 0)
                    expected_commands = [command, "status"] if command == "start" else [command]
                    self.assertEqual(run.call_count, len(expected_commands))
                    for call, expected in zip(run.call_args_list, expected_commands):
                        argv = call.args[0]
                        self.assertEqual(argv[:4], ["/fake/supabase", "--workdir", str(workdir), expected])
                        self.assertEqual(call.kwargs["cwd"], workdir)
                        self.assertNotEqual(call.kwargs["cwd"], root)
                        self.assertEqual(argv[4:], ["--output", "json"] if expected == "status" else [])
                    self.assertEqual((workdir / "supabase/config.toml").read_bytes(), config.read_bytes())
                    self.assertEqual({p.name for p in workdir.iterdir()}, {"supabase"})
                    self.assertEqual({p.name for p in (workdir / "supabase").iterdir()}, {"config.toml"})
            self.assertTrue(forbidden.isdisjoint(accesses))

    def test_refuses_dotenv_or_symlink_in_generated_workdir(self):
        for unsafe in (".env", "supabase/.env", "production.env", "supabase/production.env", "symlink"):
            with self.subTest(unsafe=unsafe), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                workdir = root / ".tools/local/supabase-project"
                (workdir / "supabase").mkdir(parents=True)
                path = workdir / unsafe
                if unsafe == "symlink":
                    path.symlink_to(root / "production.env")
                else:
                    path.write_text("MUST_NOT_BE_READ=sentinel\n")
                with patch.object(local, "ROOT", root), patch.object(local.subprocess, "run") as run:
                    with self.assertRaises(local.LocalError):
                        local.supabase_command("supabase", {}, "status")
                run.assert_not_called()


class EnvironmentTests(unittest.TestCase):
    def test_environment_drops_remote_credentials_and_proxy(self):
        inherited = {"PATH": os.defpath, "HOME": "/tmp", "DATABASE_URL": "remote",
                     "SUPABASE_URL": "https://remote", "SUPABASE_SERVICE_ROLE_KEY": "secret",
                     "SESSION_ENCRYPTION_KEY": "secret", "AI_API_KEY": "secret",
                     "AI_BASE_URL": "https://remote", "AI_MODEL": "remote",
                     "HTTP_PROXY": "https://remote", "HTTPS_PROXY": "https://remote",
                     "ALL_PROXY": "https://remote", "PGHOST": "remote",
                     "TRUSTED_PROXY_IPS": "0.0.0.0", "LOCAL_AUTH_ENABLED": "true",
                     "RUSTFLAGS": "untrusted", "SUPABASE_ACCESS_TOKEN": "secret"}
        with patch.dict(os.environ, inherited, clear=True):
            env = local.app_env(local.child_env())
        for name in ("DATABASE_URL", "SUPABASE_URL", "SUPABASE_SERVICE_ROLE_KEY",
                     "SESSION_ENCRYPTION_KEY", "AI_BASE_URL", "AI_MODEL", "HTTP_PROXY",
                     "HTTPS_PROXY", "ALL_PROXY", "PGHOST", "RUSTFLAGS", "SUPABASE_ACCESS_TOKEN"):
            self.assertNotIn(name, env)
        self.assertEqual(env["AI_API_KEY"], "")
        self.assertEqual(env["LOCAL_AUTH_ENABLED"], "false")
        self.assertEqual(env["BIND_ADDRESS"], "127.0.0.1:3000")
        self.assertEqual(env["TRUSTED_PROXY_IPS"], "")

    def test_persistent_private_session_key(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(local, "ROOT", Path(directory)):
            first = local.session_key()
            self.assertEqual(len(base64.b64decode(first)), 32)
            self.assertEqual(local.session_key(), first)
            path = Path(directory) / ".tools/local/session.key"
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o600)
            path.chmod(0o644)
            with self.assertRaises(local.LocalError):
                local.session_key()
            self.assertEqual(path.read_text().strip(), first)

    def test_status_uses_public_key_not_service_key_and_prints_nothing(self):
        payload = {"API_URL": local.API_URL, "DB_URL": "postgres://u:p@localhost:54322/postgres",
                   "PUBLISHABLE_KEY": "public-test", "SERVICE_ROLE_KEY": "never-use"}
        output = io.StringIO()
        with patch.object(local, "supabase_command", return_value=json.dumps(payload)), contextlib.redirect_stdout(output):
            status = local.local_status("supabase", {})
        self.assertEqual(status[2], "public-test")
        self.assertEqual(output.getvalue(), "")

    def test_cli_errors_do_not_expose_output(self):
        result = type("Result", (), {"returncode": 1, "stdout": "secret", "stderr": "password"})()
        with patch.object(local.subprocess, "run", return_value=result):
            with self.assertRaises(local.LocalError) as raised:
                local.captured(["supabase", "status"], {}, "Local status")
        self.assertNotIn("secret", str(raised.exception))
        self.assertNotIn("password", str(raised.exception))

    def test_preview_does_not_require_stack(self):
        with patch.object(local.sys, "argv", ["local.py", "preview"]), \
                patch.object(local, "require_stack") as stack, \
                patch.object(local, "cargo_command") as cargo:
            self.assertEqual(local.main(), 0)
        stack.assert_not_called()
        env, flag = cargo.call_args.args
        self.assertEqual(flag, "--preview")
        self.assertNotIn("DATABASE_URL", env)
        self.assertNotIn("SUPABASE_URL", env)

    def test_run_revalidates_database_before_key_creation(self):
        with patch.object(local, "session_key") as key:
            with self.assertRaises(local.LocalError):
                local.app_env({}, (local.API_URL, "postgres://u:p@remote:54322/postgres", "public"))
        key.assert_not_called()


if __name__ == "__main__":
    unittest.main()
