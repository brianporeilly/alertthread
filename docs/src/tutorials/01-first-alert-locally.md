# First alert, locally

By the end of this page you will have watched a real alert fire, thread, and resolve itself
in a fake Slack — with no Slack workspace and no Kubernetes cluster. The whole thing runs on
your machine in containers and cleans up after itself.

You will run five alerts through a genuine Prometheus and a genuine Alertmanager into the
relay, see them collapse into one threaded summary, and then watch each one turn from red to
green in place.

## Before you start

You need three things on your `PATH`:

- **`git`**
- **`just`** — the task runner ([installation](https://github.com/casey/just#installation)).
- **A container engine** — `podman` or `docker`. `just` detects whichever you have; you do
  not tell it which.

Nothing else. You do not need Rust installed: everything runs in containers.

## 1. Get the code

```console
$ git clone https://github.com/brianporeilly/alertthread.git
$ cd alertthread
```

## 2. Start the demo stack

```console
$ just demo
```

The first run builds the relay image, which takes a couple of minutes. When it finishes it
prints the URLs you need:

```
Demo stack up (podman). Open the fake Slack and watch:
    Slack UI      http://localhost:8081
    Prometheus    http://localhost:9090/alerts
    Alertmanager  http://localhost:9093

Five alerts fire now and resolve themselves in ~60s. 'just demo-down' when done.
```

## 3. Watch the alerts fire and thread

Open **<http://localhost:8081>** in your browser. Within about ten seconds you will see a
single **red** message appear in `#alerts`:

> 🚨 **FIRING · 5 of 5 alerts** · demo

Under it, threaded as replies, are the five individual alerts — `DemoDiskFilling`,
`DemoMemoryPressure`, and the rest. They arrive one per second, so the thread fills in over a
few seconds.

This is storm-collapse: five alerts firing together become one summary with the detail
tucked into a thread, instead of five separate messages. The page reloads itself every few
seconds, so just leave it open.

> **Note.** The individual alerts are threaded *under* the summary, not posted to the
> channel. They appear as replies indented beneath it, not as five separate channel messages.

## 4. Watch them resolve in place

About a minute after the stack started, the demo alerts stop firing. Keep watching the same
page — you do not need to do anything. In a few seconds:

- The summary flips to **green**: ✅ **RESOLVED · 5 alerts**.
- Each threaded alert turns **green** and is marked *edited* — its original message was
  rewritten in place, so the channel history reads correctly tomorrow.
- A short **resolve reply** appears in the thread for each one.

Both things happen because an edit alone is silent — it does not notify anyone watching the
channel live — so the relay also posts the reply that makes the resolution noticeable. You
have just seen the whole point of the project: one message per alert, updated on resolve,
with the channel kept quiet.

## 5. Clean up

```console
$ just demo-down
```

That stops every container and removes its data. Your machine is back to a clean state.

## What next

- To run this same flow as a pass/fail check — the way CI does — use `just e2e`. It asserts
  the outcome and tears itself down.
- To point the relay at a real Slack workspace, see
  [Configuration](../reference/configuration.md).
- The demo stack already runs the relay the hardened way — read-only root filesystem, no
  capabilities, and a bearer token on the webhook that Alertmanager sends. To do the same in a
  cluster, see [Harden a deployment](../how-to/harden-a-deployment.md).
- To understand *why* resolve does both an edit and a reply, and why alerts collapse into
  threads, see [Failure semantics](../explanation/failure-semantics.md) and
  [Fingerprint correlation](../explanation/fingerprint-correlation.md).
