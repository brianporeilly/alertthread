# Verify artefacts

**Goal:** confirm that the image and chart you are about to run were built by this
repository's release workflow, from this repository's source, and see what went into them.

You need [`cosign`](https://docs.sigstore.dev/cosign/system_config/installation/) and `jq`.

> **Nothing is published yet.** The commands below are the ones a release produces artefacts
> for; until `v0.1.0` is tagged, every reference in them resolves to nothing.
> [Published artefacts](../reference/published-artifacts.md) is the reference.

Throughout, set the version you are checking:

```bash
VERSION=0.1.0
IMAGE=ghcr.io/brianporeilly/alertthread
IDENTITY="https://github.com/brianporeilly/alertthread/.github/workflows/release.yml@refs/tags/v${VERSION}"
ISSUER=https://token.actions.githubusercontent.com
```

## 1. Verify the signature

```bash
cosign verify \
    --certificate-identity "$IDENTITY" \
    --certificate-oidc-issuer "$ISSUER" \
    "${IMAGE}:${VERSION}"
```

Both flags are required. Without them `cosign` will confirm that *somebody* signed the image,
which anybody can do to anything.

If the identity does not match, print what actually signed it before assuming an attack — a
workflow rename changes the string:

```bash
cosign verify --certificate-identity-regexp '.*' --certificate-oidc-issuer "$ISSUER" \
    "${IMAGE}:${VERSION}" 2>/dev/null \
    | jq -r '.[0].optional.Subject, .[0].optional.githubWorkflowRef'
```

## 2. Pin the digest

The tag is what you verified; the digest is what you should deploy. Take it out of the
verification rather than looking it up again — a second lookup can resolve to something else.

```bash
DIGEST=$(cosign verify \
    --certificate-identity "$IDENTITY" \
    --certificate-oidc-issuer "$ISSUER" \
    "${IMAGE}:${VERSION}" 2>/dev/null \
  | jq -r '.[0].critical.image."docker-manifest-digest"')
echo "$DIGEST"
```

Put it in the chart:

```yaml
image:
  repository: ghcr.io/brianporeilly/alertthread
  tag: "0.1.0@sha256:…"
```

A tag can be moved. This project does not move them, but "does not" is a policy and a digest
is a fact.

## 3. Read the SBOM

```bash
cosign verify-attestation --type spdxjson \
    --certificate-identity "$IDENTITY" \
    --certificate-oidc-issuer "$ISSUER" \
    "${IMAGE}:${VERSION}" \
  | jq -r '.payload' | base64 -d | jq '.predicate' > alertthread.spdx.json

jq -r '.packages[] | "\(.name) \(.versionInfo)"' alertthread.spdx.json | sort
```

This lists the **workspace's locked Rust dependency graph**, which is a superset of what is
linked into the shipped binary — dev-dependencies and the development slack-mock live in the
same `Cargo.lock`. It is not a lie about what is present, but it is not a minimal answer
either; [The supply chain](../explanation/supply-chain.md) says why.

The same file is attached to the GitHub release, if you would rather not need a registry
credential:

```bash
gh release download "v${VERSION}" --repo brianporeilly/alertthread --pattern 'sbom.spdx.json'
```

## 4. Check what it was built on

```bash
cosign verify-attestation \
    --type https://alertthread.dev/attestations/build-inputs/v1 \
    --certificate-identity "$IDENTITY" \
    --certificate-oidc-issuer "$ISSUER" \
    "${IMAGE}:${VERSION}" \
  | jq -r '.payload' | base64 -d | jq '.predicate'
```

```json
{
  "version": "0.1.0",
  "builderBase": {
    "image": "docker.io/library/rust:1.97.1-alpine3.22",
    "digest": "sha256:df4efa4e…",
    "attested": false
  },
  "rustToolchain": "1.97.1",
  "runtimeBase": "scratch",
  "platforms": ["linux/amd64", "linux/arm64"]
}
```

`"attested": false` is the honest part. The builder base is an upstream image that carries no
signature and no provenance of its own, so this records *which* base was used and does not
claim anything about it. That is the weakest link in the chain and it is stated rather than
implied.

## 5. Verify the chart

```bash
cosign verify \
    --certificate-identity "$IDENTITY" \
    --certificate-oidc-issuer "$ISSUER" \
    "ghcr.io/brianporeilly/charts/alertthread:${VERSION}"
```

The chart carries no SBOM. It is a few kilobytes of YAML with no dependencies, and an SBOM of
it would list nothing.

## Enforce it in a cluster

Verification a human runs once is a habit. To make it a rule, a policy controller checks the
same signature on every admission. With
[policy-controller](https://docs.sigstore.dev/policy-controller/overview/):

```yaml
apiVersion: policy.sigstore.dev/v1beta1
kind: ClusterImagePolicy
metadata:
  name: alertthread
spec:
  images:
    - glob: ghcr.io/brianporeilly/alertthread*
  authorities:
    - keyless:
        url: https://fulcio.sigstore.dev
        identities:
          - issuer: https://token.actions.githubusercontent.com
            subject: https://github.com/brianporeilly/alertthread/.github/workflows/release.yml@refs/tags/*
```

Kyverno's `verifyImages` rule takes the same two values.

## If verification fails

| Symptom | Usually means |
|---|---|
| `no matching signatures` | wrong `--certificate-identity`; check it against step 1's fallback |
| `no signatures found` | pulled a tag from before signing existed, or a mirror that did not copy the `.sig` tag |
| `error getting signer` on a mirror | the mirror copied the manifest but not the attached signature artefacts; verify against ghcr.io |
| identity ends `refs/heads/main` | not a release build — a manual workflow run, which is a dry run and publishes nothing |
