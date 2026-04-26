{{/*
Common labels and naming helpers.
*/}}
{{- define "omp.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "omp.fullname" -}}
{{- printf "%s-%s" .Release.Name (include "omp.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "omp.labels" -}}
app.kubernetes.io/name: {{ include "omp.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "omp.image" -}}
{{ .Values.image.repository }}:{{ .Values.image.tag }}
{{- end -}}
