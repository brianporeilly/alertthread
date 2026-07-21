# Enable HA with PostgreSQL

**Goal:** run more than one `alertthread` replica, so a node drain or a rolling update does
not stop alerts reaching Slack.

`alertthread` defaults to SQLite, which is exactly one replica by design. Switching the state
store to PostgreSQL is what makes more than one legal. This guide is the switch, start to
finish, for an existing deployment.

> Why the two backends differ, and why the relay refuses to run SQLite with two replicas, is
> in [ADR 001 D4](../adr/001-adr.md). This page assumes you have decided to do it.

> **Status.** The PostgreSQL store — schema, migrations, the shared-queue lease, the pruner
> — is complete and continuously tested against a real server as of Phase 2. The `figment`
> configuration layer and the Deployment that reads it arrive in Phase 4, so the manifests
> below describe the shape the switch takes rather than something you can apply today.

## Before you start

You need:

- An existing PostgreSQL the relay can reach — a CloudNativePG `Cluster`, RDS, anything.
  The relay does not manage one.
- A role that can `CREATE TABLE` and `CREATE INDEX` in the target database. The relay applies
  its own migrations at startup, so this is needed on every start, not just the first.
- The relay's current state file, if you care about in-flight alerts. See
  [What happens to the alerts already in flight](#what-happens-to-the-alerts-already-in-flight).

## 1. Create the database and role

```sql
CREATE ROLE alertthread LOGIN PASSWORD 'use-a-real-secret';
CREATE DATABASE alertthread OWNER alertthread;
```

No schema: the relay creates its own tables the first time it starts.

## 2. Put the connection string in a Secret

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: alertthread-storage
stringData:
  DATABASE_URL: postgres://alertthread:use-a-real-secret@pg-rw.databases.svc:5432/alertthread
```

If your PostgreSQL operator already publishes a connection Secret — CloudNativePG's
`<cluster>-app` does — reference that instead of writing the password twice.

## 3. Point the relay at it

Two environment variables, and one that is now wrong:

```yaml
env:
  - name: STATE_BACKEND
    value: postgres
  - name: DATABASE_URL
    valueFrom:
      secretKeyRef:
        name: alertthread-storage
        key: DATABASE_URL
```

Remove any `STATE_BACKEND=sqlite` and any SQLite path still set. Leaving both is not
ambiguous — `STATE_BACKEND` decides — but it is the sort of thing that misleads whoever
reads the manifest next.

## 4. Drop the PVC and change the deploy strategy

The RWO volume is what forced `Recreate`. With no volume, both go:

```yaml
spec:
  strategy:
    type: RollingUpdate   # was: Recreate
  template:
    spec:
      containers:
        - name: alertthread
          # volumeMounts: — remove the state volume
      # volumes: — remove the PVC
```

Do this in the same change as step 3. A replica that still mounts an RWO PVC cannot be
scheduled alongside another one, and the symptom is a pod stuck `Pending` with a volume
multi-attach error rather than anything mentioning the relay.

## 5. Start one replica and confirm the migrations applied

Roll out with `replicas: 1` first. On start the relay applies its migrations and logs the
resolved backend.

```console
$ kubectl logs deploy/alertthread | head
{"level":"info","message":"state store ready","backend":"postgres"}
```

Then confirm the schema is there:

```console
$ psql "$DATABASE_URL" -c '\dt'
         List of relations
 Schema |      Name       | Type
--------+-----------------+-------
 public | _sqlx_migrations| table
 public | alert_message   | table
 public | group_message   | table
 public | outbox          | table
```

If the relay cannot reach PostgreSQL it fails to start rather than starting empty. That is
deliberate: an alerting component that comes up with no correlation state would silently
post every resolution as an orphan.

## 6. Scale up

```console
$ kubectl scale deploy/alertthread --replicas=3
```

Nothing else changes. Every replica claims alerts against the same
`(fingerprint, channel)` primary key and leases outbox work with `FOR UPDATE SKIP LOCKED`,
so they share the queue without coordinating.

## 7. Divide the Slack rate limit by the replica count

**Do this, or Slack will start returning 429 under storms.**

Each replica holds its own per-channel token bucket, so three replicas send up to three
messages per second to one channel against Slack's limit of about one.

```yaml
slack:
  rate_limit_divisor: 3   # = spec.replicas
```

Update it whenever you change `replicas`. The 429 handling is the real backstop — the relay
honours `Retry-After` and does not count a rate-limited attempt as a failure — but a divisor
that matches reality is what keeps you off it.

## Verify

Fire a test alert and watch it correlate across replicas:

```console
$ kubectl get pods -l app=alertthread          # note the pod names
$ kubectl delete pod <the one that posted>     # kill it mid-flight
```

The alert's resolution should still update the original message, from a different pod. If it
posts a fresh "resolved" message instead, the replicas are not sharing state — check that
every pod has the same `DATABASE_URL` and that none is still on SQLite.

## What happens to the alerts already in flight

Switching backends does **not** migrate state. The new database starts empty, and the SQLite
file is left alone.

Consequences, both temporary:

- Alerts that were firing before the switch have no correlation state afterwards, so their
  resolutions arrive as orphans: they post a standalone resolved message instead of editing
  the original. Noisy, never silent. `alertthread_orphan_resolves_total` will step up once.
- Anything still in the outbox when the old pod stopped is in the SQLite file, not the new
  database, and will not be delivered. Alertmanager re-sends on its `repeat_interval`, so
  anything still firing comes back.

To keep the window small, switch at a quiet moment and keep the old PVC until the next
`repeat_interval` has passed. There is no supported state migration between backends; the
relay is designed so that losing correlation state degrades to noise rather than to silence,
and that is the property being relied on here.

## Going back to SQLite

Reverse steps 3 and 4, and scale to **exactly one replica before** changing
`STATE_BACKEND`. The relay refuses to start as SQLite when it detects more than one replica,
so doing it the other way round leaves you with a Deployment that will not roll.

## Related

- [Configuration reference: `storage`](../reference/configuration.md) — every option, its
  environment variable, and its default.
- [ADR 001 D4](../adr/001-adr.md) — why there are two backends and what each one costs.
