{{/*
Expand the name of the chart.
*/}}
{{- define "ellmephant.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "ellmephant.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "ellmephant.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "ellmephant.labels" -}}
helm.sh/chart: {{ include "ellmephant.chart" . }}
{{ include "ellmephant.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "ellmephant.selectorLabels" -}}
app.kubernetes.io/name: {{ include "ellmephant.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "ellmephant.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "ellmephant.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
The weight-cache volume mounts at a subdirectory of the store, so the store must exist first.
`models.mountPath` is always `<store>/models` unless the cache is disabled.
*/}}
{{- define "ellmephant.storeMountPath" -}}
/data
{{- end }}

{{/*
Whether a separate weight-cache volume is in play (own PVC, an existing claim, or a hostPath).
*/}}
{{- define "ellmephant.models.enabled" -}}
{{- if or .Values.models.enabled .Values.models.existingClaim .Values.models.hostPath -}}true{{- end -}}
{{- end }}

{{/*
Name of the ConfigMap holding `config.toml` (created, or an existing one).
*/}}
{{- define "ellmephant.configMapName" -}}
{{- if .Values.config.existingConfigMap -}}
{{- .Values.config.existingConfigMap -}}
{{- else -}}
{{- printf "%s-config" (include "ellmephant.fullname" .) -}}
{{- end -}}
{{- end }}

{{/*
Whether a `config.toml` is mounted at all.
*/}}
{{- define "ellmephant.config.enabled" -}}
{{- if or .Values.config.create .Values.config.existingConfigMap -}}true{{- end -}}
{{- end }}

{{/*
The MCP container port, named so probes can reference it by name.
*/}}
{{- define "ellmephant.mcpPort" -}}{{ .Values.mcp.port | default .Values.service.port }}{{- end }}

{{/*
Environment shared by the app container and the init containers. Rendered as list items so the
caller controls indentation. Flags/env override `config.toml` (SPEC §12).
*/}}
{{- define "ellmephant.env" -}}
- name: ELLMEPHANT_HOME
  value: {{ include "ellmephant.storeMountPath" . | quote }}
- name: HOME
  value: /tmp
- name: RUST_LOG
  value: {{ .Values.logging.rustLog | quote }}
- name: ELLMEPHANT_MCP_HOST
  value: {{ .Values.mcp.host | quote }}
- name: ELLMEPHANT_MCP_PORT
  value: {{ include "ellmephant.mcpPort" . | quote }}
{{- if .Values.offline }}
- name: ELLMEPHANT_OFFLINE
  value: "true"
{{- end }}
{{- if .Values.huggingFaceToken.secretName }}
- name: HF_TOKEN
  valueFrom:
    secretKeyRef:
      name: {{ .Values.huggingFaceToken.secretName }}
      key: {{ .Values.huggingFaceToken.secretKey }}
      optional: true
- name: HUGGING_FACE_HUB_TOKEN
  valueFrom:
    secretKeyRef:
      name: {{ .Values.huggingFaceToken.secretName }}
      key: {{ .Values.huggingFaceToken.secretKey }}
      optional: true
{{- end }}
{{- with .Values.env }}
{{ toYaml . }}
{{- end }}
{{- end }}

{{/*
Volume mounts, shared by the app container and the init containers. `data` is listed first so the
nested `/data/models` mount lands on a directory that already exists.
*/}}
{{- define "ellmephant.volumeMounts" -}}
- name: data
  mountPath: {{ include "ellmephant.storeMountPath" . }}
{{- if eq (include "ellmephant.models.enabled" .) "true" }}
- name: models
  mountPath: {{ include "ellmephant.storeMountPath" . }}/models
  {{- if .Values.models.readOnly }}
  readOnly: true
  {{- end }}
{{- end }}
{{- if eq (include "ellmephant.config.enabled" .) "true" }}
- name: config
  mountPath: {{ include "ellmephant.storeMountPath" . }}/config.toml
  subPath: config.toml
  readOnly: true
{{- end }}
- name: tmp
  mountPath: /tmp
{{- end }}

{{/*
Volumes. `persistence.enabled=false` degrades to an emptyDir: the store is then immortal until
the pod restarts, and mortal after.
*/}}
{{- define "ellmephant.volumes" -}}
- name: data
{{- if .Values.persistence.enabled }}
  persistentVolumeClaim:
    claimName: {{ .Values.persistence.existingClaim | default (printf "%s-data" (include "ellmephant.fullname" .)) }}
{{- else }}
  emptyDir: {}
{{- end }}
{{- if eq (include "ellmephant.models.enabled" .) "true" }}
- name: models
{{- if .Values.models.existingClaim }}
  persistentVolumeClaim:
    claimName: {{ .Values.models.existingClaim }}
    readOnly: {{ .Values.models.readOnly }}
{{- else if .Values.models.hostPath }}
  hostPath:
    path: {{ .Values.models.hostPath }}
    type: Directory
{{- else }}
  persistentVolumeClaim:
    claimName: {{ include "ellmephant.fullname" . }}-models
    readOnly: {{ .Values.models.readOnly }}
{{- end }}
{{- end }}
{{- if eq (include "ellmephant.config.enabled" .) "true" }}
- name: config
  configMap:
    name: {{ include "ellmephant.configMapName" . }}
{{- end }}
- name: tmp
  emptyDir: {}
{{- end }}

{{/*
--- Gateway API ---------------------------------------------------------------------------

Ports the in-house `network/gateway/expose` module: an app-declared ListenerSet on a shared
Gateway plus an HTTPRoute, with the cert-manager cluster-issuer annotation on the listener and
external-dns annotation on the route. Cross-namespace ReferencesGrants are added when the
Gateway lives elsewhere. Call with:
  (dict "root" $ "cfg" <gateway block> "name" <listener name>
        "backendName" <service> "backendPort" <port> "labels" <labels>)
*/}}
{{- define "ellmephant.gateway.expose" -}}
{{- $g := .cfg -}}
{{- $ns := .root.Release.Namespace -}}
{{- $name := .name -}}
{{- $hostname := $g.hostname | default (printf "%s.%s" $name $g.domain) -}}
---
apiVersion: gateway.networking.k8s.io/v1
kind: ListenerSet
metadata:
  name: {{ $name }}
  namespace: {{ $ns }}
  labels:
    {{- .labels | nindent 4 }}
  {{- if and (eq $g.protocol "HTTPS") $g.certIssuer }}
  annotations:
    cert-manager.io/cluster-issuer: {{ $g.certIssuer | quote }}
  {{- end }}
spec:
  parentRef:
    name: {{ $g.gatewayName }}
    namespace: {{ $g.gatewayNamespace }}
  listeners:
    - name: {{ $name }}
      port: {{ $g.port }}
      protocol: {{ $g.protocol }}
      hostname: {{ $hostname | quote }}
      allowedRoutes:
        namespaces:
          from: All
      {{- if eq $g.protocol "HTTPS" }}
      tls:
        mode: Terminate
        certificateRefs:
          - name: cert-{{ $hostname }}
      {{- end }}
{{- if ne $ns $g.gatewayNamespace }}
---
apiVersion: gateway.networking.k8s.io/v1beta1
kind: ReferenceGrant
metadata:
  name: {{ $name }}-listenerset
  namespace: {{ $ns }}
  labels:
    {{- .labels | nindent 4 }}
spec:
  from:
    - group: gateway.networking.k8s.io
      kind: ListenerSet
      namespace: {{ $ns }}
  to:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: {{ $g.gatewayName }}
{{- end }}
{{- range $route := $g.httpRoutes }}
{{- $hosts := $route.hostnames | default (list $hostname) }}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: {{ $route.name | default $name }}
  namespace: {{ $ns }}
  labels:
    {{- $.labels | nindent 4 }}
  annotations:
    external-dns.alpha.kubernetes.io/hostname: {{ first $hosts | quote }}
    {{- with $route.annotations }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
spec:
  parentRefs:
    - group: gateway.networking.k8s.io
      kind: ListenerSet
      name: {{ $name }}
      namespace: {{ $ns }}
      sectionName: {{ $route.sectionName | default $name }}
  hostnames:
    {{- toYaml $hosts | nindent 4 }}
  rules:
    - backendRefs:
        - name: {{ $route.backendName | default $.backendName }}
          port: {{ $route.backendPort | default $.backendPort }}
      {{- with $route.filters }}
      filters:
        {{- toYaml . | nindent 8 }}
      {{- end }}
{{- end }}
{{- if ne $ns $g.gatewayNamespace }}
---
apiVersion: gateway.networking.k8s.io/v1beta1
kind: ReferenceGrant
metadata:
  name: {{ $name }}-httproute
  namespace: {{ $ns }}
  labels:
    {{- .labels | nindent 4 }}
spec:
  from:
    - group: gateway.networking.k8s.io
      kind: HTTPRoute
      namespace: {{ $ns }}
  to:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: {{ $g.gatewayName }}
{{- end }}
{{- end }}
