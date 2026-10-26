#!/usr/bin/env python3
"""Offline artifact integrity gate plus explicit manual attestations. Never deploys."""
import argparse
from datetime import datetime
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys

from local import ROOT, child_env

ORIGIN = "https://vitattendance.rasmalai.tech"
ATTESTATIONS = (
    "official_calendar_review", "live_google_non_test_login", "real_supabase_auth_journey",
    "backup_restore_drill", "identity_migration_review", "identity_cutover_plan",
    "rollback_review", "account_deletion_process", "local_supabase_stack_validation",
)
FIXED = ("Cargo.lock", "target/release/debarred-server", "crates/domain/data/winter-2026.v1.json")
TREES = ("crates/web/dist", "crates/server/migrations")


class GateError(Exception):
    pass


def regular(root, path):
    relative = path.relative_to(root)
    current = root
    for part in relative.parts:
        current = current / part
        if current.is_symlink():
            raise GateError("symlink_in_declared_artifacts")
    if not path.is_file():
        raise GateError("missing_declared_artifact")
    # Public dist must never contain dotenv/private files, and we must not read them.
    if any(part.startswith(".") or part.endswith(".env") for part in relative.parts):
        raise GateError("unexpected_private_artifact_name")


def manifest(root=ROOT):
    paths = [root / name for name in FIXED]
    for name in TREES:
        directory = root / name
        if not directory.is_dir() or directory.is_symlink():
            raise GateError("missing_or_symlinked_artifact_directory")
        entries = sorted(directory.rglob("*"))
        if any(p.is_symlink() for p in entries):
            raise GateError("symlink_in_declared_artifacts")
        files = [p for p in entries if p.is_file()]
        if not files:
            raise GateError("empty_artifact_directory")
        paths.extend(files)
    names = {p.relative_to(root).as_posix() for p in paths}
    if not {"crates/web/dist/assets/app.js", "crates/web/dist/assets/app_bg.wasm"} <= names:
        raise GateError("missing_web_build")
    if not any(n.startswith("crates/server/migrations/") and n.endswith(".sql") for n in names):
        raise GateError("missing_migrations")
    inputs = []
    for path in sorted(paths):
        regular(root, path)
        data = path.read_bytes()
        inputs.append({"path": path.relative_to(root).as_posix(), "bytes": len(data),
                       "sha256": hashlib.sha256(data).hexdigest()})
    canonical = json.dumps(inputs, sort_keys=True, separators=(",", ":")).encode()
    return {"version": 1, "algorithm": "sha256", "fingerprint": hashlib.sha256(canonical).hexdigest(),
            "inputs": inputs}


def operator_path(value, root=ROOT):
    path = Path(value)
    if not path.is_absolute():
        path = root / path
    # Resolve containment but also reject every symlink, even one pointing inside.
    try:
        relative = path.absolute().relative_to(root / ".tools")
    except ValueError:
        raise GateError("operator_json_must_be_under_tools") from None
    if ".." in relative.parts or path.suffix != ".json":
        raise GateError("operator_json_must_be_under_tools")
    current = root
    for part in path.relative_to(root).parts:
        current = current / part
        if current.is_symlink():
            raise GateError("symlinked_operator_path")
    return path


def read_json(path):
    try:
        if path.stat().st_size > 1024 * 1024:
            raise GateError("operator_json_too_large")
        return json.loads(path.read_text())
    except (OSError, ValueError):
        raise GateError("missing_or_invalid_operator_json") from None


def git_status(root=ROOT):
    try:
        result = subprocess.run(["git", "--no-optional-locks", "status", "--porcelain=v1", "--untracked-files=all"],
                                cwd=root, env=child_env(), capture_output=True, text=True, timeout=10)
        if result.returncode:
            return {"passed": False, "reason": "git_status_unavailable"}
        # Never read diffs or report filenames. Only aggregate two-column statuses.
        statuses = {}
        for line in result.stdout.splitlines():
            status = line[:2]
            if re.fullmatch(r"[ MADRCU?!]{2}", status):
                statuses[status] = statuses.get(status, 0) + 1
            else:
                statuses["other"] = statuses.get("other", 0) + 1
        return {"passed": not statuses, "status_counts_names_redacted": statuses}
    except (OSError, subprocess.TimeoutExpired):
        return {"passed": False, "reason": "git_status_unavailable"}


def valid_attestation(value):
    if not isinstance(value, dict) or set(value) != {"passed", "reviewer", "reviewed_at", "reference"}:
        return False
    if value["passed"] is not True:
        return False
    # Nonsecret operator identifiers and ticket IDs only, not arbitrary notes/URLs.
    if not all(isinstance(value[k], str) and re.fullmatch(r"[A-Za-z0-9_-]{1,64}", value[k])
               for k in ("reviewer", "reference")):
        return False
    try:
        date = datetime.fromisoformat(value["reviewed_at"].replace("Z", "+00:00"))
        return date.tzinfo is not None and date <= datetime.now(date.tzinfo)
    except (ValueError, TypeError, AttributeError):
        return False


def evidence_checks(evidence, fingerprint):
    required = {"version", "artifact_fingerprint", "public_origin", "contact_email", "attestations"}
    schema = (isinstance(evidence, dict) and set(evidence) == required
              and evidence.get("version") == 1 and isinstance(evidence.get("attestations"), dict)
              and set(evidence["attestations"]) == set(ATTESTATIONS))
    if not schema:
        return {"evidence_schema": False}
    email = evidence["contact_email"]
    contact = (isinstance(email, str) and len(email) <= 254
               and re.fullmatch(r"[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,63}", email) is not None
               and not email.lower().endswith((".test", ".invalid", ".example", "@example.com", "@example.org", "@example.net")))
    return {"evidence_schema": True, "artifact_binding": (isinstance(fingerprint, str) and re.fullmatch(r"[0-9a-f]{64}", fingerprint) is not None
                                     and evidence["artifact_fingerprint"] == fingerprint),
            "production_origin": evidence["public_origin"] == ORIGIN, "contact_email_supplied": contact,
            **{name: valid_attestation(evidence["attestations"][name]) for name in ATTESTATIONS}}


def check(manifest_path, evidence_path, root=ROOT):
    automated = {}
    failures = []
    fingerprint = None
    try:
        current = manifest(root)
        fingerprint = current["fingerprint"]
        saved = read_json(operator_path(manifest_path, root))
        automated["artifact_hashes_and_fingerprint"] = {"passed": saved == current}
        if saved != current:
            failures.append("artifact_manifest_stale_or_invalid")
    except GateError as error:
        automated["artifact_hashes_and_fingerprint"] = {"passed": False}
        failures.append(str(error))
    automated["clean_git_tree"] = git_status(root)
    if not automated["clean_git_tree"]["passed"]:
        failures.append("git_tree_dirty_or_unavailable")
    try:
        evidence = read_json(operator_path(evidence_path, root))
        manual = evidence_checks(evidence, fingerprint)
    except GateError as error:
        failures.append(str(error))
        manual = {"evidence_available": False, "production_origin": False,
                          "contact_email_supplied": False, **{name: False for name in ATTESTATIONS}}
    failures.extend("manual_pending_" + name for name, passed in manual.items() if not passed)
    return {"version": 1, "status": "BLOCKED" if failures else "READY",
            "artifact_fingerprint": fingerprint, "automated": automated,
            "manual_attestations_not_automated_tests": manual,
            "local_docker_cli": "available_not_validated" if shutil.which("docker", path=child_env().get("PATH")) else "absent_manual_validation_pending",
            "failures": failures, "deployment_performed": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    create = commands.add_parser("manifest")
    create.add_argument("--output", required=True)
    gate = commands.add_parser("check")
    gate.add_argument("--manifest", required=True)
    gate.add_argument("--evidence", required=True)
    args = parser.parse_args()
    if args.command == "manifest":
        path = operator_path(args.output)
        report = manifest()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(report, indent=2) + "\n")
    else:
        report = check(args.manifest, args.evidence)
    print(json.dumps(report, indent=2))
    return 1 if report.get("status") == "BLOCKED" else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (GateError, OSError):
        print(json.dumps({"version": 1, "status": "BLOCKED", "failures": ["invalid_or_unavailable_declared_artifact_or_operator_path"], "deployment_performed": False}))
        sys.exit(1)
