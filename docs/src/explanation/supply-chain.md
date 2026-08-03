# The supply chain

*What a release signs, what that proves, and where the chain is still weak.*

`alertthread` runs in the alerting path. If somebody can replace the image, they can silence
every alert in a cluster and the symptom is that nothing happens — which is the failure this
project exists to prevent, arriving by a different route. So the release is signed, and this
page is about what that is and is not worth.

[Verify artefacts](../how-to/verify-artifacts.md) is the procedure.
[Published artefacts](../reference/published-artifacts.md) is the reference.

## Keyless, because a key is a thing to lose

Signing keys have to live somewhere. A key in repository secrets is available to any workflow
that can be made to run, and a key on somebody's laptop is a single point of both compromise
and bus factor. Rotating either one invalidates every signature made with it.

Sigstore's keyless flow replaces the long-lived key with a short-lived certificate. The
release workflow presents its GitHub OIDC token; Fulcio issues a certificate binding a
signature to *that workflow, on that repository, at that tag*; the certificate expires in
minutes and the signing key never persists. What an operator checks is not "was this signed
by a key I trust" but "was this built by this workflow from this tag", which is the claim they
actually wanted.

The cost is a dependency on Sigstore's public good infrastructure being reachable at
verification time, and a verification command that needs two arguments nobody remembers. Both
are cheaper than key custody.

**A signature says who built it. It says nothing about whether what they built is correct.**
Nothing in this pipeline makes the relay less likely to have a bug.

## The SBOM covers the lock file, not the image

The obvious way to produce an SBOM is to scan the image. That does not work here, and the
reason is the runtime base:

```
$ syft scan podman:localhost/alertthread:dev -o json | jq '.artifacts | length'
0
```

The image is `FROM scratch` with a static musl binary and a CA bundle in it. There is no
package database, no distribution manifest, no shared libraries — nothing a scanner
recognises as a package, because there are no packages. An image-derived SBOM here would be
an empty document that looks like a complete one, which is worse than no document.

So the SBOM is generated from `Cargo.lock`, which is where the dependency inventory really
is: 300-odd crates with exact versions, in SPDX 2.3 JSON.

**It is a superset.** `Cargo.lock` covers the whole workspace, so it includes
dev-dependencies (`insta`, `wiremock`) and `dev/slack-mock`, none of which are linked into the
shipped binary. Reporting a crate that is not there is a false positive in a vulnerability
scan; reporting one that is would be a false negative. Between those two, the superset is the
safe direction, and it is stated rather than glossed.

The narrow answer exists — [`cargo auditable`](https://github.com/rust-secure-code/cargo-auditable)
embeds the actual dependency list of the built binary into the binary, which a scanner can
then read back off the image. It is not free: another tool in the build stage, and an
interaction with `strip = true` in the release profile that would need proving rather than
assuming. ROADMAP known open item 28 carries it.

### Both places, on purpose

The SBOM is attached to the image as a cosign attestation *and* to the GitHub release as a
file. They serve different readers: an admission controller checks the attestation and never
downloads a file, and a person answering "are we exposed to this advisory" at 2am wants a
file and does not want to authenticate to a registry first. Neither is a copy of the other
going stale — both are written once from the same bytes at release time.

## The builder base is still the weakest link

The image is built in two halves. The runtime half is `scratch`: nothing to attest, because
there is nothing there. The builder half is `docker.io/library/rust:1.97.1-alpine3.22`, which
is where the compiler that decides the binary's contents actually lives — and it is unsigned,
carries no provenance, and publishes no SBOM.

Moving it to a base that does have those things was spiked and failed: no hardened Rust
stream ships an `x86_64-unknown-linux-musl` standard library, so a musl static binary cannot
be built on one. ROADMAP known open item 24 records the whole attempt. The fallback taken was
to pin the base by digest, which fixes *what* is pulled without saying anything about whether
it is trustworthy.

Signing an artefact built on an unattested base leaves the weakest link exactly where the
claims are strongest, so the release states it instead of leaving it to be discovered:

```json
"builderBase": {
  "image": "docker.io/library/rust:1.97.1-alpine3.22",
  "digest": "sha256:df4efa4e…",
  "attested": false
}
```

That is a `build-inputs` attestation on the image digest, signed by the same keyless identity
as everything else. It does not improve the base. It makes the gap checkable by a policy — a
cluster can refuse a release whose builder base digest is not one it has approved — and it
means the next person to ask "what was this compiled with" does not have to read a Dockerfile
at a tag.

A digest pin has one failure mode of its own, and it is the kind this project keeps finding: a
pinned digest makes the version tag beside it decorative, so bumping the version without
bumping the digest is a silent no-op. The Dockerfile asserts `rustc --version` against the
declared toolchain for exactly that reason (ROADMAP known open item 25).

## What the chain does not cover

Stated plainly, because a partial guarantee described as a complete one is worse than none:

- **The builder base**, above.
- **crates.io.** 300 crates are pulled by `cargo` at build time. `cargo-deny` checks their
  licences and their advisories on every build; nothing checks that crates.io served the same
  bytes to us as to anybody else.
- **GitHub Actions itself.** The workflow trusts its runner, its cache, and the actions it
  calls. A compromised action in the release job signs whatever it likes with a legitimate
  identity.
- **Reproducibility.** Two builds of the same tag are not guaranteed to produce the same
  digest, so a signature cannot be independently reconstructed — only checked.
- **The chart's contents.** The chart is signed, which says it is the chart this repository
  released. What it renders is held down by `scripts/chart-test.py` on every pull request, and
  that is a different mechanism with a different failure mode.

## Why not GitHub's built-in attestations

`actions/attest-build-provenance` produces SLSA provenance stored in GitHub's own attestation
store and verified with `gh attestation verify`. It is good, and it is a second mechanism: a
second place attestations live, a second tool an operator installs, a second identity model to
explain. One tool that covers signature, SBOM and build inputs is easier to actually run than
two that each cover part of it, and `cosign` is the one a policy controller already speaks.

Worth revisiting if the provenance predicate ever needs to be richer than the build-inputs
document, which is deliberately small.
