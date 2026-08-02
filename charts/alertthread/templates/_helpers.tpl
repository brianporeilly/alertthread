{{/*
Name helpers, standard shape.
*/}}
{{- define "alertthread.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "alertthread.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{- define "alertthread.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
`app.kubernetes.io/name` is load-bearing beyond convention: the ServiceMonitor's
jobLabel reads it off the Service to produce `job="alertthread"`, which is what
the shipped alert rules select on.
*/}}
{{- define "alertthread.selectorLabels" -}}
app.kubernetes.io/name: {{ include "alertthread.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "alertthread.labels" -}}
helm.sh/chart: {{ include "alertthread.chart" . }}
{{ include "alertthread.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/component: alert-relay
app.kubernetes.io/part-of: alertthread
{{- with .Values.commonLabels }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{- define "alertthread.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "alertthread.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "alertthread.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) -}}
{{- end -}}

{{/*
Whether this release keeps local state. SQLite does; PostgreSQL does not, and
its Deployment has no PVC, no state mount and a RollingUpdate strategy.
*/}}
{{- define "alertthread.isSqlite" -}}
{{- eq .Values.config.storage.backend "sqlite" | ternary "true" "" -}}
{{- end -}}

{{- define "alertthread.pvcName" -}}
{{- default (printf "%s-state" (include "alertthread.fullname" .)) .Values.persistence.existingClaim -}}
{{- end -}}

{{/* Secret names: an existing Secret if named, otherwise the one this chart creates. */}}
{{- define "alertthread.slackSecretName" -}}
{{- default (printf "%s-slack" (include "alertthread.fullname" .)) .Values.slack.existingSecret -}}
{{- end -}}

{{- define "alertthread.slackSecretKey" -}}
{{- if .Values.slack.existingSecret -}}
{{- .Values.slack.existingSecretKey -}}
{{- else -}}
token
{{- end -}}
{{- end -}}

{{- define "alertthread.webhookSecretName" -}}
{{- default (printf "%s-webhook" (include "alertthread.fullname" .)) .Values.webhookAuth.existingSecret -}}
{{- end -}}

{{- define "alertthread.webhookSecretKey" -}}
{{- if .Values.webhookAuth.existingSecret -}}
{{- .Values.webhookAuth.existingSecretKey -}}
{{- else -}}
token
{{- end -}}
{{- end -}}

{{/* Whether the chart has any message-template override to write. */}}
{{- define "alertthread.hasTemplates" -}}
{{- $found := "" -}}
{{- range $name, $body := .Values.templates -}}
{{- if $body -}}{{- $found = "true" -}}{{- end -}}
{{- end -}}
{{- $found -}}
{{- end -}}

{{/*
The relay's configuration file, as the ConfigMap holds it.
*/}}
{{- define "alertthread.configYaml" -}}
{{- $config := deepCopy .Values.config -}}
{{- $_ := set $config.slack "token_file" "/etc/alertthread/secrets/slack/token" -}}
{{- if .Values.postgres.existingSecret -}}
{{- $_ := unset $config.storage "url" -}}
{{- end -}}
{{- if .Values.webhookAuth.enabled -}}
{{- $_ := set $config.server "auth_token_file" "/etc/alertthread/secrets/webhook/token" -}}
{{- end -}}
{{- if include "alertthread.hasTemplates" . -}}
{{- $_ := set $config "templates" (dict "dir" "/etc/alertthread/templates") -}}
{{- end -}}
{{- toYaml $config -}}
{{- end -}}

{{/*
Preflight. Every check here is a mistake whose Kubernetes-side symptom points
nowhere near its cause, so the chart refuses to render instead.
*/}}
{{- define "alertthread.validate" -}}
{{- if not (has .Values.config.storage.backend (list "sqlite" "postgres")) -}}
{{- fail (printf "config.storage.backend must be sqlite or postgres, got %q" .Values.config.storage.backend) -}}
{{- end -}}

{{- if and (include "alertthread.isSqlite" .) (gt (int .Values.replicaCount) 1) -}}
{{- fail "replicaCount > 1 needs config.storage.backend=postgres: two processes on one SQLite file corrupts correlation state (ADR 001 D4). See docs/src/how-to/enable-ha-postgres.md" -}}
{{- end -}}

{{- if not .Values.config.slack.default_channel -}}
{{- fail "config.slack.default_channel is required: the relay refuses to start without somewhere to post when a webhook URL carries no ?channel= (ADR 001 D8)" -}}
{{- end -}}

{{- if and (not .Values.slack.existingSecret) (not .Values.slack.token) -}}
{{- fail "set slack.existingSecret (preferred) or slack.token: the relay has no degraded mode without a bot token" -}}
{{- end -}}

{{- if and .Values.webhookAuth.enabled (not .Values.webhookAuth.existingSecret) (not .Values.webhookAuth.token) -}}
{{- fail "webhookAuth.enabled needs webhookAuth.existingSecret or webhookAuth.token" -}}
{{- end -}}

{{- if and (eq .Values.config.storage.backend "postgres") (not .Values.postgres.existingSecret) (not (hasPrefix "postgres" .Values.config.storage.url)) -}}
{{- fail "config.storage.backend=postgres needs postgres.existingSecret, or a postgres:// URL in config.storage.url" -}}
{{- end -}}

{{- if include "alertthread.isSqlite" . -}}
{{- $path := .Values.config.storage.url | trimPrefix "sqlite://" -}}
{{- if not (hasPrefix (printf "%s/" .Values.persistence.mountPath) $path) -}}
{{- fail (printf "config.storage.url must be a sqlite:// path under persistence.mountPath (%s): the root filesystem is read-only, and the database, its -wal and its -shm all have to live on the same writable mount" .Values.persistence.mountPath) -}}
{{- end -}}
{{- end -}}
{{- end -}}
