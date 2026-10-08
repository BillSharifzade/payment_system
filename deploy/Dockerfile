# Multi-stage build for the payment system. Produces ONE image containing both
# binaries (payment-server + payment-workers); the compose files pick which to
# run per service. Runtime is distroless + nonroot: no shell, no package
# manager, minimal attack surface.
#
# Portable across the legacy Docker builder and BuildKit: plain FROM/COPY/RUN
# only (no `# syntax=` line, no --mount, no heredocs), so it builds anywhere
# `docker build` runs, including a production host without buildx.
#
# Layout (cargo-chef): chef = toolchain + cargo-chef; planner = computes the
# dependency recipe; builder = compiles dependencies in their own cached layer,
# then the workspace. A source-only change re-runs just the last `cargo build`
# instead of recompiling every crate from scratch (minutes vs. tens of minutes
# on a 2-vCPU box).
#
# Build from the REPO ROOT (the context must contain Cargo.*, crates/,
# migrations/ and rust-toolchain.toml):
#   docker build -f deploy/Dockerfile -t payment-system:<tag> .

# ---- chef ---------------------------------------------------------------
# Base images are pinned by digest (the tag is for humans). Refresh both with
# scripts/image-digest.sh <image:tag> when bumping.
FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS chef
WORKDIR /build
# The repo pins its toolchain in rust-toolchain.toml (channel "1.98"). Copy it
# in and let rustup install exactly that channel up front, in its own cached
# layer, so the image builds with the pinned toolchain even if someone bumps
# the FROM tag above without touching the file (or vice versa). The channel
# resolves to the latest 1.98.x patch — the same release the rust:1.98 image
# ships — at the cost of a one-off ~600 MB download in this layer only.
COPY rust-toolchain.toml ./
RUN rustup toolchain install && rustup show active-toolchain
# Pinned: builds must be reproducible. Bump deliberately.
RUN cargo install cargo-chef --locked --version 0.1.78

# ---- planner ------------------------------------------------------------
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo chef prepare --recipe-path recipe.json

# ---- builder ------------------------------------------------------------
FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
# Compiles ONLY the dependency graph. This layer is reused until Cargo.lock or
# a crate manifest changes.
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The storage crate embeds migrations/ at compile time (sqlx::migrate!).
COPY migrations ./migrations
RUN cargo build --release --locked --bin payment-server --bin payment-workers \
    && mkdir -p /out/kyc-docs \
    && cp target/release/payment-server target/release/payment-workers /out/

# ---- runtime ------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f AS runtime
COPY --from=builder /out/payment-server  /usr/local/bin/payment-server
COPY --from=builder /out/payment-workers /usr/local/bin/payment-workers
# Pre-create the KYC document directory owned by nonroot (uid/gid 65532). Docker
# copies this ownership onto a brand-new named volume mounted at this path, so
# the server can write uploads without a chown step (distroless has no shell to
# fix permissions later). Existing volumes keep their ownership — see
# deploy/README.md "KYC volume permissions".
COPY --from=builder --chown=65532:65532 /out/kyc-docs /data/kyc-docs
USER nonroot
# 8080 = HTTP API (BIND_ADDR), 9100/9101 = Prometheus metrics for server/workers
# (METRICS_ADDR). Metrics ports are internal-only; never publish them.
EXPOSE 8080 9100 9101
# Default entrypoint is the API server; the workers service overrides it.
# Both binaries also implement `healthcheck` (used by the compose healthchecks):
#   payment-server  healthcheck  -> GET 127.0.0.1:<BIND_ADDR port>/ready, exit 0 on 200
#   payment-workers healthcheck  -> exit 0 if WORKER_HEARTBEAT_FILE is fresh
ENTRYPOINT ["/usr/local/bin/payment-server"]
