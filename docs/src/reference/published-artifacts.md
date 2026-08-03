# Published artefacts

What a release publishes, where, and under what names.

> **Nothing is published yet.** The version in the tree is `0.0.0` and no tag exists. Every
> reference on this page is the name an artefact *will* have; none of them resolve today.
> [Verify artefacts](../how-to/verify-artifacts.md) is the procedure, and
> [The supply chain](../explanation/supply-chain.md) is what the signatures do and do not
> prove.

## Registry references

| Artefact | Reference |
|---|---|
| Image | `ghcr.io/brianporeilly/alertthread:<version>` |
| Chart | `oci://ghcr.io/brianporeilly/charts/alertthread` version `<version>` |
| Book | GitHub Pages, deployed from the tag |
| SBOM, build inputs, chart tarball | Attached to the GitHub release |

## Image tags

**One tag per release, and it is the exact version.** `0.1.0`, `0.1.1`, `0.2.0`.

There is **no `latest`**, and no floating `0` or `0.1`. A mutable tag makes "which build is
running" unanswerable at the moment somebody needs the answer, which for a relay in the
alerting path is during an incident. Renovate and Flux both bump an exact tag; neither needs a
floating one.

The digest is the real identity. `image.tag` in the chart accepts one:

```yaml
image:
  repository: ghcr.io/brianporeilly/alertthread
  tag: "0.1.0@sha256:…"
```

## Architectures

`linux/amd64` and `linux/arm64`, published as one manifest list. A pull resolves the right
one; nothing needs `--platform`.

Both are **native** builds — the arm64 image is compiled on an arm64 runner. The Dockerfile
builds a static musl binary on Alpine, whose native target is already musl, so neither
architecture is cross-compiled and neither is emulated.

## Versioning

**One number, in four places, and they are always equal.**

| Where | What it is |
|---|---|
| `Cargo.toml` `[workspace.package] version` | what `alertthread --version` prints |
| `Cargo.toml` three workspace path dependencies | the same number again |
| `Chart.yaml` `version` | the chart's version |
| `Chart.yaml` `appVersion` | the image tag the chart renders by default |

`release-please` rewrites all four from the Conventional Commit history, and
`scripts/release-version.py` — `just check-version`, run by `just ci` — fails the build when
one is left behind.

**The chart version and the app version move together.** This project has one release train:
a tag releases the repository, not a component of it, and every tag publishes an image. So
`appVersion` always names a tag that exists, which is the property that matters. The cost is
that a chart-only change ships as a version bump that also republishes an identical relay
binary; the benefit is that there is no state in which the chart names an image nobody built.

Semantics are semver, pre-1.0: a breaking change bumps the minor, everything else the patch.
`feat:` and `fix:` commit prefixes are what decide it.

## Attestations

Attached to the image manifest list's digest, keyless, verifiable with `cosign`.

| Predicate type | Contents |
|---|---|
| `spdxjson` | SPDX 2.3 SBOM of the locked Rust dependency graph |
| `https://alertthread.dev/attestations/build-inputs/v1` | builder base image and digest, Rust toolchain, source revision, platforms |

The SBOM is generated from `Cargo.lock`, not by scanning the image. Scanning the image finds
nothing: it is `scratch` plus one static binary and a CA bundle, with no package database to
catalogue. That means the SBOM is the **whole workspace's** locked graph — a superset of what
is linked into the shipped binary, because dev-dependencies and `dev/slack-mock` share the
lock file. See [The supply chain](../explanation/supply-chain.md).

The `build-inputs` predicate exists because the builder base is `docker.io/library/rust`,
which is unsigned and carries no provenance of its own. Attesting which base a release was
built on does not fix that; it makes it checkable.

## Signing identity

Keyless, through GitHub's OIDC provider. There is no private key in the repository, in its
secrets, or anywhere else.

| | |
|---|---|
| Issuer | `https://token.actions.githubusercontent.com` |
| Identity | `https://github.com/brianporeilly/alertthread/.github/workflows/release.yml@refs/tags/<tag>` |

Both are required arguments to `cosign verify`. Omitting them verifies that *something*
signed the image, which is not a useful claim.

## What is not published

- **crates.io.** `alertthread-core` is a plausible library and publishing it is deliberately
  still open (ROADMAP known open item 3); the workspace path dependencies carry versions so
  that it stays possible.
- **A Helm repository index.** The chart is an OCI artefact only. `helm repo add` does not
  apply; `helm install oci://…` does.
- **Debian, RPM or Homebrew packages.** The image is the artefact.
