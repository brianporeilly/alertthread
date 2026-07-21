# syntax=docker/dockerfile:1
#
# cargo-chef -> musl static -> scratch
#
# Validated by the Phase 0 build spike; the measurements and the two runtime
# gotchas this file works around are recorded in
# docs/src/explanation/build-and-packaging.md. Read that before changing the
# runtime stage.
#
# Alpine is the build base rather than a Debian image with a musl
# cross-toolchain because Alpine's *native* target is already musl. Nothing is
# cross-compiled, so libsqlite3-sys builds with a plain native cc and the whole
# category of cross-compilation problems does not arise.

ARG RUST_VERSION=1.97.1
ARG ALPINE_VERSION=3.22

# ---------------------------------------------------------------------------
# chef — build toolchain, shared by the planner and builder stages
# ---------------------------------------------------------------------------
FROM docker.io/library/rust:${RUST_VERSION}-alpine${ALPINE_VERSION} AS chef

# musl-dev: libc headers for libsqlite3-sys.
# cmake/make/g++: rustls' default aws-lc-rs provider compiles C.
RUN apk add --no-cache musl-dev cmake make g++ perl \
    && cargo install cargo-chef --locked

WORKDIR /build

# ---------------------------------------------------------------------------
# planner — reduce the workspace to a dependency-only recipe
# ---------------------------------------------------------------------------
FROM chef AS planner

COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---------------------------------------------------------------------------
# builder — cook dependencies, then build the application
# ---------------------------------------------------------------------------
FROM chef AS builder

ARG TARGET=x86_64-unknown-linux-musl

# Cooking the recipe compiles only dependencies. This layer is cached unless
# the dependency set itself changes, which is the entire point: with sqlx,
# axum and reqwest in the tree, the difference between a cached and an
# uncached rebuild is roughly ten seconds versus two minutes.
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --target ${TARGET} --recipe-path recipe.json

COPY . .
RUN cargo build --release --target ${TARGET} --package alertthread --bin alertthread \
    && cp target/${TARGET}/release/alertthread /build/alertthread

# scratch has no trust store, and reqwest's rustls feature resolves roots
# through rustls-platform-verifier — which fails at Client::builder().build(),
# at startup, not on first request. Stage the bundle for the runtime image.
RUN apk add --no-cache ca-certificates

# ---------------------------------------------------------------------------
# runtime — scratch
# ---------------------------------------------------------------------------
FROM scratch AS runtime

# ~180 KB, and the reason the image is 6.02 MB rather than 5.84 MB.
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /build/alertthread /alertthread

# Numeric, because scratch has no /etc/passwd to resolve a name against.
# 65532 is the conventional "nonroot" uid, matching distroless.
USER 65532:65532

EXPOSE 8080

# No shell, so this must be exec form. There is nothing in the image to
# interpolate a string with.
ENTRYPOINT ["/alertthread"]

LABEL org.opencontainers.image.title="alertthread" \
      org.opencontainers.image.description="Alertmanager to Slack relay with fingerprint-keyed threading and update-on-resolve" \
      org.opencontainers.image.source="https://github.com/brianporeilly/alertthread" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
