# alertthread

**Alertmanager → Slack relay with fingerprint-keyed threading and update-on-resolve.**

Alertmanager's built-in Slack receiver posts an independent message for every notification.
A firing alert and its later resolution are two unrelated messages in the channel, often
hours and hundreds of lines apart. `alertthread` sits between Alertmanager and Slack and
correlates them by alert fingerprint, so a resolution *updates* the original message and
threads under it instead of posting somewhere new.

> **This is alerting infrastructure. The worst possible bug is silence.** A duplicate
> message is a nuisance; a dropped alert is an outage nobody hears about. Every trade-off
> in this codebase resolves in that direction.

## These docs

They follow [Diátaxis](https://diataxis.fr/), which means each page answers exactly one
kind of question. If you are looking for something and cannot find it, the quadrant is the
fastest way to guess where it lives:

| If you want to… | Read |
|---|---|
| be taught, from scratch | **Tutorials** |
| accomplish a specific goal | **How-to guides** |
| look up an exact option or value | **Reference** |
| understand why it works this way | **Explanation** |

Architecture decisions are recorded in **ADRs**, which are append-only — superseded, never
rewritten.

## Project status

Under construction, following the phased plan in `ROADMAP.md`. Phase 0 (foundations) is
complete: the workspace, gates, CI and packaging are in place, and the musl/`scratch` build
is [validated and measured](explanation/build-and-packaging.md). The relay itself is not
implemented yet.
