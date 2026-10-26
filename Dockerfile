FROM rust:slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends python3 pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# The web builder uses a minimal environment and standard HOME toolchain paths.
RUN ln -s /usr/local/cargo /root/.cargo && ln -s /usr/local/rustup /root/.rustup
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/domain ./crates/domain
COPY crates/migrate ./crates/migrate
COPY crates/server ./crates/server
COPY crates/web ./crates/web
COPY scripts/build-web.py scripts/local.py ./scripts/
RUN rustup target add wasm32-unknown-unknown
# Pin CLI to the lockfile's exact wasm-bindgen library version without shell substitution.
RUN python3 -c 'import pathlib, re, subprocess; lock = pathlib.Path("Cargo.lock").read_text(); version = re.search(r"\[\[package\]\]\s+name = \"wasm-bindgen\"\s+version = \"([^\"]+)\"", lock).group(1); subprocess.run(["cargo", "install", "wasm-bindgen-cli", "--version", version, "--locked"], check=True)'
RUN python3 scripts/build-web.py
RUN cargo build --locked --release -p debarred-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --user-group --uid 10001 --create-home app
WORKDIR /app
COPY --from=build /build/target/release/debarred-server /usr/local/bin/debarred-server
COPY --from=build /build/crates/web/dist /app/frontend
USER 10001:10001
ENV BIND_ADDRESS=0.0.0.0:3000 RUST_LOG=debarred_server=info FRONTEND_DIST=/app/frontend
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/debarred-server"]
