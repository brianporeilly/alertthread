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

### Measured size, on the shipped artefact

Measured on `linux/amd64` from `podman build -t localhost/alertthread:dev .`, on 2026-08-03:

| | Bytes | |
|---|---:|---|
| Binary | 8 659 056 | `sha256:b1411e4a…b90f` |
| CA bundle | 179 359 | |
| **Image** | **8 847 180** | 8.85 MB |

The remainder is tar metadata. `podman image inspect` reports the same 8.85 MB.

**This slightly exceeds ADR 001's "~8 MB static musl binary on `scratch`".** The estimate was
made before any of the code existed and it is close, not wrong; the divergence is recorded in
ROADMAP known open item 26 rather than by editing the ADR, because the ADR's estimate was
itself a decision input and is preserved as it was written.

Build settings that materially affect this, all set in the workspace `Cargo.toml`:
`opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.

### The Phase 0 projections, and why they are no longer the number

The spike that validated musl linking, before any relay code was written, projected:

| Build | Binary | `scratch` image |
|---|---|---|
| Phase 0 as it stood, no dependencies | 381 KB | 570 KB |
| `sqlx` + bundled SQLite only | 1.78 MB | 1.78 MB |
| Full projected dependency set | 5.84 MB | 5.84 MB |
| Full set + CA certificates | 5.84 MB | 6.02 MB |

Those rows linked every crate ADR 001 D1 commits to, with each one actually referenced so the
linker could not discard it — but they linked *only* that, against placeholder calls. The
shipped binary is 47% larger, which is what real code, real error taxonomies, real templates
and real tests around them cost. **6.02 MB was a prediction and is kept here as one.** The
table above is the measurement.

### Verification

```
$ file alertthread
ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV), static-pie linked, stripped

$ podman run --rm localhost/alertthread:dev --version
alertthread 0.0.0 (core 0.0.0, store 0.0.0, slack 0.0.0)
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

That costs 179 359 bytes of the shipped image, and is the whole difference between the binary
and the image. The alternative —
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

The builder base is pinned by digest and is not attested by its publisher, which is the
weakest link in the release; [The supply chain](supply-chain.md) covers what that means.

## Two architectures, neither of them emulated

Releases publish `linux/amd64` and `linux/arm64` as one manifest list, and **each is compiled
on a runner of its own architecture**.

That follows from the paragraph above rather than being a separate choice. The build is native
because Alpine's target is musl; the moment an arm64 image is built on an amd64 runner, either
a cross-toolchain reappears — the exact thing this Dockerfile was shaped to avoid — or `rustc`
runs under QEMU, and emulated compilation of `sqlx`, `axum` and `reqwest` is slow enough to
make the job unusable. A native arm64 runner costs nothing on a public repository and keeps
both halves of the argument intact.

The target is derived from `TARGETARCH` inside the builder stage rather than passed in as a
build argument, with `uname -m` as the fallback when a builder does not set it. Defaulting to
amd64 instead would silently produce an x86_64 binary on an arm64 host — a build that
succeeds and ships the wrong thing.

## Reproducing

```
just image                 # builds and smoke-tests localhost/alertthread:dev
podman build -t localhost/alertthread:dev .
```

`just image` is what CI runs on every pull request. It builds the image and then checks that
the static binary executes on `scratch` at all, and that a start with no token is a clean
non-zero rather than a hang.
