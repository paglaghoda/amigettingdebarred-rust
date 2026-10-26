#!/usr/bin/env python3
"""Synthetic native-loopback backup drill. Never backs up or drops an existing DB."""
import argparse
import base64
from datetime import datetime, timezone
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import stat
import subprocess
import sys
import time
from urllib.parse import quote, unquote, urlsplit
import uuid

from local import ROOT, child_env

PREFIX = "vitattendance_drill_"
NAME = re.compile(r"vitattendance_drill_[0-9a-f]{32}_(source|restore)\Z")
PRIVATE_TABLES = {"student_plans", "auth_sessions", "oauth_flows", "ai_usage",
                  "account_deletion_requests", "migration_batches", "migration_rows"}
# Standalone synthetic fixture: no private input file, serializer or domain engine required.
PLAN = {
    "schema_version": 1, "semester_id": "winter-2026", "calendar_revision": 1,
    "timetable": {"courses": [{"code": "CSE1001", "name": "Programming"},
                              {"code": "MAT1001", "name": "Mathematics"}],
                  "grid": {"Mon": ["CSE1001", "MAT1001", None, None, None, None, None],
                           "Tue": ["CSE1001", None, None, None, None, None, None],
                           "Wed": ["MAT1001", None, None, None, None, None, None],
                           "Thu": ["CSE1001", None, None, None, None, None, None],
                           "Fri": ["CSE1001", "MAT1001", None, None, None, None, None]}},
    "attendance": {"as_of": "2026-11-20", "attended": {"CSE1001": 4, "MAT1001": 3}},
    "leave": [],
}


class DrillError(Exception):
    """Only fixed, nonsecret reason identifiers may leave the executor."""


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


def digest(value):
    return hashlib.sha256(canonical(value)).hexdigest()


def endpoint(value):
    try:
        if not isinstance(value, str) or any(ord(c) <= 32 or ord(c) == 127 for c in value):
            raise ValueError
        parsed = urlsplit(value)
        host = parsed.hostname
        if (parsed.scheme not in ("postgres", "postgresql") or not host or "%" in host
                or not ipaddress.ip_address(host).is_loopback
                or "?" in value or "#" in value or not parsed.username
                or not re.fullmatch(r"/[A-Za-z0-9_-]+", parsed.path)
                or parsed.port == 0):
            raise ValueError
        user = unquote(parsed.username, errors="strict")
        password = unquote(parsed.password or "", errors="strict")
        if any(ord(c) < 32 or ord(c) == 127 for c in user + password) or not user:
            raise ValueError
        # Never retain or report the raw URI. Libpq receives discrete fields only.
        return {"host": str(ipaddress.ip_address(host)), "port": str(parsed.port or 5432),
                "user": user, "password": password, "admin": parsed.path[1:]}
    except (ValueError, TypeError, UnicodeError):
        raise DrillError("invalid_explicit_numeric_loopback_admin_endpoint") from None


def safe_directory(root=ROOT):
    directory = root / ".tools/release"
    for path in (root / ".tools", directory):
        if path.is_symlink():
            raise DrillError("symlinked_output_directory")
        path.mkdir(exist_ok=True, mode=0o700)
    return directory


def private_file(path):
    # Never follow/overwrite a preexisting archive, report, or symlink.
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL
                 | getattr(os, "O_NOFOLLOW", 0), 0o600)
    os.fchmod(fd, 0o600)
    return os.fdopen(fd, "wb")


def write_report(directory, report):
    destination = directory / "backup-drill.json"
    if destination.is_symlink() or (destination.exists() and not destination.is_file()):
        raise DrillError("unsafe_report_destination")
    temporary = directory / ("report_" + uuid.uuid4().hex + ".json")
    try:
        with private_file(temporary) as output:
            output.write(json.dumps(report, indent=2).encode() + b"\n")
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def native_tools(env):
    tools = {}
    for name in ("psql", "createdb", "dropdb", "pg_dump", "pg_restore"):
        path = shutil.which(name, path=env.get("PATH", os.defpath))
        fallback = Path("/opt/homebrew/opt/postgresql@17/bin") / name
        if not path and fallback.is_file() and os.access(fallback, os.X_OK):
            path = str(fallback)
        if not path:
            raise DrillError("native_postgresql_tools_unavailable")
        tools[name] = path
    return tools


def release_binary(value, root=ROOT):
    path = Path(value)
    if not path.is_absolute():
        path = root / path
    try:
        relative = path.relative_to(root / "target/release")
    except ValueError:
        raise DrillError("binary_must_be_workspace_release_executable") from None
    if ".." in relative.parts:
        raise DrillError("binary_must_be_workspace_release_executable")
    current = root
    for part in path.relative_to(root).parts:
        current = current / part
        if current.is_symlink():
            raise DrillError("symlinked_release_binary")
    if not path.is_file() or not os.access(path, os.X_OK):
        raise DrillError("release_binary_unavailable_build_explicitly")
    return path


# pg_restore can materialize an implicit catalog-default ACL explicitly. Expand
# defaults before comparing ACL text; never discard actual grants or revokes.
SECURITY_SQL = """
SELECT jsonb_build_object(
 'tables', (SELECT coalesce(jsonb_agg(jsonb_build_object(
     'name', c.relname, 'rls', c.relrowsecurity, 'force_rls', c.relforcerowsecurity,
     'owner', pg_get_userbyid(c.relowner), 'acl', coalesce(c.relacl,acldefault('r',c.relowner))::text) ORDER BY c.relname), '[]')
     FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
     WHERE n.nspname='app_private' AND c.relkind IN ('r','p')),
 'schemas', (SELECT jsonb_agg(jsonb_build_object('name', nspname,
     'owner', pg_get_userbyid(nspowner), 'acl', coalesce(nspacl,acldefault('n',nspowner))::text) ORDER BY nspname)
     FROM pg_namespace WHERE nspname IN ('app_private','auth','public')),
 'functions', (SELECT coalesce(jsonb_agg(jsonb_build_object('name', p.proname,
     'arguments', pg_get_function_identity_arguments(p.oid), 'security_definer', p.prosecdef,
     'config', p.proconfig, 'acl', p.proacl::text, 'owner', pg_get_userbyid(p.proowner),
     'definition', pg_get_functiondef(p.oid)) ORDER BY p.proname, p.oid::regprocedure::text), '[]')
     FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='app_private'),
 'triggers', (SELECT coalesce(jsonb_agg(jsonb_build_object('table', c.relname,
     'name', t.tgname, 'enabled', t.tgenabled, 'definition', pg_get_triggerdef(t.oid))
     ORDER BY c.relname,t.tgname), '[]') FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid
     JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='app_private' AND NOT t.tgisinternal),
 'default_acls', (SELECT coalesce(jsonb_agg(jsonb_build_object('role', pg_get_userbyid(d.defaclrole),
     'schema', n.nspname, 'type', d.defaclobjtype, 'acl', d.defaclacl::text)
     ORDER BY pg_get_userbyid(d.defaclrole),n.nspname,d.defaclobjtype), '[]')
     FROM pg_default_acl d JOIN pg_namespace n ON n.oid=d.defaclnamespace WHERE n.nspname='app_private'),
 'api_roles', (SELECT coalesce(jsonb_agg(jsonb_build_object('name', rolname,
     'superuser', rolsuper, 'bypass_rls', rolbypassrls) ORDER BY rolname), '[]')
     FROM pg_roles WHERE rolname IN ('anon','authenticated')),
 'revokes_pass', NOT EXISTS (
     SELECT 1 FROM pg_namespace n, LATERAL aclexplode(coalesce(n.nspacl,acldefault('n',n.nspowner))) a
     WHERE n.nspname='app_private' AND a.grantee IN
        (SELECT oid FROM pg_roles WHERE rolname IN ('anon','authenticated') UNION ALL SELECT 0::oid)
     UNION ALL
     SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace,
        LATERAL aclexplode(coalesce(c.relacl,acldefault('r',c.relowner))) a
     WHERE n.nspname='app_private' AND c.relkind IN ('r','p') AND a.grantee IN
        (SELECT oid FROM pg_roles WHERE rolname IN ('anon','authenticated') UNION ALL SELECT 0::oid)
     UNION ALL
     SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace,
        LATERAL aclexplode(coalesce(p.proacl,acldefault('f',p.proowner))) a
     WHERE n.nspname='app_private' AND a.grantee IN
        (SELECT oid FROM pg_roles WHERE rolname IN ('anon','authenticated') UNION ALL SELECT 0::oid)
     UNION ALL
     SELECT 1 FROM pg_default_acl d JOIN pg_namespace n ON n.oid=d.defaclnamespace,
        LATERAL aclexplode(d.defaclacl) a WHERE n.nspname='app_private' AND a.grantee IN
        (SELECT oid FROM pg_roles WHERE rolname IN ('anon','authenticated') UNION ALL SELECT 0::oid)),
 'effective_api_privileges_pass', NOT EXISTS (
     SELECT 1 FROM pg_roles r WHERE r.rolname IN ('anon','authenticated') AND (
       r.rolsuper OR r.rolbypassrls OR
       has_schema_privilege(r.oid,'app_private','USAGE,CREATE') OR EXISTS (
         SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
         WHERE n.nspname='app_private' AND c.relkind IN ('r','p')
           AND has_table_privilege(r.oid,c.oid,'SELECT,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER')) OR EXISTS (
         SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
         WHERE n.nspname='app_private' AND has_function_privilege(r.oid,p.oid,'EXECUTE'))))
);
"""


def security_pass(metadata):
    tables = {t["name"]: t for t in metadata["tables"]}
    functions = {f["name"]: f for f in metadata["functions"]}
    guards = {t["table"]: t for t in metadata["triggers"] if t["name"].endswith("_account_guard")}
    return (PRIVATE_TABLES <= tables.keys() and all(t["rls"] for t in tables.values())
            and metadata["revokes_pass"] is True and metadata["effective_api_privileges_pass"] is True
            and functions.get("lock_account_write", {}).get("security_definer") is True
            and functions.get("lock_account_write", {}).get("config") == ["search_path=pg_catalog, app_private"]
            and "guard_account_write" in functions
            and all(guards.get(name, {}).get("enabled") == "O"
                    for name in ("student_plans", "auth_sessions", "ai_usage")))


class Drill:
    def __init__(self, connection, tools, binary, directory, env):
        self.connection = connection
        self.tools = tools
        self.binary = binary
        self.directory = directory
        self.env = env.copy()
        self.env.update(PGHOST=connection["host"], PGPORT=connection["port"],
                        PGUSER=connection["user"], PGPASSWORD=connection["password"],
                        PGPASSFILE=os.devnull, PGSERVICEFILE=os.devnull, PGSSLMODE="disable",
                        PGCONNECT_TIMEOUT="5", PGAPPNAME="vitattendance_synthetic_backup_drill",
                        PGOPTIONS="-c statement_timeout=45000 -c lock_timeout=5000 -c timezone=UTC -c search_path=public")
        self.run_id = uuid.uuid4().hex
        self.names = {suffix: PREFIX + self.run_id + "_" + suffix for suffix in ("source", "restore")}
        self.created = []
        self.created_count = 0
        self.deadline = time.monotonic() + 240

    def run(self, args, *, sql=None, output=None, env=None, cleanup=False):
        timeout = 15 if cleanup else min(60, self.deadline - time.monotonic())
        if timeout <= 0:
            raise DrillError("drill_deadline_exceeded")
        try:
            result = subprocess.run([str(a) for a in args], cwd=ROOT, env=self.env if env is None else env,
                                    input=sql.encode() if sql is not None else None,
                                    stdout=output if output is not None else subprocess.PIPE,
                                    stderr=subprocess.PIPE, timeout=timeout, check=False)
        except (OSError, subprocess.TimeoutExpired):
            raise DrillError("subprocess_unavailable_or_timed_out") from None
        if result.returncode:
            raise DrillError("native_command_or_migration_failed")
        return result.stdout

    def sql(self, name, statement):
        self.assert_owned(name)
        data = self.run([self.tools["psql"], "-X", "--no-password", "--quiet", "--tuples-only",
                         "--no-align", "--set=ON_ERROR_STOP=1", "--dbname", name], sql=statement)
        return data.decode().strip()

    def assert_owned(self, name):
        if not NAME.fullmatch(name) or name not in self.names.values() or name not in self.created:
            raise DrillError("refusing_unowned_database_operation")

    def create(self, suffix):
        name = self.names[suffix]
        if not NAME.fullmatch(name) or name in self.created or name == self.connection["admin"]:
            raise DrillError("refusing_database_creation")
        self.run([self.tools["createdb"], "--no-password", "--maintenance-db", self.connection["admin"],
                  "--template", "template0", "--", name])
        # A preexisting name collision fails createdb; it is never added or dropped.
        self.created.append(name)
        self.created_count += 1
        return name

    def cleanup(self):
        failed = 0
        for name in list(reversed(self.created)):
            try:
                self.assert_owned(name)
                self.run([self.tools["dropdb"], "--no-password", "--maintenance-db", self.connection["admin"],
                          "--force", "--", name], cleanup=True)
                self.created.remove(name)
            except DrillError:
                failed += 1
        return failed == 0

    def migrate(self, name):
        self.assert_owned(name)
        self.sql(name, "CREATE SCHEMA auth; CREATE TABLE auth.users (id uuid PRIMARY KEY);")
        c = self.connection
        host = "[" + c["host"] + "]" if ":" in c["host"] else c["host"]
        uri = f"postgresql://{quote(c['user'], safe='')}:{quote(c['password'], safe='')}@{host}:{c['port']}/{name}"
        env = self.env.copy()
        env.update(DATABASE_URL=uri, PUBLIC_ORIGIN="http://127.0.0.1:3000", BIND_ADDRESS="127.0.0.1:0",
                   SUPABASE_URL="http://127.0.0.1:1", SUPABASE_ANON_KEY="synthetic-unused-key",
                   SESSION_ENCRYPTION_KEY=base64.b64encode(secrets.token_bytes(32)).decode(),
                   LOCAL_AUTH_ENABLED="false", AI_API_KEY="", RUST_LOG="off")
        self.run([self.binary, "--migrate"], env=env)
        ledger = json.loads(self.sql(name, "SELECT jsonb_agg(jsonb_build_object('version',version,'success',success,'checksum',encode(checksum,'hex')) ORDER BY version) FROM public._sqlx_migrations;"))
        expected = {int(p.name.split("_", 1)[0]): hashlib.sha384(p.read_bytes()).hexdigest()
                    for p in (ROOT / "crates/server/migrations").glob("[0-9]*_*.sql")}
        if (len(ledger) != len(expected) or not {1, 2, 3} <= expected.keys()
                or any(not row["success"] or expected.get(row["version"]) != row["checksum"] for row in ledger)):
            raise DrillError("embedded_migrations_stale_or_incomplete")

    def seed(self, name):
        active, pending, batch = (str(uuid.uuid4()) for _ in range(3))
        plan = canonical(PLAN).decode().replace("'", "''")
        plan_hash = digest(PLAN)
        marker = hashlib.sha256(b"synthetic-backup-drill-not-an-import").hexdigest()
        # These bytea values are deliberately NOT valid encrypted Auth sessions.
        session, blob, verifier, flow = (secrets.token_hex(32) for _ in range(4))
        self.sql(name, f"""
BEGIN;
INSERT INTO auth.users(id) VALUES ('{active}'), ('{pending}');
INSERT INTO app_private.student_plans(user_id,semester_id,plan,revision)
 VALUES ('{active}','winter-2026','{plan}'::jsonb,1);
INSERT INTO app_private.auth_sessions(token_hash,user_id,encrypted_tokens,access_expires_at,expires_at,authenticated_at)
 VALUES (decode('{session}','hex'),'{active}',decode('{blob}','hex'),now()+interval '1 hour',now()+interval '1 hour',now());
INSERT INTO app_private.ai_usage(user_id,usage_day,kind,used) VALUES ('{active}','2026-11-20','helper',1);
INSERT INTO app_private.oauth_flows(token_hash,encrypted_verifier,expires_at)
 VALUES (decode('{flow}','hex'),decode('{verifier}','hex'),now()+interval '1 hour');
INSERT INTO app_private.account_deletion_requests(user_id) VALUES ('{pending}');
INSERT INTO app_private.migration_batches(batch_id,report_version,fingerprint,source_sha256,mapping_sha256,row_count,status)
 VALUES ('{batch}',1,'{marker}','{marker}','{marker}',1,'applied');
INSERT INTO app_private.migration_rows(batch_id,user_id,semester_id,inserted_revision,plan_sha256,plan_storage_sha256)
 VALUES ('{batch}','{active}','winter-2026',1,'{plan_hash}','{plan_hash}');
COMMIT;
""")
        # Re-read JSONB to confirm the stored hash, rather than trusting the input serializer.
        stored = json.loads(self.sql(name, "SELECT plan FROM app_private.student_plans;"))
        if digest(stored) != plan_hash:
            raise DrillError("seed_plan_storage_hash_mismatch")
        return active, pending

    def snapshot(self, name):
        tables = json.loads(self.sql(name, """SELECT jsonb_agg(c.relname ORDER BY c.relname)
            FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
            WHERE n.nspname='app_private' AND c.relkind IN ('r','p');"""))
        if not PRIVATE_TABLES <= set(tables):
            raise DrillError("required_private_tables_missing")
        names = [("app_private", table) for table in tables] + [("auth", "users"), ("public", "_sqlx_migrations")]
        fingerprints = {}
        for schema, table in sorted(names):
            if not re.fullmatch(r"[a-z_][a-z0-9_]*", table):
                raise DrillError("unexpected_table_identifier")
            rows = json.loads(self.sql(name, f"SELECT coalesce(jsonb_agg(row ORDER BY row::text),'[]') FROM (SELECT to_jsonb(t) AS row FROM {schema}.{table} t) s;"))
            fingerprints[schema + "." + table] = {"count": len(rows), "sha256": digest(rows)}
        security = json.loads(self.sql(name, SECURITY_SQL))
        return fingerprints, security

    def pending_guard(self, name, active, pending):
        result = self.sql(name, f"""
SELECT app_private.lock_account_write('{active}');
DO $drill$
DECLARE rejected boolean := false;
BEGIN
  BEGIN
    INSERT INTO app_private.student_plans(user_id,semester_id,plan,revision)
      VALUES ('{pending}','winter-2026','{{}}'::jsonb,1);
  EXCEPTION WHEN insufficient_privilege THEN
    IF SQLERRM <> 'account identity deletion is pending' THEN RAISE; END IF;
    rejected := true;
  END;
  IF NOT rejected THEN RAISE EXCEPTION 'pending guard not enforced'; END IF;
END
$drill$;
SELECT true;
""")
        return result.endswith("t")

    def execute(self, report):
        report["release_binary_sha256"] = hashlib.sha256(self.binary.read_bytes()).hexdigest()
        source = self.create("source")
        restore = self.create("restore")
        self.migrate(source)
        active, pending = self.seed(source)
        before, source_security = self.snapshot(source)
        report["source"] = before
        report["source_security"] = {"sha256": digest(source_security), "passed": security_pass(source_security)}
        if not report["source_security"]["passed"]:
            raise DrillError("source_security_expectations_failed")
        report["source_pending_guard_pass"] = self.pending_guard(source, active, pending)
        archive = self.directory / ("backup_" + self.run_id + ".dump")
        start = time.monotonic()
        with private_file(archive) as output:
            self.run([self.tools["pg_dump"], "--no-password", "--format=custom", "--dbname", source], output=output)
        report["dump_seconds"] = round(time.monotonic() - start, 6)
        report["archive"] = {"bytes": archive.stat().st_size, "sha256": hashlib.sha256(archive.read_bytes()).hexdigest(),
                             "mode": stat.S_IMODE(archive.stat().st_mode), "acl_suppression_used": False}
        start = time.monotonic()
        self.run([self.tools["pg_restore"], "--no-password", "--no-owner", "--exit-on-error", "--single-transaction",
                  "--dbname", restore, archive])
        report["restore_seconds"] = round(time.monotonic() - start, 6)
        after, restored_security = self.snapshot(restore)
        report["restored"] = after
        report["restored_security"] = {"sha256": digest(restored_security), "passed": security_pass(restored_security)}
        report["data_pass"] = before == after
        report["security_mismatch_components"] = [key for key in sorted(source_security)
                                                  if source_security[key] != restored_security.get(key)]
        report["security_pass"] = source_security == restored_security and security_pass(restored_security)
        report["pending_guard_pass"] = self.pending_guard(restore, active, pending)
        # Verify the guard probes had no data side effects in either database.
        report["guard_probes_unchanged"] = self.snapshot(source)[0] == before and self.snapshot(restore)[0] == after
        if not all(report[k] for k in ("data_pass", "security_pass", "pending_guard_pass", "source_pending_guard_pass", "guard_probes_unchanged")):
            raise DrillError("restored_integrity_security_or_guard_mismatch")


def new_report():
    return {"version": 1, "synthetic": True, "status": "FAILED", "measured_at": datetime.now(timezone.utc).isoformat(),
            "data_pass": False, "security_pass": False, "pending_guard_pass": False,
            "cleanup_pass": False, "key_decryption_tested": False, "local_supabase_tested": False,
            "production_backup_tested": False, "opaque_blobs": "synthetic_byte_integrity_only_not_auth_ciphertext",
            "failures": []}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/debarred-server",
                        help="Existing workspace target/release executable; never built automatically")
    args = parser.parse_args()
    report = new_report()
    drill = None
    directory = None
    start = time.monotonic()
    try:
        connection = endpoint(os.environ.get("DRILL_ADMIN_DATABASE_URL", ""))
        directory = safe_directory()
        env = child_env()
        drill = Drill(connection, native_tools(env), release_binary(args.binary), directory, env)
        drill.execute(report)
        report["status"] = "PASSED"
    except DrillError as error:
        report["failures"].append(str(error))
    except (OSError, ValueError, KeyError, TypeError):
        report["failures"].append("local_drill_operation_failed_details_suppressed")
    except KeyboardInterrupt:
        report["failures"].append("interrupted")
    finally:
        if drill is not None:
            report["cleanup_pass"] = drill.cleanup()
            report["databases_created_count"] = drill.created_count
            report["databases_remaining_count"] = len(drill.created)
            if not report["cleanup_pass"]:
                report["status"] = "FAILED"
                report["failures"].append("owned_database_cleanup_failed")
        report["elapsed_seconds"] = round(time.monotonic() - start, 6)
        if directory is not None:
            try:
                write_report(directory, report)
            except (OSError, DrillError):
                report["status"] = "FAILED"
                report["failures"].append("private_report_write_failed")
    print(json.dumps(report, indent=2))
    return 0 if report["status"] == "PASSED" else 1


if __name__ == "__main__":
    sys.exit(main())
