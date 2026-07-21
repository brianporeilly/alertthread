# Build and packaging

*Why the binary is built the way it is, and what was measured to justify it.*

This page records the outcome of the Phase 0 build spike. ADR 001 (D1) claims Rust buys
"a ~8 MB static musl binary on `scratch`". That claim rests entirely on `libsqlite3-sys` —
a C dependency — linking cleanly under musl. If it did not, the Dockerfile and possibly the
store crate would need reshaping, so it was validated before anything else was written.

## Outcome: the ADR's claim holds

`sqlx` 0.9 with bundled SQLite cross-compiles to `x86_64-unknown-linux-musl` as a fully
static binary and runs from a `scratch` image. **No fallback was needed.** The
distroless-glibc and `rusqlite` alternatives listed as contingencies were not used.

### Measured sizes

| Build | Binary | `scratch` image |
|---|---|---|
| Phase 0 as it stands today (no dependencies yet) | 381 KB | 570 KB |
| `sqlx` + bundled SQLite only | 1.78 MB | 1.78 MB |
| Full projected dependency set | 5.84 MB | 5.84 MB |
| Full set + CA certificates | 5.84 MB | **6.02 MB** |

The first row is what this repository actually produces right now and is not a useful
prediction — the Phase 0 crates have no dependencies. **6.02 MB is the number to compare
against the ADR**, because it is the one measured with everything ADR 001 commits to
actually linked in.

The "full projected dependency set" is every crate ADR 001 D1 commits to — `sqlx` with both
the `sqlite` and `postgres` drivers, `axum`, `tower-http`, `reqwest` with rustls, `minijinja`,
`prometheus-client`, `figment`, `chrono`, `tracing` and `tracing-subscriber` — linked into
one binary with each dependency actually referenced so the linker could not discard it.

**6.02 MB against the ADR's ~8 MB estimate.** The ADR is conservative rather than wrong, so
no revision is required. The number will drift upward as real code replaces the spike's
placeholder calls, but there is roughly 2 MB of headroom before the published figure is
misleading.

Build settings that materially affect this, all set in the workspace `Cargo.toml`:
`opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.

### Verification

```
$ file muslspike
ELF 64-bit LSB pie executable, x86-64, static-pie linked, stripped

$ podman run --rm localhost/muslspike:certs
full-stack musl spike OK: sqlite 3.51.3 row=(abc123, #alerts)
```

`static-pie linked` is the property that matters: no interpreter, no shared objects, so
`scratch` is sufficient.

## Two findings that changed the Dockerfile

The spike is worth reading for these, because both fail at *runtime* rather than at build
time — the second one specifically would have surfaced as "Slack calls fail in production
but work locally".

### 1. `scratch` has no CA certificates

`reqwest` 0.13's `rustls` feature pulls in `rustls-platform-verifier`, which loads roots
from the system trust store. On `scratch` there is no trust store, and building a client
fails outright:

```
reqwest::Error { kind: Builder,
  source: General("No CA certificates were loaded from the system") }
```

This is not a TLS handshake failure that shows up on first use — `Client::builder().build()`
returns `Err` at startup. The fix in the Dockerfile is to copy a CA bundle out of the builder
stage:

```dockerfile
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/
```

That costs 180 KB and is why the image is 6.02 MB rather than 5.84 MB. The alternative —
bundling `webpki-roots` into the binary — was rejected because it moves trust-root updates
from "rebuild the image" to "bump a crate and rebuild the image", which is strictly worse
for a security-relevant input, and because `rustls-platform-verifier` is what the `rustls`
feature selects by default.

### 2. `reqwest` 0.13 renamed its TLS features

`rustls-tls` does not exist in 0.13; it is now `rustls`. A `Cargo.toml` carried over from
0.12 fails to resolve with a bare "does not have that feature" error. The relevant names are
`rustls`, `rustls-no-provider`, `native-tls` and `rustls-native-certs`. ROADMAP.md flags
`reqwest` 0.13 as "newer than the common 0.12"; this is the concrete edge that bites.

Building `rustls` under musl also needs `cmake`, `make` and `g++` in the builder image, as
its default `aws-lc-rs` provider compiles C.

## Why `scratch` rather than distroless

`scratch` contains nothing: no shell, no package manager, no libc, no setuid binaries. For a
service sitting in the alerting path the audit story is worth the inconvenience, and the
inconvenience is small because a static binary has nothing to inconvenience it. The cost is
that debugging inside the container is impossible — there is nothing to exec. That is
acceptable for a process whose entire diagnostic surface is structured logs on stdout,
Prometheus metrics, and `/healthz` and `/readyz`.

`gcr.io/distroless/cc-debian12` remains the documented fallback if a future dependency
cannot be linked statically. It was not needed.

## The build itself

The Dockerfile uses `cargo-chef` to cache dependency compilation separately from application
compilation. Without it, touching one line of source recompiles the entire dependency
tree — with `sqlx`, `axum` and `reqwest` in it, that is the difference between a ten-second
and a two-minute rebuild.

Stages:

1. **chef** — `rust:alpine` with the musl toolchain and `cargo-chef`.
2. **planner** — produces `recipe.json`, a dependency-only manifest.
3. **builder** — cooks the recipe (cached unless dependencies change), then builds the app.
4. **runtime** — `scratch`, plus the binary and the CA bundle.

Alpine is used as the build base rather than a Debian image with a musl cross-toolchain
because Alpine's *native* target is already musl, so nothing is cross-compiled and
`libsqlite3-sys` builds with a plain native `cc`. That removes the whole category of
cross-compilation problems the spike existed to check for.

## Reproducing

There is deliberately no `just` recipe for this. The recipe list is fixed to the set
AGENTS.md documents, and image building belongs to CI and release rather than the developer
inner loop.

```
podman build -t localhost/alertthread:dev .
podman run --rm localhost/alertthread:dev
```
