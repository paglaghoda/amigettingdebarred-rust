#!/usr/bin/env python3
"""Local-only development commands. Never reads dotenv files or uses hosted CLI commands."""
import argparse
import base64
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
from urllib.error import HTTPError, URLError
from urllib.parse import quote, unquote, urlsplit, urlunsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, Request, build_opener

ROOT = Path(__file__).resolve().parents[1]
API_URL = "http://127.0.0.1:54321"
DEV_EMAIL = "dev@vitattendance.test"
DEV_PASSWORD = "LocalAttendance123!"  # Public, disposable LOCAL-ONLY account.


class LocalError(Exception):
    pass


def child_env():
    # Deliberately do not inherit application credentials, PG*, AI*, proxies,
    # RUSTFLAGS, dynamic-loader overrides, or hosted Supabase configuration.
    allowed = ("HOME", "USER", "LOGNAME", "TMPDIR", "TEMP", "TMP", "PATH",
               "LANG", "LC_ALL", "TERM", "SYSTEMROOT")
    env = {name: os.environ[name] for name in allowed if name in os.environ}
    cargo = ROOT / ".tools/cargo"
    rustup = ROOT / ".tools/rustup"
    if cargo.is_dir():
        env["CARGO_HOME"] = str(cargo)
        env["PATH"] = str(cargo / "bin") + os.pathsep + env.get("PATH", os.defpath)
    if rustup.is_dir():
        env["RUSTUP_HOME"] = str(rustup)
    env.update(AI_API_KEY="", TRUSTED_PROXY_IPS="", NO_PROXY="localhost,127.0.0.1,::1")
    return env


def binary(name, env):
    found = shutil.which(name, path=env.get("PATH", os.defpath))
    if not found and name == "supabase":
        candidate = ROOT / ".tools/bin/supabase"
        if candidate.is_file() and os.access(candidate, os.X_OK):
            found = str(candidate)
    if not found:
        raise LocalError(f"Missing {name}; install it explicitly (see docs/local-development.md).")
    return found


def captured(args, env, label, timeout=30, cwd=None):
    try:
        result = subprocess.run(args, cwd=ROOT if cwd is None else cwd, env=env, capture_output=True,
                                text=True, timeout=timeout, check=False)
    except (OSError, subprocess.TimeoutExpired):
        raise LocalError(f"{label} could not finish; output suppressed to protect credentials.") from None
    if result.returncode:
        raise LocalError(f"{label} failed; output suppressed to protect credentials.")
    return result.stdout


def require_stack(env):
    cli = binary("supabase", env)
    docker = binary("docker", env)
    captured([docker, "info", "--format", "{{.ServerVersion}}"], env,
             "Docker daemon check", timeout=15)
    return cli


def cli_env(env):
    env = env.copy()
    # Only explicitly named development OAuth variables reach the CLI.
    for name in ("LOCAL_GOOGLE_CLIENT_ID", "LOCAL_GOOGLE_CLIENT_SECRET"):
        env[name] = os.environ.get(name, "")
    return env


def supabase_workdir():
    # Never let CLI dotenv/config discovery start at the repository root.
    workdir = ROOT / ".tools/local/supabase-project"
    for directory in (ROOT / ".tools", ROOT / ".tools/local", workdir, workdir / "supabase"):
        if directory.is_symlink():
            raise LocalError("Refusing symlinked local Supabase workdir.")
        directory.mkdir(exist_ok=True, mode=0o700)
    for directory in (workdir, workdir / "supabase"):
        for entry in directory.iterdir():
            if entry.is_symlink() or entry.name.startswith(".env") or entry.name.endswith(".env"):
                raise LocalError("Refusing dotenv files or symlinks in generated Supabase workdir.")
    source = ROOT / "supabase/config.toml"
    if source.is_symlink() or not source.is_file():
        raise LocalError("Expected a regular tracked supabase/config.toml.")
    # Copy this one file only: never copy root/supabase dotenv, links, or migrations.
    shutil.copyfile(source, workdir / "supabase/config.toml")
    return workdir


def supabase_command(cli, env, command, timeout=30):
    if command not in ("start", "status", "stop"):
        raise LocalError("Unsupported local Supabase command.")
    workdir = supabase_workdir()
    args = [cli, "--workdir", str(workdir), command]
    if command == "status":
        args.extend(["--output", "json"])
    return captured(args, cli_env(env), f"Local Supabase {command}",
                    timeout=timeout, cwd=workdir)


def is_loopback(host):
    if host == "localhost":
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except (ValueError, TypeError):
        return False


def validate_api(value):
    try:
        url = urlsplit(value)
        valid = (url.scheme == "http" and is_loopback(url.hostname)
                 and url.port == 54321 and not url.username and not url.password
                 and url.path in ("", "/") and not url.query and not url.fragment)
    except (ValueError, TypeError):
        valid = False
    if not valid:
        raise LocalError("Refusing non-local or unexpected Supabase API endpoint.")
    return API_URL


def validate_database(value):
    try:
        url = urlsplit(value)
        # Reject query parameters entirely: libpq host/hostaddr/options overrides
        # must never redirect an apparently local URI to another endpoint.
        valid = (url.scheme in ("postgres", "postgresql") and is_loopback(url.hostname)
                 and url.port == 54322 and not url.query and not url.fragment
                 and bool(url.username) and bool(url.password)
                 and re.fullmatch(r"/[A-Za-z0-9_-]+", url.path))
        if valid:
            user = quote(unquote(url.username), safe="")
            password = quote(unquote(url.password), safe="")
            return urlunsplit(("postgresql", f"{user}:{password}@127.0.0.1:54322",
                               url.path, "", ""))
    except (ValueError, TypeError):
        pass
    raise LocalError("Refusing non-local or ambiguous database endpoint; expected loopback port 54322.")


def local_status(cli, env):
    raw = supabase_command(cli, env, "status")
    try:
        status = json.loads(raw)
        api = validate_api(status.get("API_URL"))
        database = validate_database(status.get("DB_URL"))
        anon = status.get("ANON_KEY") or status.get("PUBLISHABLE_KEY")
        if not isinstance(anon, str) or not anon.strip():
            raise LocalError("Local status did not provide an anon/publishable key.")
        return api, database, anon
    except (ValueError, TypeError, AttributeError):
        raise LocalError("Invalid local status JSON; output suppressed.") from None


def session_key():
    directory = ROOT / ".tools/local"
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    path = directory / "session.key"
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    try:
        fd = os.open(path, flags, 0o600)
    except FileExistsError:
        pass
    else:
        with os.fdopen(fd, "w") as output:
            output.write(base64.b64encode(secrets.token_bytes(32)).decode() + "\n")
    # Refuse symlinks, insecure modes, and malformed keys rather than replacing them.
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o600:
        raise LocalError("Local session.key must be a regular file with mode 0600.")
    key = path.read_text().strip()
    try:
        if len(base64.b64decode(key, validate=True)) != 32:
            raise ValueError
    except (ValueError, base64.binascii.Error):
        raise LocalError("Invalid local session.key; refusing to overwrite it.") from None
    return key


def app_env(env, status=None):
    env = env.copy()
    env.update(PUBLIC_ORIGIN="http://localhost:3000", BIND="127.0.0.1:3000",
               BIND_ADDRESS="127.0.0.1:3000", LOCAL_AUTH_ENABLED="false",
               AI_API_KEY="", TRUSTED_PROXY_IPS="")
    if status is not None:
        api, database, anon = status
        env.update(SUPABASE_URL=validate_api(api), DATABASE_URL=validate_database(database),
                   SUPABASE_ANON_KEY=anon, SESSION_ENCRYPTION_KEY=session_key(),
                   LOCAL_AUTH_ENABLED="true")
    return env


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def seed(status):
    api, database, anon = status
    validate_api(api)
    validate_database(database)
    # No proxy or redirects: never forward a local password or anon key elsewhere.
    opener = build_opener(ProxyHandler({}), NoRedirect())
    body = json.dumps({"email": DEV_EMAIL, "password": DEV_PASSWORD}).encode()
    request = Request(API_URL + "/auth/v1/signup", data=body,
                      headers={"apikey": anon, "Content-Type": "application/json"}, method="POST")
    try:
        with opener.open(request, timeout=15) as response:
            response.read()  # Auth responses contain tokens; never display them.
    except HTTPError as error:
        # Repeated signup may report an existing user. Verify using the same
        # real password flow, rather than modifying auth.users or using admin keys.
        if error.code not in (400, 422):
            raise LocalError("Local signup failed; response suppressed.") from None
        request = Request(API_URL + "/auth/v1/token?grant_type=password", data=body,
                          headers={"apikey": anon, "Content-Type": "application/json"}, method="POST")
        try:
            with opener.open(request, timeout=15) as response:
                response.read()
        except (HTTPError, URLError, TimeoutError, OSError):
            raise LocalError("Local account creation/verification failed; response suppressed.") from None
    except (URLError, TimeoutError, OSError):
        raise LocalError("Cannot reach local Auth; response suppressed.") from None
    print(f"Local-only account ready: {DEV_EMAIL}. Password is documented, never logged here.")


def cargo_command(env, flag):
    args = [binary("cargo", env), "run", "-p", "debarred-server"]
    if flag:
        args.extend(["--", flag])
    # Foreground by design. Redact known local credentials from backend errors too.
    sensitive = [env.get(name, "") for name in
                 ("DATABASE_URL", "SUPABASE_ANON_KEY", "SESSION_ENCRYPTION_KEY")]
    if env.get("DATABASE_URL"):
        password = urlsplit(env["DATABASE_URL"]).password
        sensitive.extend([password, unquote(password)])
    sensitive.append(DEV_PASSWORD)
    try:
        with subprocess.Popen(args, cwd=ROOT, env=env, stdout=subprocess.PIPE,
                              stderr=subprocess.STDOUT, text=True) as process:
            for line in process.stdout:
                for value in sorted(filter(None, sensitive), key=len, reverse=True):
                    line = line.replace(value, "[redacted]")
                print(line, end="", flush=True)
            code = process.wait()
    except OSError:
        raise LocalError("Could not launch Cargo.") from None
    if code:
        raise LocalError(f"Cargo exited with status {code}.")


def check(env):
    missing = False
    for name in ("cargo", "rustup", "wasm-bindgen", "supabase", "docker"):
        try:
            binary(name, env)
            print(f"{name}: available")
        except LocalError:
            print(f"{name}: missing")
            missing = True
    if shutil.which("docker", path=env.get("PATH", os.defpath)):
        try:
            require_stack(env)
            print("Docker daemon: reachable")
        except LocalError:
            print("Local stack prerequisites: unavailable (CLI or Docker daemon)")
            missing = True
    print("No dotenv files read. See docs/local-development.md for setup and preview mode.")
    return 1 if missing else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name, help_text in {
        "check": "Check prerequisites without starting anything",
        "start": "Start local Supabase (requires CLI and Docker daemon)",
        "status": "Show safe local endpoints, never keys",
        "stop": "Stop local Supabase, retaining its data",
        "seed": "Create/verify the disposable local Auth account",
        "migrate": "Run server --migrate against verified local DB",
        "run": "Migrate local DB, then run server in foreground",
        "preview": "Run --preview: device-only sandbox, no Supabase or database",
    }.items():
        commands.add_parser(name, help=help_text, description=help_text)
    args = parser.parse_args()
    env = child_env()
    if args.command == "check":
        return check(env)
    if args.command == "preview":
        cargo_command(app_env(env), "--preview")
        return 0
    cli = require_stack(env)
    if args.command in ("start", "stop"):
        supabase_command(cli, env, args.command, timeout=600)
        print(f"Local Supabase {args.command} completed; credentials suppressed.")
        if args.command == "stop":
            return 0
    status = local_status(cli, env)
    if args.command in ("start", "status"):
        print(f"Local API: {API_URL}\nLocal DB: 127.0.0.1:54322\nStudio: http://localhost:54323\nMail: http://localhost:54324")
    elif args.command == "seed":
        seed(status)
    else:
        env = app_env(env, status)
        cargo_command(env, "--migrate")
        if args.command == "run":
            # No extra arguments means the normal foreground server.
            cargo_command(env, "")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except LocalError as error:
        print(f"local: {error}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        sys.exit(130)
    except (OSError, ValueError):
        print("local: operation failed; details suppressed to protect credentials.", file=sys.stderr)
        sys.exit(1)
