{{/* Resource name prefix: the release name, unless it already says ferry. */}}
{{- define "ferry.fullname" -}}
{{- if contains "ferry" .Release.Name -}}
{{- .Release.Name | trunc 40 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-ferry" .Release.Name | trunc 40 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "ferry.controller" -}}{{ include "ferry.fullname" . }}-controller{{- end -}}
{{- define "ferry.proxy" -}}{{ include "ferry.fullname" . }}-proxy{{- end -}}
{{- define "ferry.certgen" -}}{{ include "ferry.fullname" . }}-certgen{{- end -}}
{{- define "ferry.controllerTLS" -}}{{ include "ferry.controller" . }}-tls{{- end -}}
{{- define "ferry.proxyTLS" -}}{{ include "ferry.proxy" . }}-tls{{- end -}}

{{/* The name proxies verify in the controller's certificate. */}}
{{- define "ferry.controllerHost" -}}{{ include "ferry.controller" . }}.{{ .Release.Namespace }}.svc{{- end -}}

{{- define "ferry.image" -}}{{ .Values.image.repository }}:{{ .Values.image.tag | default .Chart.AppVersion }}{{- end -}}

{{- define "ferry.labels" -}}
app.kubernetes.io/part-of: ferry
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
{{- end -}}

{{/* Pod and container security shared by every Ferry workload. */}}
{{- define "ferry.podSecurity" -}}
runAsNonRoot: true
runAsUser: 65532
runAsGroup: 65532
seccompProfile:
  type: RuntimeDefault
{{- end -}}
{{- define "ferry.containerSecurity" -}}
allowPrivilegeEscalation: false
readOnlyRootFilesystem: true
runAsNonRoot: true
capabilities:
  drop: ["ALL"]
{{- end -}}

{{/* Proxy Service annotations: the AWS NLB set, then the extra ones. */}}
{{- define "ferry.serviceAnnotations" -}}
{{- $svc := .Values.proxy.service -}}
{{- $ann := dict -}}
{{- if $svc.awsNlb.enabled -}}
{{- $_ := set $ann "service.beta.kubernetes.io/aws-load-balancer-type" "external" -}}
{{- $_ := set $ann "service.beta.kubernetes.io/aws-load-balancer-nlb-target-type" "ip" -}}
{{- $_ := set $ann "service.beta.kubernetes.io/aws-load-balancer-scheme" $svc.awsNlb.scheme -}}
{{- $_ := set $ann "service.beta.kubernetes.io/aws-load-balancer-target-group-attributes" (printf "deregistration_delay.timeout_seconds=%v,preserve_client_ip.enabled=false" $svc.awsNlb.deregistrationDelaySeconds) -}}
{{- if .Values.proxy.proxyProtocol -}}
{{- $_ := set $ann "service.beta.kubernetes.io/aws-load-balancer-proxy-protocol" "*" -}}
{{- end -}}
{{- end -}}
{{- toJson (merge ($svc.annotations | default dict) $ann) -}}
{{- end -}}
