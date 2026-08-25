# Multi-stage build for the payment system. Produces ONE image containing both
# binaries (payment-server + payment-workers); the compose file picks which to
# run per service. Runtime is distroless + nonroot: no shell, no package
# manager, minimal attack surface.
#
# Portable across the legacy Docker builder and BuildKit (no BuildKit-only
# features), so it builds anywhere `docker build` runs.

# ---- builder -------------------------------------------------------------
FROM rust:1.96-bookworm AS builder
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY migrations ./migrations

# ring (the TLS backend) needs only a C compiler, already in the rust image.
RUN cargo build --release --locked \
        --bin payment-server --bin payment-workers && \
    cp target/release/payment-server target/release/payment-workers /usr/local/bin/

# ---- runtime -------------------------------------------------------------
FROM gcr.io/distroless/cc-debian12:nonroot AS runtime
COPY --from=builder /usr/local/bin/payment-server /usr/local/bin/payment-server
COPY --from=builder /usr/local/bin/payment-workers /usr/local/bin/payment-workers
USER nonroot
EXPOSE 8080
# Default entrypoint is the API server; the workers service overrides it.
ENTRYPOINT ["/usr/local/bin/payment-server"]
