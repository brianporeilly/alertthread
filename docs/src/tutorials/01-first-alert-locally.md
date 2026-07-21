# First alert, locally

*Status: written in Phase 4, once the walking skeleton is live.*

This tutorial will take you from a clean checkout to watching a real alert fire, thread and
resolve in a local fake Slack — with no Slack workspace and no Kubernetes cluster.

The path it will walk:

1. `just up` — start the compose stack (podman or docker, auto-detected).
2. Run `alertthread` against it.
3. Fire a real Prometheus rule, let Alertmanager group it and deliver the webhook.
4. Watch the message appear in the slack-mock UI.
5. Resolve the alert, and watch the original message turn green and grow a thread reply.

This is the page that has to work for a newcomer, so it is written last — after there is
something real to teach, and after the compose stack has been exercised enough to trust.

Until then, `explanation/build-and-packaging.md` describes how to build the binary and its
container image.
