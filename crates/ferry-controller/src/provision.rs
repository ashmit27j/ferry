//! Proxy infrastructure for each Gateway (GEP-1762, ADR 0035).
//!
//! Every accepted Gateway gets its own proxy Deployment, Service,
//! PodDisruptionBudget and (optionally) HorizontalPodAutoscaler in the
//! controller's namespace, rendered from a [`ProxyTemplate`] that the Helm
//! chart writes from its values. Keeping them next to the controller means
//! its write access to Deployments and Services is a namespaced Role, and
//! the proxies' client certificate Secret never leaves that namespace.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{LocalObjectReference, ResourceRequirements, Toleration};
use k8s_openapi::api::networking::v1::NetworkPolicyPeer;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::translate::{ADMIN_PORT, ProxyPort};
use crate::world::ObjKey;

/// Label naming the component (`ferry-proxy`).
pub const NAME_LABEL: &str = "app.kubernetes.io/name";
/// Label naming one Gateway's proxy fleet.
pub const INSTANCE_LABEL: &str = "app.kubernetes.io/instance";
/// Label marking objects this controller manages.
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
/// Value of [`MANAGED_BY_LABEL`].
pub const MANAGED_BY: &str = "ferry-controller";
/// Annotation holding the Gateway's `namespace/name`.
pub const GATEWAY_ANNOTATION: &str = "gateway.ferry.dev/gateway";
/// The Gateway API's recommended label for generated resources.
pub const GATEWAY_NAME_LABEL: &str = "gateway.networking.k8s.io/gateway-name";

/// How proxies are deployed; written by the Helm chart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct ProxyTemplate {
    /// Proxy image, e.g. `ghcr.io/ashmit27j/ferry:0.1.0`.
    pub image: String,
    /// `IfNotPresent`, `Always` or `Never`.
    pub image_pull_policy: Option<String>,
    /// Pull secrets.
    pub image_pull_secrets: Vec<LocalObjectReference>,
    /// Replicas when autoscaling is off.
    pub replicas: i32,
    /// Horizontal autoscaling.
    pub autoscaling: Option<Autoscaling>,
    /// PodDisruptionBudget `maxUnavailable`; `None` creates none.
    pub max_unavailable: Option<i32>,
    /// Container resources.
    pub resources: Option<ResourceRequirements>,
    /// Pod annotations (e.g. for Prometheus).
    pub pod_annotations: BTreeMap<String, String>,
    /// Node selector.
    pub node_selector: BTreeMap<String, String>,
    /// Tolerations.
    pub tolerations: Vec<Toleration>,
    /// Spread replicas across zones and nodes (best effort).
    pub topology_spread: bool,
    /// The proxy Service.
    pub service: ServiceTemplate,
    /// Proxies expect PROXY protocol v2 (an NLB with it enabled in front).
    pub proxy_protocol: bool,
    /// `--drain-delay-secs`: cover the load balancer's deregistration delay.
    pub drain_delay_secs: u64,
    /// `--drain-timeout-secs`.
    pub drain_timeout_secs: u64,
    /// `FERRY_LOG`.
    pub log_level: String,
    /// `--access-log`: `stdout`, `stderr` or `off`.
    pub access_log: String,
    /// `--otlp-endpoint`, if traces are exported.
    pub otlp_endpoint: Option<String>,
    /// The controller's snapshot endpoint.
    pub controller_url: String,
    /// Name in the controller's certificate.
    pub controller_server_name: String,
    /// Secret with the proxies' client certificate (`ca.crt`, `tls.crt`, `tls.key`).
    pub tls_secret: Option<String>,
    /// Service account of proxy pods (they need no API access).
    pub service_account: String,
    /// A NetworkPolicy per fleet; `None` creates none.
    pub network_policy: Option<NetworkPolicyTemplate>,
}

/// The fleet's NetworkPolicy: listener ports stay open to every source (the
/// NLB sends clients straight to pod IPs); the admin port only admits
/// `metrics_from`. Egress is left open, since proxies reach arbitrary
/// backends and the controller.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct NetworkPolicyTemplate {
    /// Peers allowed to scrape the admin port; empty admits none (kubelet
    /// probes are not subject to NetworkPolicy).
    pub metrics_from: Vec<NetworkPolicyPeer>,
}

/// HPA settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Autoscaling {
    /// Floor.
    pub min_replicas: i32,
    /// Ceiling.
    pub max_replicas: i32,
    /// Target average CPU utilisation, percent of requests.
    pub target_cpu_utilization: i32,
}

/// The proxy Service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct ServiceTemplate {
    /// `LoadBalancer` (default) or `ClusterIP`.
    #[serde(rename = "type")]
    pub type_: String,
    /// Annotations, e.g. the AWS Load Balancer Controller's NLB settings.
    pub annotations: BTreeMap<String, String>,
    /// `Cluster` or `Local`.
    pub external_traffic_policy: Option<String>,
    /// `spec.loadBalancerClass` (e.g. `service.k8s.aws/nlb`).
    pub load_balancer_class: Option<String>,
}

impl Default for ServiceTemplate {
    fn default() -> Self {
        Self {
            type_: "LoadBalancer".into(),
            annotations: BTreeMap::new(),
            external_traffic_policy: None,
            load_balancer_class: None,
        }
    }
}

impl Default for ProxyTemplate {
    fn default() -> Self {
        Self {
            image: "ghcr.io/ashmit27j/ferry:latest".into(),
            image_pull_policy: None,
            image_pull_secrets: Vec::new(),
            replicas: 3,
            autoscaling: None,
            max_unavailable: Some(1),
            resources: None,
            pod_annotations: BTreeMap::new(),
            node_selector: BTreeMap::new(),
            tolerations: Vec::new(),
            topology_spread: true,
            service: ServiceTemplate::default(),
            proxy_protocol: false,
            drain_delay_secs: 15,
            drain_timeout_secs: 30,
            log_level: "info".into(),
            access_log: "off".into(),
            otlp_endpoint: None,
            controller_url: "https://ferry-controller.ferry-system.svc:18000".into(),
            controller_server_name: "ferry-controller.ferry-system.svc".into(),
            tls_secret: Some("ferry-proxy-tls".into()),
            service_account: "ferry-proxy".into(),
            network_policy: None,
        }
    }
}

/// The name of a Gateway's proxy resources: `ferry-<namespace>-<name>`,
/// shortened with a hash to fit a DNS label.
#[must_use]
pub fn resource_name(gateway: &ObjKey) -> String {
    let full = format!("ferry-{}-{}", gateway.namespace, gateway.name).replace('.', "-");
    if full.len() <= 63 {
        return full;
    }
    let hash = twox_hash::XxHash64::oneshot(0, gateway.to_string().as_bytes());
    let head = full.get(..54).unwrap_or(&full).trim_end_matches('-');
    format!("{head}-{:08x}", hash & 0xffff_ffff)
}

/// Every object of one Gateway's fleet, as server-side-apply documents. The
/// Service is omitted when the Gateway has no servable listener.
#[must_use]
pub fn render(
    gateway: &ObjKey,
    ports: &[ProxyPort],
    t: &ProxyTemplate,
    namespace: &str,
) -> Vec<Value> {
    let name = resource_name(gateway);
    let mut labels = json!({
        NAME_LABEL: "ferry-proxy",
        INSTANCE_LABEL: name,
        MANAGED_BY_LABEL: MANAGED_BY,
    });
    if gateway.name.len() <= 63 {
        labels[GATEWAY_NAME_LABEL] = json!(gateway.name);
    }
    let selector = json!({ NAME_LABEL: "ferry-proxy", INSTANCE_LABEL: name });
    let annotations = json!({ GATEWAY_ANNOTATION: gateway.to_string() });
    let meta = |kind_annotations: Value| {
        let mut a = annotations.clone();
        if let (Some(a), Some(extra)) = (a.as_object_mut(), kind_annotations.as_object()) {
            a.extend(extra.clone());
        }
        json!({ "name": name, "namespace": namespace, "labels": labels, "annotations": a })
    };
    let mut out = vec![deployment(
        gateway,
        ports,
        t,
        meta(json!({})),
        &labels,
        &selector,
    )];
    if !ports.is_empty() {
        let mut spec = json!({
            "type": t.service.type_,
            "selector": selector,
            "ports": ports.iter().map(|p| json!({
                "name": p.name,
                "port": p.port,
                "targetPort": p.container_port,
                "protocol": "TCP",
            })).collect::<Vec<_>>(),
        });
        if let Some(p) = &t.service.external_traffic_policy {
            spec["externalTrafficPolicy"] = json!(p);
        }
        if let Some(c) = &t.service.load_balancer_class {
            spec["loadBalancerClass"] = json!(c);
        }
        out.push(json!({
            "apiVersion": "v1",
            "kind": "Service",
            "metadata": meta(json!(t.service.annotations)),
            "spec": spec,
        }));
    }
    if let Some(max) = t.max_unavailable {
        out.push(json!({
            "apiVersion": "policy/v1",
            "kind": "PodDisruptionBudget",
            "metadata": meta(json!({})),
            "spec": { "maxUnavailable": max, "selector": { "matchLabels": selector } },
        }));
    }
    if let Some(a) = &t.autoscaling {
        out.push(json!({
            "apiVersion": "autoscaling/v2",
            "kind": "HorizontalPodAutoscaler",
            "metadata": meta(json!({})),
            "spec": {
                "scaleTargetRef": { "apiVersion": "apps/v1", "kind": "Deployment", "name": name },
                "minReplicas": a.min_replicas,
                "maxReplicas": a.max_replicas,
                "metrics": [{
                    "type": "Resource",
                    "resource": {
                        "name": "cpu",
                        "target": { "type": "Utilization", "averageUtilization": a.target_cpu_utilization },
                    },
                }],
            },
        }));
    }
    if let Some(np) = &t.network_policy {
        let mut ingress = Vec::new();
        if !ports.is_empty() {
            ingress.push(json!({
                "ports": ports.iter().map(|p| json!({ "port": p.container_port, "protocol": "TCP" })).collect::<Vec<_>>(),
            }));
        }
        if !np.metrics_from.is_empty() {
            ingress.push(json!({
                "from": np.metrics_from,
                "ports": [{ "port": ADMIN_PORT, "protocol": "TCP" }],
            }));
        }
        out.push(json!({
            "apiVersion": "networking.k8s.io/v1",
            "kind": "NetworkPolicy",
            "metadata": meta(json!({})),
            "spec": {
                "podSelector": { "matchLabels": selector },
                "policyTypes": ["Ingress"],
                "ingress": ingress,
            },
        }));
    }
    out
}

fn deployment(
    gateway: &ObjKey,
    ports: &[ProxyPort],
    t: &ProxyTemplate,
    metadata: Value,
    labels: &Value,
    selector: &Value,
) -> Value {
    let mut args = vec![
        "proxy".to_owned(),
        "--controller".into(),
        t.controller_url.clone(),
        "--gateway".into(),
        gateway.to_string(),
        "--admin-address".into(),
        format!("0.0.0.0:{ADMIN_PORT}"),
        "--drain-delay-secs".into(),
        t.drain_delay_secs.to_string(),
        "--drain-timeout-secs".into(),
        t.drain_timeout_secs.to_string(),
        "--access-log".into(),
        t.access_log.clone(),
    ];
    if let Some(endpoint) = &t.otlp_endpoint {
        args.extend(["--otlp-endpoint".into(), endpoint.clone()]);
    }
    let mut volumes = Vec::new();
    let mut mounts = Vec::new();
    if let Some(secret) = &t.tls_secret {
        args.extend([
            "--controller-tls-dir".into(),
            "/etc/ferry/tls".into(),
            "--controller-server-name".into(),
            t.controller_server_name.clone(),
        ]);
        volumes.push(json!({ "name": "tls", "secret": { "secretName": secret } }));
        mounts.push(json!({ "name": "tls", "mountPath": "/etc/ferry/tls", "readOnly": true }));
    }
    let mut container_ports: Vec<Value> = ports
        .iter()
        .map(|p| json!({ "name": p.name, "containerPort": p.container_port, "protocol": "TCP" }))
        .collect();
    container_ports
        .push(json!({ "name": "admin", "containerPort": ADMIN_PORT, "protocol": "TCP" }));
    let probe = |path: &str| {
        json!({
            "httpGet": { "path": path, "port": ADMIN_PORT },
            "periodSeconds": 5,
            "failureThreshold": 3,
        })
    };
    let mut container = json!({
        "name": "proxy",
        "image": t.image,
        "args": args,
        "env": [
            { "name": "POD_NAME", "valueFrom": { "fieldRef": { "fieldPath": "metadata.name" } } },
            { "name": "FERRY_LOG", "value": t.log_level },
            { "name": "FERRY_LOG_FORMAT", "value": "json" },
        ],
        "ports": container_ports,
        "readinessProbe": probe("/ready"),
        "livenessProbe": probe("/live"),
        "volumeMounts": mounts,
        "securityContext": {
            "allowPrivilegeEscalation": false,
            "readOnlyRootFilesystem": true,
            "runAsNonRoot": true,
            "capabilities": { "drop": ["ALL"] },
        },
    });
    if let Some(p) = &t.image_pull_policy {
        container["imagePullPolicy"] = json!(p);
    }
    if let Some(r) = &t.resources {
        container["resources"] = json!(r);
    }
    let mut pod_spec = json!({
        "serviceAccountName": t.service_account,
        "automountServiceAccountToken": false,
        "terminationGracePeriodSeconds": t.drain_delay_secs + t.drain_timeout_secs + 5,
        "securityContext": {
            "runAsNonRoot": true,
            "runAsUser": 65_532,
            "runAsGroup": 65_532,
            "seccompProfile": { "type": "RuntimeDefault" },
        },
        "containers": [container],
        "volumes": volumes,
    });
    if !t.image_pull_secrets.is_empty() {
        pod_spec["imagePullSecrets"] = json!(t.image_pull_secrets);
    }
    if !t.node_selector.is_empty() {
        pod_spec["nodeSelector"] = json!(t.node_selector);
    }
    if !t.tolerations.is_empty() {
        pod_spec["tolerations"] = json!(t.tolerations);
    }
    if t.topology_spread {
        pod_spec["topologySpreadConstraints"] =
            ["topology.kubernetes.io/zone", "kubernetes.io/hostname"]
                .iter()
                .map(|key| {
                    json!({
                        "maxSkew": 1,
                        "topologyKey": key,
                        "whenUnsatisfiable": "ScheduleAnyway",
                        "labelSelector": { "matchLabels": selector },
                    })
                })
                .collect();
    }
    let mut spec = json!({
        "selector": { "matchLabels": selector },
        // Never take a ready proxy away before its replacement is ready.
        "strategy": { "type": "RollingUpdate", "rollingUpdate": { "maxUnavailable": 0, "maxSurge": 1 } },
        "template": {
            "metadata": { "labels": labels, "annotations": t.pod_annotations },
            "spec": pod_spec,
        },
    });
    // With an HPA, replicas belong to it; setting them here would fight it.
    if t.autoscaling.is_none() {
        spec["replicas"] = json!(t.replicas);
    }
    json!({ "apiVersion": "apps/v1", "kind": "Deployment", "metadata": metadata, "spec": spec })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports() -> Vec<ProxyPort> {
        vec![ProxyPort {
            name: "http-80".into(),
            port: 80,
            container_port: 10_080,
        }]
    }

    #[test]
    fn names_fit_a_dns_label() {
        assert_eq!(
            resource_name(&ObjKey::new("infra", "web")),
            "ferry-infra-web"
        );
        let long = resource_name(&ObjKey::new("a".repeat(40), "b.".repeat(30)));
        assert!(long.len() <= 63, "{long}");
        assert!(
            long.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        );
        assert_ne!(
            long,
            resource_name(&ObjKey::new("a".repeat(40), "c.".repeat(30)))
        );
    }

    #[test]
    fn fleet_has_deployment_service_and_pdb() {
        let docs = render(
            &ObjKey::new("infra", "web"),
            &ports(),
            &ProxyTemplate::default(),
            "ferry-system",
        );
        let kinds: Vec<&str> = docs.iter().filter_map(|d| d["kind"].as_str()).collect();
        assert_eq!(kinds, ["Deployment", "Service", "PodDisruptionBudget"]);
        let d = &docs[0];
        assert_eq!(d["spec"]["replicas"], 3);
        let c = &d["spec"]["template"]["spec"]["containers"][0];
        assert_eq!(c["securityContext"]["readOnlyRootFilesystem"], true);
        assert_eq!(c["ports"][0]["containerPort"], 10_080);
        let args: Vec<&str> = c["args"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(args.windows(2).any(|w| w == ["--gateway", "infra/web"]));
        let svc = &docs[1];
        assert_eq!(svc["spec"]["type"], "LoadBalancer");
        assert_eq!(svc["spec"]["ports"][0]["targetPort"], 10_080);
        assert_eq!(
            svc["metadata"]["annotations"][GATEWAY_ANNOTATION],
            "infra/web"
        );
    }

    #[test]
    fn autoscaling_owns_replicas_and_no_ports_means_no_service() {
        let t = ProxyTemplate {
            autoscaling: Some(Autoscaling {
                min_replicas: 3,
                max_replicas: 10,
                target_cpu_utilization: 70,
            }),
            ..ProxyTemplate::default()
        };
        let docs = render(&ObjKey::new("infra", "web"), &[], &t, "ferry-system");
        let kinds: Vec<&str> = docs.iter().filter_map(|d| d["kind"].as_str()).collect();
        assert_eq!(
            kinds,
            [
                "Deployment",
                "PodDisruptionBudget",
                "HorizontalPodAutoscaler"
            ]
        );
        assert!(docs[0]["spec"].get("replicas").is_none());
    }

    #[test]
    fn network_policy_opens_listeners_and_limits_the_admin_port() {
        let t: ProxyTemplate = serde_saphyr::from_str(
            "networkPolicy:
  metricsFrom:
    - namespaceSelector: { matchLabels: { kubernetes.io/metadata.name: monitoring } }
",
        )
        .unwrap();
        let docs = render(&ObjKey::new("infra", "web"), &ports(), &t, "ferry-system");
        let np = docs.iter().find(|d| d["kind"] == "NetworkPolicy").unwrap();
        assert_eq!(np["metadata"]["name"], "ferry-infra-web");
        assert_eq!(
            np["spec"]["podSelector"]["matchLabels"][NAME_LABEL],
            "ferry-proxy"
        );
        let ingress = np["spec"]["ingress"].as_array().unwrap();
        assert!(
            ingress[0].get("from").is_none(),
            "listeners admit every source"
        );
        assert_eq!(ingress[0]["ports"][0]["port"], 10_080);
        assert_eq!(
            ingress[1]["from"][0]["namespaceSelector"]["matchLabels"]["kubernetes.io/metadata.name"],
            "monitoring"
        );
        assert_eq!(ingress[1]["ports"][0]["port"], ADMIN_PORT);

        // Without ports or scrapers, the pods admit nothing but probes.
        let closed = ProxyTemplate {
            network_policy: Some(NetworkPolicyTemplate::default()),
            ..ProxyTemplate::default()
        };
        let docs = render(&ObjKey::new("infra", "web"), &[], &closed, "ferry-system");
        let np = docs.iter().find(|d| d["kind"] == "NetworkPolicy").unwrap();
        assert_eq!(np["spec"]["ingress"], json!([]));
        assert!(
            render(
                &ObjKey::new("infra", "web"),
                &ports(),
                &ProxyTemplate::default(),
                "ns"
            )
            .iter()
            .all(|d| d["kind"] != "NetworkPolicy")
        );
    }

    #[test]
    fn helm_values_shape_parses() {
        let t: ProxyTemplate = serde_saphyr::from_str(
            r"
image: ferry:dev
replicas: 1
service:
  type: ClusterIP
  annotations: { service.beta.kubernetes.io/aws-load-balancer-type: external }
resources: { requests: { cpu: 100m, memory: 64Mi } }
tolerations: [{ key: dedicated, operator: Exists }]
",
        )
        .unwrap();
        assert_eq!(t.replicas, 1);
        assert_eq!(t.service.type_, "ClusterIP");
        assert_eq!(t.drain_timeout_secs, 30, "unset fields keep their defaults");
    }
}
