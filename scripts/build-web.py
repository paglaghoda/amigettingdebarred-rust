#!/usr/bin/env python3
"""Build Leptos hydration assets without Node or automatic tool installation."""
import gzip
import re
import shutil
import subprocess
import sys

from local import ROOT, LocalError, binary, captured, child_env


def precompress_assets(assets):
    extensions = {".wasm", ".js", ".css", ".ttf", ".svg", ".json"}
    # Snapshot first: generated siblings must never become compression inputs.
    sources = sorted(path for path in assets.rglob("*")
                     if path.is_file() and path.suffix.lower() in extensions)
    wasm_bytes = 0
    wasm_gzip_bytes = 0
    for source in sources:
        data = source.read_bytes()
        compressed = gzip.compress(data, compresslevel=9, mtime=0)
        destination = source.with_name(source.name + ".gz")
        if len(compressed) < len(data):
            destination.write_bytes(compressed)
            if source.suffix.lower() == ".wasm":
                wasm_gzip_bytes += len(compressed)
        else:
            destination.unlink(missing_ok=True)
        if source.suffix.lower() == ".wasm":
            wasm_bytes += len(data)
    print(f"Wasm totals: {wasm_bytes} bytes uncompressed; "
          f"{wasm_gzip_bytes} bytes in stored .gz siblings.")
    return wasm_bytes, wasm_gzip_bytes


def main():
    if len(sys.argv) != 1:
        raise LocalError("Usage: sh scripts/build-web.sh (no arguments)")
    env = child_env()
    cargo = binary("cargo", env)
    bindgen = binary("wasm-bindgen", env)
    lock = (ROOT / "Cargo.lock").read_text()
    match = re.search(r'\[\[package\]\]\s+name = "wasm-bindgen"\s+version = "([^"]+)"', lock)
    if not match:
        raise LocalError("Cargo.lock has no wasm-bindgen package; resolve workspace dependencies first.")
    version = match.group(1)
    installed = captured([bindgen, "--version"], env, "wasm-bindgen version check").strip()
    if installed != f"wasm-bindgen {version}":
        raise LocalError(f"wasm-bindgen CLI must match Cargo.lock ({version}). "
                         f"Install explicitly: cargo install wasm-bindgen-cli --version {version} --locked")
    try:
        subprocess.run([cargo, "build", "--locked", "--release", "--target", "wasm32-unknown-unknown",
                        "-p", "debarred-web", "--no-default-features", "--features", "hydrate"],
                       cwd=ROOT, env=env, check=True)
        assets = ROOT / "crates/web/dist/assets"
        assets.mkdir(parents=True, exist_ok=True)
        subprocess.run([bindgen, "--target", "web", "--out-dir", str(assets), "--out-name", "app",
                        str(ROOT / "target/wasm32-unknown-unknown/release/debarred_web.wasm")],
                       cwd=ROOT, env=env, check=True)
        public = ROOT / "crates/web/public"
        for source in public.iterdir():
            destination = assets / source.name
            if source.is_dir():
                shutil.copytree(source, destination, dirs_exist_ok=True)
            else:
                shutil.copy2(source, destination)
        precompress_assets(assets)
    except subprocess.CalledProcessError:
        raise LocalError("Web build failed. Check the Rust wasm target and workspace setup; see docs/local-development.md.") from None
    print("Built crates/web/dist/assets (Wasm, JS glue, and all public assets).")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (LocalError, OSError) as error:
        print(f"build-web: {error}", file=sys.stderr)
        sys.exit(1)
