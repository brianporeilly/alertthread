# Configuration

*Status: populated per-phase, as options are added.*

Configuration is loaded by `figment`: a YAML file, layered with environment-variable
overrides.

**A new config option is not merged until it appears on this page.** That rule is in
AGENTS.md, and this page is the reason it exists.

The tables below will document, for every option: its key, its environment-variable
equivalent, its type, its default, and what it does.

Planned sections, in the phase that fills them:

| Section | Phase |
|---|---|
| `storage.*` — backend selection, DSN, migrations, retention | 2 |
| `slack.*` — token, default channel, rate limiting | 3 |
| `templates.*` — overrides | 3 |
| `server.*` — bind address, timeouts, optional bearer token | 4 |
| `resolve.*` — `update_in_place`, `thread_reply` | 4 |
| `collapse.*` — `collapse_threshold` | 4 |

⚠️ The bot token is read from an environment variable or a file and is never logged. The
config type carries a redacting `Debug` implementation.
