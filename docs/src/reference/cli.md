# Command line

`alertthread` takes one optional positional argument and one subcommand.

```
alertthread [CONFIG]                run the relay
alertthread replay [OPTIONS]        return parked operations to the outbox
alertthread --version               print the build identity
alertthread --help                  print usage
```

**A bare `alertthread`, or `alertthread <path>`, runs the relay.** That has not changed and
will not: only the exact literal `replay` as the first argument selects the subcommand, so a
configuration file called `replay.yaml`, or `./replay`, is still a path.

| Argument | Meaning |
|---|---|
| `CONFIG` | Path to a YAML configuration file. Optional |
| `--version`, `-V` | Print `alertthread <version> (core …, store …, slack …)` and exit `0`. Answered before anything is loaded, so it works on the `scratch` image with no configuration |
| `--help`, `-h` | Print usage and exit `0` |

Configuration resolves the same way for every form: the path on the command line, then
`ALERTTHREAD_CONFIG`, then the environment and the built-in defaults. Every setting is in
[Configuration](configuration.md).

Exit codes are `0` for success and `1` for anything else, including a command line this
build cannot parse. There is no third value.

---

## `alertthread replay`

Returns dead-lettered operations to the outbox. A dead letter is an alert that was accepted,
queued and then given up on — see [`alertthread_dead_letter_total`](metrics.md) — and this is
the supported way back from one when nothing revives it automatically.

**It is a dry run unless you pass `--commit`.**

| Option | Meaning |
|---|---|
| `--channel <CHANNEL>` | Only operations addressed to this channel, matched exactly, including the leading `#` |
| `--fingerprint <FINGERPRINT>` | Only operations for this alert fingerprint, matched exactly |
| `--commit` | Actually re-queue. Without it nothing is written |
| `--config <PATH>` | Configuration file, as the positional argument to the relay |
| `--help`, `-h` | Print usage and exit `0` |

Both filters may be given together, in which case they are `AND`ed. Giving neither selects
every parked operation.

An empty value — `--channel ""` — is refused rather than treated as "no filter", because a
shell variable that failed to expand would otherwise widen a targeted replay into the whole
queue. A repeated `--channel` is refused for the same reason: taking the last one silently
would act on one channel while reading as though it asked for two.

### What it does

`replay` clears `dead_lettered_at`, resets the operation's attempt budget, and — for a
parked *post* — returns the alert from `failed` to `claimed` so its eventual resolution
correlates to the message that is about to be sent instead of arriving as an orphan.

**It does not talk to Slack.** The rows go back into the outbox and the relay's worker
delivers them, under the same lease as any other queued work. Three consequences:

- **The relay does not have to be stopped.** Running this against a store a relay is actively
  draining is the expected case, not a hazard: the re-queue is one transaction and the
  revived rows are picked up by whichever worker leases them next.
- **Nothing is delivered if no relay is running.** The operations sit in the outbox until one
  is. The command's output says so.
- **It does not run migrations.** If the schema is not there, the relay has never started
  against this store.

### Where to run it

The binary is already in the image, so `kubectl exec` into the pod is the whole procedure.
This is deliberate — the decision and its alternatives are in
[ADR 003 §5.2](../adr/003-hardening-divergences.md). Authorization is "who can exec into this
pod", which the cluster has already decided.

On SQLite the store is a file next to `storage.url`, so the replay has to run in the same
container as the relay. `kubectl exec` does. On PostgreSQL any process with the same
configuration reaches the same database.

### Output

```console
$ alertthread replay --channel '#alerts'
2 operations are parked in channel #alerts and never reached Slack.

  ID  OP    CHANNEL  FINGERPRINT  TRIES  PARKED                          LAST ERROR
  17  post  #alerts  9f2ab1c4     1      2026-07-31T09:14:02Z (2h4m ago)  chat.postMessage: Slack cannot deliver to that channel (channel_not_found)
  18  post  #alerts  c31d0a77     1      2026-07-31T09:14:03Z (2h4m ago)  chat.postMessage: Slack cannot deliver to that channel (channel_not_found)

DRY RUN — nothing has been changed.
Re-run with --commit to return 2 operation(s) to the outbox.
```

| Column | What it is |
|---|---|
| `ID` | The outbox row id |
| `OP` | The operation kind, the same closed set as `alertthread_outbox_depth{op}` |
| `CHANNEL` | The `channel` column, which is what `--channel` matches |
| `FINGERPRINT` | The `fingerprint` column, which is what `--fingerprint` matches. `-` for a storm-collapse summary, which belongs to a group rather than to one alert and is therefore never selected by `--fingerprint` |
| `TRIES` | Attempts spent before it was parked |
| `PARKED` | When it was given up on |
| `LAST ERROR` | The verbatim failure, elided at 120 characters |

An empty result is reported rather than being an error:

```console
$ alertthread replay --channel '#alrets'
Nothing is parked in channel #alrets. There is nothing to replay.
```

The listing shows at most 50 rows and reads at most 10 000; `--commit` acts on every matching
row regardless of either, and says so when a cap was reached.

### What it is invisible to

Two things a dashboard will not show, both because `replay` is a separate process from the
relay and carries no metrics registry (ROADMAP known open item 19):

- **`alertthread_dead_letter_revived_total` does not move.** That counter belongs to the
  relay's automatic sweep. After a replay, `alertthread_outbox_dead_lettered` falls with
  nothing accounting for the drop.
- **Nothing checks that a worker exists to deliver what was re-queued.** The command reports
  what it re-queued, not what was sent. `alertthread_outbox_depth` falling back to zero is
  the confirmation.
