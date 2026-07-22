# Customize message templates

**Goal:** change the wording or layout of the Slack messages `alertthread` posts, without
forking the binary.

Four templates ship built in — `firing`, `resolved`, `group_summary` and `thread_reply` —
written in [MiniJinja], a Jinja2-compatible template language. You override any of them by
mounting your own version; the built-in stays in place for the ones you do not.

> A broken template cannot take alerting down. See [If it goes wrong](#if-it-goes-wrong).

[MiniJinja]: https://docs.rs/minijinja

---

## 1. Copy the built-in you want to change

The four sources live in
[`crates/slack/templates/`](https://github.com/brianporeilly/alertthread/tree/main/crates/slack/templates).
Start from one of them rather than from scratch — they are short, and starting from a
working template is how you find out what the variables are called.

## 2. Write your version

A template's whole job is to produce **Slack `mrkdwn` text**. Everything structural is
applied by the relay around whatever you produce:

| The relay does | You do not |
|---|---|
| Wraps the output in a coloured attachment (red firing, green resolved) | Choose a colour |
| Splits the output into Block Kit `section` blocks | Emit Block Kit JSON |
| Enforces Slack's 3000-character and 50-block limits | Count characters |
| Escapes every value before your template sees it | Escape anything |
| Builds the notification preview from your first line | Set `text` |

So a template is only ever about wording.

## 3. Mount it

Templates are read from a directory. Files are matched by name, with `.j2`, `.jinja` and
`.txt` extensions accepted and ignored:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: alertthread-templates
data:
  firing.j2: |
    *:fire: {{ alert.alertname }}* ({{ alert.severity }})
    {{ alert.summary }}
    _since {{ alert.started_at }}_
```

```yaml
# Deployment
volumeMounts:
  - name: templates
    mountPath: /etc/alertthread/templates
    readOnly: true
volumes:
  - name: templates
    configMap:
      name: alertthread-templates
```

Point `slack.template_dir` at that path. See
[reference/configuration.md](../reference/configuration.md).

A file whose name is not one of the four is **not loaded** and is reported at startup. That
is deliberate: silently ignoring `firing.md` would look exactly like a working override.

---

## The variables

### `firing`, `resolved`, `thread_reply`

All three receive one variable, `alert`. Every field is always present — never undefined —
so you never need a `default` filter for a field in this table.

| Field | Type | Notes |
|---|---|---|
| `alert.alertname` | string | The `alertname` label, or `(unnamed alert)` |
| `alert.severity` | string | The `severity` label, or empty |
| `alert.summary` | string | The `summary` annotation, or empty |
| `alert.description` | string | The `description` annotation, or empty |
| `alert.runbook_url` | string | The `runbook_url` annotation, or empty |
| `alert.fingerprint` | string | Alertmanager's identity for the alert |
| `alert.labels` | map | Every label. Use `{% for k, v in alert.labels\|items %}` |
| `alert.annotations` | map | Every annotation, likewise |
| `alert.generator_url` | string | Link back to the Prometheus rule, or empty |
| `alert.started_at` | string | `2026-07-21 14:02:00 UTC` |
| `alert.resolved_at` | string | Same format; empty while the alert is firing |
| `alert.duration` | string | `45s`, `29m 0s`, `12h 4m`, `3d 3h` |
| `alert.firing` | bool | `true` until the alert resolves |

Individual labels and annotations are reached through the maps —
`alert.labels.namespace`, `alert.annotations.dashboard` — and a key that is not there is
**undefined**, which is the one case where you do need a guard:

```jinja
{% if alert.labels.namespace %}namespace `{{ alert.labels.namespace }}`{% endif %}
```

### `group_summary`

Receives one variable, `group`, describing a storm-collapse parent.

| Field | Type | Notes |
|---|---|---|
| `group.firing` | int | Members still firing |
| `group.resolved` | int | Members that have resolved |
| `group.total` | int | `firing + resolved` |
| `group.all_resolved` | bool | `firing == 0` |
| `group.group_key` | string | Alertmanager's `groupKey` |
| `group.labels` | map | The labels Alertmanager grouped on, its `groupLabels` |
| `group.title` | string | A heading for the group. **Never empty** |

`group.labels` is Alertmanager's `groupLabels` — the labels its `group_by` grouped on, not
the full label set of any one alert. Reach individual ones through the map, guarding as
above:

```jinja
{% if group.labels.namespace %}in `{{ group.labels.namespace }}`{% endif %}
```

`group.title` is computed for you, and needs no guard. It is the first of these that is
non-empty:

1. `group.labels.alertname`, when `alertname` is one of the grouping labels;
2. otherwise the grouping labels rendered as `k=v`, space-separated, in key order —
   `namespace=rook-ceph severity=critical`;
3. otherwise `group.group_key`, which is what a `group_by: []` leaves.

Use it wherever you would have used an alert's `alertname`. Building your own heading from
`group.labels.alertname` alone means a blank heading for every group whose `group_by` does
not include `alertname`.

---

## Preview before deploying

```
cargo run -p alertthread -- render --template firing --payload testdata/firing.json
```

*(Available from Phase 4, alongside the binary's other subcommands.)*

Until then, the fastest loop is `cargo test -p alertthread-slack --test rendering`, whose
snapshots in `crates/slack/tests/snapshots/` show the exact JSON each template produces.

---

## Rules the relay applies to your output

**Values arrive already escaped.** `&`, `<` and `>` in a label or annotation are converted
to `&amp;`, `&lt;` and `&gt;` before your template runs. Do not escape them again, and do
not undo it: an annotation containing `<!channel>` would otherwise notify your entire
workspace, and annotations are written by whoever wrote the `PrometheusRule`.

You can still write Slack markup yourself, because your template's own text is not data:

```jinja
<{{ alert.generator_url }}|view in Prometheus>
```

**Whitespace is tidied.** Trailing spaces are removed from each line and runs of blank
lines are collapsed to one, so you do not have to put `{%-` and `-%}` on every control
block to avoid gaps in the message.

**Long output is split, then truncated.** Output over 3000 characters is split across
several `section` blocks, breaking at line ends. Output that would need more than 50 blocks
is cut, and a `:scissors:` line saying how much was lost is appended. Nothing about this is
silent, in the message or in the metrics.

---

## If it goes wrong

**A broken template cannot stop alerts being delivered.** ADR 001 D9: rendering is always
wrapped in a catch-and-fall-back. There are two failure points and neither is fatal.

| When | What happens | What you see |
|---|---|---|
| The template does not compile | The override is rejected; the built-in is used | An error logged at startup naming the template and the line |
| The template fails while rendering | A hardcoded minimal message is posted instead | `alertthread_fallback_posts_total{reason="render_failed"}` |
| The template renders to nothing | Same | `alertthread_fallback_posts_total{reason="empty_output"}` |

**If you are seeing plain messages where you expect formatted ones, check
`alertthread_fallback_posts_total` first,** then the relay's logs — the log line carries
MiniJinja's own error, including the line number.

The commonest cause is a typo in a variable name. Undefined names are an error rather than
an empty string, on purpose: a message quietly missing its alert name looks like it worked,
and nothing anywhere would record that it did not.

The commonest *second* cause is a template with no `else`:

```jinja
{% if alert.severity == "critical" %}...{% endif %}
```

That renders to nothing at all for every warning-level alert. The relay catches it and
posts the minimal message, which is why `empty_output` is a reason of its own.

---

Background — why rendering works this way, and why the deprecated attachment wrapper is
unavoidable for the colour bar — is in [ADR 001 D9 and D10](../adr/001-adr.md). The Slack
errors that can prevent a message being delivered *after* it renders are in
[reference/slack-errors.md](../reference/slack-errors.md).
