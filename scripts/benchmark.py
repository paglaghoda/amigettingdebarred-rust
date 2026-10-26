#!/usr/bin/env python3
"""Explicit disposable-loopback benchmark; no dotenv discovery or provider calls."""
import argparse
import ipaddress
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlsplit

from local import ROOT, LocalError, binary, child_env


def validate_database(value):
    try:
        url = urlsplit(value)
        if (url.scheme in ("postgres", "postgresql")
                and ipaddress.ip_address(url.hostname).is_loopback
                and not url.query and not url.fragment
                and url.port != 0
                and re.fullmatch(r"/[A-Za-z0-9_-]+", url.path)):
            return value
    except (ValueError, TypeError):
        pass
    raise LocalError("BENCH_DATABASE_URL must name a disposable numeric-loopback PostgreSQL database, without query overrides.")


def stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=2)


def run_bounded(args, env, timeout, rss=False):
    # Temporary files avoid pipe deadlocks; captured diagnostics never reach a report.
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        process = subprocess.Popen(args, cwd=ROOT, env=env, stdout=out, stderr=err,
                                   start_new_session=True)
        peak = None
        deadline = time.monotonic() + timeout
        try:
            while process.poll() is None:
                if time.monotonic() >= deadline:
                    raise LocalError("Benchmark subprocess timed out; output suppressed. Inspect disposable DB for interrupted-run users.")
                if rss:
                    try:
                        sample = subprocess.run(["/bin/ps", "-o", "rss=", "-p", str(process.pid)],
                                                capture_output=True, text=True, timeout=1, env=env)
                        if sample.returncode == 0 and sample.stdout.strip().isdigit():
                            peak = max(peak or 0, int(sample.stdout.strip()) * 1024)
                    except (OSError, subprocess.TimeoutExpired):
                        pass
                time.sleep(0.05)
            if process.returncode:
                raise LocalError("Benchmark build/execution failed; diagnostics suppressed (no credentials logged).")
            out.seek(0)
            return out.read().decode(), peak
        finally:
            stop(process)


def check_report(report):
    return (report["version"] == 1
            and report["metadata"]["build_mode"] == "release"
            and report["metadata"]["debug_assertions"] is False
            and len(report["scenarios"]) == 7
            and all(s["error_count"] == 0 and s["sample_count"] == 100
                    and s["p95"] < 250 for s in report["scenarios"]))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / ".tools/performance/report.json")
    parser.add_argument("--check", action="store_true", help="Require zero errors and p95 <250ms (local budget only)")
    parser.add_argument("--rss", action="store_true", help="Sample executor PID RSS via /bin/ps, not Cargo or descendants")
    args = parser.parse_args()
    database = validate_database(os.environ.get("BENCH_DATABASE_URL", ""))
    env = child_env()
    cargo = binary("cargo", env)
    print("Local native PostgreSQL measurements are not Supabase/provider-auth measurements; no baseline comparison.", file=sys.stderr)
    # Build separately so the executor's 60 second limit and RSS exclude compilation.
    run_bounded([cargo, "build", "--offline", "--locked", "--release", "-p", "debarred-server", "--example", "benchmark"], env, 600)
    rustc, _ = run_bounded([binary("rustc", env), "--version"], env, 10)
    env["BENCH_DATABASE_URL"] = database
    raw, peak = run_bounded([str(ROOT / "target/release/examples/benchmark")], env, 60, args.rss)
    try:
        report = json.loads(raw)
        passed = check_report(report)
    except (ValueError, KeyError, TypeError):
        raise LocalError("Invalid executor JSON; output suppressed.") from None
    report["metadata"].update(rustc=rustc.strip(), platform_detail=platform.platform(),
                              peak_sampled_executor_rss_bytes=peak,
                              rss_sampling_interval_ms=50 if args.rss else None)
    report["local_budget"] = {"enforced": args.check, "passed": passed, "p95_limit_ms_exclusive": 250}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 1 if args.check and not passed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (LocalError, OSError):
        print("benchmark: failed; use explicit disposable BENCH_DATABASE_URL, installed toolchain and built web assets. No dotenv read; diagnostics suppressed.", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        sys.exit(130)
