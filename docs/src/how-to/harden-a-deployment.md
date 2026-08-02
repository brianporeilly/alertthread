# Harden a deployment

Two independent jobs: **close the webhook** with a bearer token, and **close the container**
with a read-only root filesystem, no capabilities and a seccomp profile. Do them in either
order.

**If you deploy with the Helm chart, the container half is already done.** Every setting in
[Close the container](#close-the-container) is a chart default, and `just chart` asserts each
one renders — so the fragments below describe what
[`charts/alertthread`](https://github.com/brianporeilly/alertthread/tree/main/charts/alertthread)
produces rather than something you have to paste. See
[Install with Helm](install-with-helm.md). Read on anyway if you deploy some other way, or if
you want to know what the chart is doing and why.

The webhook token is two-sided — the relay and the Alertmanager receiver — and the chart can
only do the relay's half, so that section applies whatever you deploy with.

The compose stack in this repository runs the relay under the same flags and `just e2e` drives
it, which is how they are kept working at runtime rather than only on paper.

## Require a bearer token on the webhook

The relay accepts unauthenticated webhook deliveries by default. Setting one option changes
that, and there are exactly two things to keep in step: the token the relay holds, and the
credential the Alertmanager receiver sends.

### 1. Create the secret

```bash
kubectl -n observability create secret generic alertthread-webhook \
    --from-literal=token="$(openssl rand -hex 32)"
```

### 2. Point the relay at it

With the chart, that is two values — it mounts the Secret and sets
`server.auth_token_file` for you:

```yaml
webhookAuth:
  enabled: true
  existingSecret: alertthread-webhook
  existingSecretKey: token
```

By hand:

```yaml
env:
  - name: ALERTTHREAD_SERVER__AUTH_TOKEN
    valueFrom:
      secretKeyRef:
        name: alertthread-webhook
        key: token
```

Or mount it and use `server.auth_token_file`, which is the better shape if you rotate secrets
with a controller that updates files in place:

```yaml
env:
  - name: ALERTTHREAD_SERVER__AUTH_TOKEN_FILE
    value: /etc/alertthread/webhook/token
volumeMounts:
  - name: webhook-token
    mountPath: /etc/alertthread/webhook
    readOnly: true
```

The file's trailing whitespace is trimmed, because `kubectl create secret --from-file` keeps
the newline. The value read from the environment variable is **not** trimmed — an environment
variable with a trailing space is a token with a trailing space.

The token is read once, at startup. A rotated secret needs a pod restart.

### 3. Send it from Alertmanager

```yaml
receivers:
  - name: alertthread
    webhook_configs:
      - url: http://alertthread.observability.svc:8080/webhook?channel=%23alerts-critical
        send_resolved: true
        max_alerts: 0
        http_config:
          authorization:
            type: Bearer
            credentials_file: /etc/alertmanager/secrets/alertthread-webhook/token
```

`credentials_file` rather than `credentials` so the token is not in the Alertmanager
`ConfigMap`. With `kube-prometheus-stack`, add the secret to
`alertmanager.alertmanagerSpec.secrets` and it appears under
`/etc/alertmanager/secrets/<name>/`.

⚠️ **Alertmanager does not trim what it reads from `credentials_file`.** A secret created with
a trailing newline sends a credential with a trailing newline, the relay answers `401`, and the
alerts in that delivery are lost. Create it with `--from-literal`, or `printf` rather than
`echo` if you write the file yourself.

### 4. Check it

```bash
kubectl -n observability port-forward deploy/alertthread 8080:8080

curl -si -X POST localhost:8080/webhook -d '{}' | head -1                    # 401
curl -si -X POST localhost:8080/webhook -H "authorization: Bearer $TOKEN" \
     -d '{"version":"4","status":"firing","alerts":[]}' | head -1            # 200
curl -si localhost:8080/readyz | head -1                                     # 200
```

The relay also says which mode it is in, once, at startup:

```
INFO POST /webhook requires the bearer token in server.auth_token; /healthz, /readyz and /metrics do not
```

If it says `server.auth_token is set to an empty value` instead, your secret did not resolve
and the webhook is **open** — that warning is the only signal, because an empty token behaves
exactly like no token.

### What the token does not cover

`/healthz`, `/readyz` and `/metrics` are never authenticated, and this is deliberate rather
than an omission:

| Endpoint | Why it stays open |
|---|---|
| `/healthz` | The kubelet sends no credentials. A `401` is a pod Kubernetes restarts for ever |
| `/readyz` | Same, and worse: a `401` is a pod that never joins the Service, so nothing receives webhooks at all |
| `/metrics` | Prometheus sends no credentials by default. A `401` here breaks the relay's own alerting, which is the failure this project exists to prevent |

None of the three reveals the contents of an alert. If you need them closed anyway, close them
at the network layer — a `NetworkPolicy` restricting port 8080 to Alertmanager and Prometheus
does the job without breaking the probes:

```yaml
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: alertthread
spec:
  podSelector:
    matchLabels: { app.kubernetes.io/name: alertthread }
  policyTypes: [Ingress]
  ingress:
    - from:
        - namespaceSelector:
            matchLabels: { kubernetes.io/metadata.name: monitoring }
      ports:
        - { protocol: TCP, port: 8080 }
```

Note that a `NetworkPolicy` restricting ingress does not block the kubelet's probes, which come
from the node.

### What a refusal looks like

Every refusal is the same `401`, with a bare `WWW-Authenticate: Bearer` header and the body
`unauthorized`. A caller cannot tell from the response whether it sent nothing, the wrong
credential, or one that is nearly right; the comparison against the configured token is
constant-time for the same reason.

You get the distinction, in the metrics and the log:

| `alertthread_webhook_requests_total{outcome=…}` | Meaning |
|---|---|
| `auth_missing` | No `Authorization` header, or one that was not a `Bearer` credential |
| `auth_mismatch` | A bearer credential that is not the configured token |

Both are logged at ERROR, because if the sender is your own Alertmanager then those alerts are
gone: **it does not retry a `401`.** `deploy/alertthread.rules.yaml` includes
`AlertthreadWebhookUnauthenticated` for exactly this.

---

## Close the container

The image is already non-root: it is `scratch`, with `USER 65532:65532` baked in and no shell,
no package manager and no writable path in it at all. What is left is the runtime side.

**This section used to be advice.** It is now the chart's defaults, under
`podSecurityContext` and `securityContext` in
[`values.yaml`](https://github.com/brianporeilly/alertthread/blob/main/charts/alertthread/values.yaml),
and `scripts/chart-test.py` fails if any one of them stops rendering — including the two
writable mounts. That check exists because a documented fragment nothing verifies drifts from
the code that has to honour it, which it did for a whole phase (ROADMAP known open item 18).
Change a value here and change it there, or the test will tell you.

```yaml
securityContext:
  runAsNonRoot: true
  runAsUser: 65532
  runAsGroup: 65532
  seccompProfile:
    type: RuntimeDefault
containers:
  - name: alertthread
    securityContext:
      allowPrivilegeEscalation: false
      privileged: false
      readOnlyRootFilesystem: true
      capabilities:
        drop: ["ALL"]
```

The relay needs no capability at all: it binds 8080, which is above 1024, and it never changes
uid, reads another process, or touches a device.

`seccompProfile: RuntimeDefault` has to be spelled out. Kubernetes defaults to `Unconfined`
unless the `SeccompDefault` feature gate is on for the kubelet — unlike podman and docker,
which apply their default profile automatically, which is why `compose.yaml` does not set it
and a manifest must.

### Writable paths, declared

`readOnlyRootFilesystem: true` breaks nothing on PostgreSQL: the relay writes to the network
and to stdout, and nothing else. **On SQLite it needs two mounts**, and the right fix is
declaring them rather than relaxing the flag.

```yaml
    volumeMounts:
      - name: state
        mountPath: /var/lib/alertthread
      - name: tmp
        mountPath: /tmp
volumes:
  - name: state
    persistentVolumeClaim:
      claimName: alertthread-state
  - name: tmp
    emptyDir:
      medium: Memory
      sizeLimit: 16Mi
```

| Path | Why |
|---|---|
| `/var/lib/alertthread` | `storage.url`'s database, plus the `-wal` and `-shm` files WAL mode creates beside it. All three must be on the same writable filesystem |
| `/tmp` | Where SQLite puts a spill file if a statement ever needs one. Nothing does today; an `emptyDir` costs nothing and turns a future crash into a non-event |

The PVC must be writable by uid 65532. `fsGroup: 65532` in the pod's `securityContext` is the
usual way; some CSI drivers need `fsGroupChangePolicy: OnRootMismatch` to avoid rechowning the
whole volume on every start. The chart sets both.

Note that a mounted Secret is affected by the same setting: with `fsGroup` set the kubelet
gives the volume `root:fsGroup` ownership, so a `defaultMode` of `0400` is unreadable by uid
65532 and `0440` is what works.

On SQLite the deployment must also be `strategy: Recreate` with exactly one replica — two
processes on one SQLite file is not a supported configuration. The chart refuses to render
`replicaCount > 1` on SQLite for that reason; the relay itself does not detect it (ROADMAP
known open item 21). See [Enable HA with PostgreSQL](enable-ha-postgres.md) for the other
shape, which has no PVC and no `/var/lib` mount at all.

Nothing may be mounted *inside* one of these mounts, either. The parent is read-only and the
image is `scratch`, so there is no writable directory for the kubelet to create the inner
mount point in. The chart keeps `/etc/alertthread/config`, `/etc/alertthread/secrets/*` and
`/etc/alertthread/templates` as siblings for exactly this reason, and a test asserts they stay
that way.

### Check it

```bash
kubectl -n observability exec deploy/alertthread -- /bin/sh    # fails: there is no shell
kubectl -n observability get pod -l app.kubernetes.io/name=alertthread \
    -o jsonpath='{.items[0].spec.containers[0].securityContext}'
```

The honest end-to-end check is a restart: `kubectl rollout restart` and then confirm
`/readyz` returns `200`. A read-only filesystem with nowhere for SQLite to write fails at
startup, loudly, with `unable to open database file` — see
[Troubleshoot](troubleshoot.md#the-relay-crash-loops-on-a-read-only-filesystem).

### The same settings, locally

`compose.yaml` runs the relay with `read_only: true`, `cap_drop: [ALL]`,
`no-new-privileges:true` and `tmpfs` mounts for `/data` and `/tmp`, and `just e2e` drives it —
so the hardening is exercised on every CI run rather than being advice. If you want to see the
failure mode instead, remove the `tmpfs` entry for `/data` and run `just e2e`.

The two checks answer different questions and neither replaces the other. `just e2e` proves
the relay *runs* under these flags; `just chart` proves the flags are still *set* in what
Kubernetes will be given. A regression in the second is invisible to the first, because the
compose stack has its own copy of the settings.

## Where to look next

- [Install with Helm](install-with-helm.md) — where every setting on this page is a default
- [Alert on the relay](alert-on-the-relay.md) — and why the rules must not be routed through it
- [Configuration](../reference/configuration.md) — `server.auth_token` and `server.auth_token_file`
- [HTTP API](../reference/http-api.md) — the status codes, including the `401`
- [Build and packaging](../explanation/build-and-packaging.md) — what the `scratch` image contains
