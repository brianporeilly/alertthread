# Customize message templates

*Status: written in Phase 3, alongside the rendering layer.*

Message bodies are MiniJinja templates and can be overridden without forking the binary —
usually by mounting a ConfigMap. Four templates ship built in: `firing`, `resolved`,
`group_summary` and `thread_reply`.

This guide will cover:

- The variables each template receives.
- Overriding one template while keeping the built-in versions of the others.
- Previewing a template against a captured Alertmanager payload before deploying it.

⚠️ A broken template cannot take alerting down. Rendering is always wrapped in a
catch-and-fall-back that degrades to a hardcoded plain-text message and emits
`alertthread_fallback_posts_total`. Degraded output is acceptable; silence is not. If you
are seeing plain messages where you expect formatted ones, check that metric first.

Background on why rendering works this way is in [ADR 001 D10](../adr/001-adr.md).
