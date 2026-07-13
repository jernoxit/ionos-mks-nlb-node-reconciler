{{- define "reconciler.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "reconciler.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s" (include "reconciler.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "reconciler.labels" -}}
app.kubernetes.io/name: {{ include "reconciler.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- with .Chart.AppVersion }}
app.kubernetes.io/version: {{ . | quote }}
{{- end }}
{{- end -}}

{{- define "reconciler.selectorLabels" -}}
app.kubernetes.io/name: {{ include "reconciler.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "reconciler.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "reconciler.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/* The Secret name holding the IONOS token, and the key within it. */}}
{{- define "reconciler.tokenSecretName" -}}
{{- if .Values.token.existingSecret -}}
{{- .Values.token.existingSecret -}}
{{- else -}}
{{- printf "%s-token" (include "reconciler.fullname" .) -}}
{{- end -}}
{{- end -}}

{{- define "reconciler.tokenSecretKey" -}}
{{- default "token" .Values.token.existingSecretKey -}}
{{- end -}}
