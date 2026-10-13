"""Generates the Ferry Grafana dashboard.

Usage: python deploy/grafana/generate.py deploy/grafana/ferry-dashboard.json

Edit panels here, regenerate, and run `cargo nextest run --test dashboard`,
which checks that every metric the dashboard queries is exported.
"""
import json, sys

DS = {"type": "prometheus", "uid": "${datasource}"}
RI = "$__rate_interval"
L = 'job=~"$job",listener=~"$listener"'
R = 'route=~"$route"'
C = 'job=~"$job",cluster=~"$cluster"'

panels = []
_id = [0]
y = [0]


def nid():
    _id[0] += 1
    return _id[0]


def row(title, collapsed=False):
    panels.append({"type": "row", "title": title, "id": nid(), "collapsed": collapsed,
                   "gridPos": {"h": 1, "w": 24, "x": 0, "y": y[0]}, "panels": []})
    y[0] += 1


def target(expr, legend="", ref="A", instant=False):
    t = {"datasource": DS, "expr": expr, "legendFormat": legend, "refId": ref}
    if instant:
        t["instant"] = True
        t["range"] = False
    return t


def stat(title, desc, exprs, unit, thresholds, x, w=4, h=5, decimals=None, mode="absolute"):
    steps = [{"color": c, "value": v} for c, v in thresholds]
    fc = {"defaults": {"unit": unit, "thresholds": {"mode": mode, "steps": steps},
                       "color": {"mode": "thresholds"}}, "overrides": []}
    if decimals is not None:
        fc["defaults"]["decimals"] = decimals
    panels.append({
        "type": "stat", "title": title, "description": desc, "id": nid(), "datasource": DS,
        "gridPos": {"h": h, "w": w, "x": x, "y": y[0]},
        "targets": [target(e, l, chr(65 + i)) for i, (e, l) in enumerate(exprs)],
        "fieldConfig": fc,
        "options": {"reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False},
                    "colorMode": "background", "graphMode": "area", "justifyMode": "auto",
                    "textMode": "auto", "orientation": "auto"},
    })


def ts(title, desc, exprs, unit, x, w=12, h=8, stack=False, thresholds=None, min0=True):
    defaults = {"unit": unit, "custom": {"lineWidth": 1, "fillOpacity": 10 if not stack else 60,
                                         "stacking": {"mode": "normal" if stack else "none"},
                                         "showPoints": "never"},
                "color": {"mode": "palette-classic"}}
    if min0:
        defaults["min"] = 0
    if thresholds:
        defaults["thresholds"] = {"mode": "absolute",
                                  "steps": [{"color": c, "value": v} for c, v in thresholds]}
        defaults["custom"]["thresholdsStyle"] = {"mode": "dashed"}
    panels.append({
        "type": "timeseries", "title": title, "description": desc, "id": nid(), "datasource": DS,
        "gridPos": {"h": h, "w": w, "x": x, "y": y[0]},
        "targets": [target(e, l, chr(65 + i)) for i, (e, l) in enumerate(exprs)],
        "fieldConfig": {"defaults": defaults, "overrides": []},
        "options": {"legend": {"displayMode": "table", "placement": "bottom",
                               "calcs": ["lastNotNull", "max"]},
                    "tooltip": {"mode": "multi", "sort": "desc"}},
    })


def advance(h):
    y[0] += h


def q(p, metric, sel, by=""):
    by_clause = f"le{', ' + by if by else ''}"
    return f'histogram_quantile({p}, sum by ({by_clause}) (rate({metric}_bucket{{{sel}}}[{RI}])))'


# ---------------------------------------------------------------- SLOs
row("Service level objectives")
stat("Added latency p50",
     "SLO: under 0.2 ms. Latency Ferry itself adds: time to response headers minus time spent waiting on upstreams (ferry_request_overhead_seconds).",
     [(q(0.5, "ferry_request_overhead_seconds", L), "p50")], "s",
     [("green", None), ("red", 0.0002)], 0)
stat("Added latency p99",
     "SLO: under 1 ms at 10k req/s per core.",
     [(q(0.99, "ferry_request_overhead_seconds", L), "p99")], "s",
     [("green", None), ("red", 0.001)], 4)
stat("Requests per core-second",
     "Throughput per CPU core actually used: request rate divided by CPU seconds per second. The latency SLO applies at 10k.",
     [(f'sum(rate(ferry_requests_total{{{L}}}[{RI}])) / sum(rate(process_cpu_seconds_total{{job=~"$job"}}[{RI}]))', "req/s per core")],
     "short", [("blue", None)], 8)
stat("Success ratio",
     "SLO: 99.95% of requests succeed (non-5xx) while backends are healthy. Compare with the healthy-endpoint panels below.",
     [(f'sum(rate(ferry_requests_total{{{L},code_class!="5xx"}}[{RI}])) / sum(rate(ferry_requests_total{{{L}}}[{RI}]))', "success")],
     "percentunit", [("red", None), ("green", 0.9995)], 12, decimals=3)
stat("Failed or cut-off requests (range)",
     "SLO: zero dropped requests during reloads and rolling restarts. 5xx answers plus responses not delivered in full, over the dashboard range. Read with the reload and restart counts beside it and the timeline below.",
     [(f'sum(increase(ferry_requests_total{{{L},code_class="5xx"}}[$__range])) + sum(increase(ferry_downstream_incomplete_responses_total{{{L}}}[$__range]))', "dropped")],
     "short", [("green", None), ("red", 1)], 16, decimals=0)
stat("Resident memory growth",
     "SLO: flat memory over a 72-hour soak. Slope of resident memory per hour over the last 6 hours, per process.",
     [(f'max(deriv(process_resident_memory_bytes{{job=~"$job"}}[6h])) * 3600', "per hour")],
     "bytes", [("green", None), ("orange", 1048576), ("red", 8388608)], 20)
advance(5)
ts("Added latency (Ferry overhead)",
   "Quantiles of ferry_request_overhead_seconds. Dashed lines: 0.2 ms (p50 SLO) and 1 ms (p99 SLO).",
   [(q(0.5, "ferry_request_overhead_seconds", L), "p50"),
    (q(0.9, "ferry_request_overhead_seconds", L), "p90"),
    (q(0.99, "ferry_request_overhead_seconds", L), "p99"),
    (q(0.999, "ferry_request_overhead_seconds", L), "p99.9")],
   "s", 0, w=8, thresholds=[("green", None), ("orange", 0.0002), ("red", 0.001)])
ts("Reloads, restarts and failures",
   "Config reloads and proxy starts against 5xx answers and cut-off responses. A zero-drop rollout shows reload and start markers with flat failure lines.",
   [(f'sum(increase(ferry_config_reloads_total{{job=~"$job",result="success"}}[1m]))', "reloads / min"),
    (f'sum(increase(ferry_config_reloads_total{{job=~"$job",result="failure"}}[1m]))', "rejected configs / min"),
    (f'count(process_start_time_seconds{{job=~"$job"}} > time() - 60) or vector(0)', "proxies started in the last minute"),
    (f'sum(rate(ferry_requests_total{{{L},code_class="5xx"}}[{RI}]))', "5xx / s"),
    (f'sum(rate(ferry_downstream_incomplete_responses_total{{{L}}}[{RI}]))', "cut off / s")],
   "short", 8, w=8)
ts("Resident memory",
   "process_resident_memory_bytes per proxy. Flat over a soak is the SLO.",
   [(f'process_resident_memory_bytes{{job=~"$job"}}', "{{instance}}")],
   "bytes", 16, w=8)
advance(8)

# ---------------------------------------------------------------- Routes
row("Requests by route (RED)")
ts("Request rate",
   "Downstream requests per second by route.",
   [(f'sum by (route) (rate(ferry_requests_total{{{L},{R}}}[{RI}]))', "{{route}}")],
   "reqps", 0, w=8)
ts("Error ratio (5xx)",
   "Share of each route's requests answered 5xx.",
   [(f'sum by (route) (rate(ferry_requests_total{{{L},{R},code_class="5xx"}}[{RI}])) / sum by (route) (rate(ferry_requests_total{{{L},{R}}}[{RI}]))', "{{route}}")],
   "percentunit", 8, w=8)
ts("Request duration p99",
   "Arrival to last response byte, including backend time.",
   [(q(0.99, "ferry_request_duration_seconds", f"{L},{R}", "route"), "{{route}}")],
   "s", 16, w=8)
advance(8)
ts("Responses by status class",
   "All routes.",
   [(f'sum by (code_class) (rate(ferry_requests_total{{{L},{R}}}[{RI}]))', "{{code_class}}")],
   "reqps", 0, w=8, stack=True)
ts("Local replies by reason",
   "Requests Ferry answered itself: no route, rate limited, no healthy upstream, overflow, upstream failure, timeout, ...",
   [(f'sum by (reason) (rate(ferry_local_replies_total{{{L}}}[{RI}])) > 0', "{{reason}}")],
   "reqps", 8, w=8, stack=True)
ts("gRPC status",
   "gRPC responses by grpc-status, from headers or trailers.",
   [(f'sum by (grpc_code) (rate(ferry_grpc_responses_total{{{L},{R}}}[{RI}]))', "{{grpc_code}}")],
   "reqps", 16, w=8, stack=True)
advance(8)

# ---------------------------------------------------------------- Backends
row("Backends")
ts("Upstream rate by cluster",
   "Upstream attempts per second (retries included).",
   [(f'sum by (cluster) (rate(ferry_upstream_responses_total{{{C}}}[{RI}]))', "{{cluster}}")],
   "reqps", 0, w=8)
ts("Upstream 5xx ratio",
   "Share of upstream responses that were 5xx, per cluster.",
   [(f'sum by (cluster) (rate(ferry_upstream_responses_total{{{C},code_class="5xx"}}[{RI}])) / sum by (cluster) (rate(ferry_upstream_responses_total{{{C}}}[{RI}]))', "{{cluster}}")],
   "percentunit", 8, w=8)
ts("Upstream latency p99",
   "Time from sending an attempt to its response headers.",
   [(q(0.99, "ferry_upstream_latency_seconds", C, "cluster"), "{{cluster}}")],
   "s", 16, w=8)
advance(8)
ts("Endpoints",
   "Configured, healthy (active checks), ejected (outlier detection) and eligible endpoints per cluster.",
   [(f'sum by (cluster) (ferry_cluster_endpoints{{{C}}})', "{{cluster}} configured"),
    (f'sum by (cluster) (ferry_cluster_healthy_endpoints{{{C}}})', "{{cluster}} healthy"),
    (f'sum by (cluster) (ferry_cluster_ejected_endpoints{{{C}}})', "{{cluster}} ejected"),
    (f'min by (cluster) (ferry_cluster_eligible_endpoints{{{C}}})', "{{cluster}} eligible")],
   "short", 0, w=8)
ts("Ejections and panic mode",
   "Outlier ejections per minute; panic mode means too few endpoints are healthy and health is ignored.",
   [(f'sum by (cluster) (increase(ferry_endpoint_ejections_total{{{C}}}[1m]))', "{{cluster}} ejections / min"),
    (f'max by (cluster) (ferry_cluster_panic{{{C}}})', "{{cluster}} panic")],
   "short", 8, w=8)
ts("Upstream failures",
   "Connect failures, resets before response headers and per-try timeouts.",
   [(f'sum by (cluster) (rate(ferry_upstream_connect_failures_total{{{C}}}[{RI}]))', "{{cluster}} connect"),
    (f'sum by (cluster) (rate(ferry_upstream_resets_total{{{C}}}[{RI}]))', "{{cluster}} reset"),
    (f'sum by (cluster) (rate(ferry_upstream_attempt_timeouts_total{{{C}}}[{RI}]))', "{{cluster}} timeout")],
   "reqps", 16, w=8)
advance(8)
ts("Retries and retry budget",
   "Retries per second, policy retries refused by the budget, and the budget's remaining headroom.",
   [(f'sum by (cluster) (rate(ferry_upstream_retries_total{{{C}}}[{RI}]))', "{{cluster}} retries / s"),
    (f'sum by (cluster) (rate(ferry_retry_budget_exhausted_total{{{C}}}[{RI}]))', "{{cluster}} budget exhausted / s"),
    (f'min by (cluster) (ferry_retry_budget_remaining{{{C}}})', "{{cluster}} budget remaining")],
   "short", 0, w=8)
ts("Circuit breakers",
   "Usage as a share of each limit, and requests refused because a breaker was full.",
   [(f'max by (cluster, breaker) (ferry_circuit_breaker_active{{{C}}} / ferry_circuit_breaker_max{{{C}}})', "{{cluster}} {{breaker}} used"),
    (f'sum by (cluster, breaker) (rate(ferry_circuit_breaker_overflows_total{{{C}}}[{RI}])) > 0', "{{cluster}} {{breaker}} overflow / s")],
   "short", 8, w=8)
ts("Connections and load per endpoint",
   "Idle pooled connections and in-flight requests per endpoint.",
   [(f'sum by (cluster) (ferry_pool_idle_connections{{{C}}})', "{{cluster}} idle pooled"),
    (f'sum by (cluster, breaker) (ferry_circuit_breaker_active{{{C},breaker="connections"}})', "{{cluster}} open"),
    (f'sum by (cluster) (ferry_endpoint_active_requests{{{C}}})', "{{cluster}} in flight")],
   "short", 16, w=8)
advance(8)

# ---------------------------------------------------------------- Proxy
row("Proxy")
ts("Downstream connections",
   "Open connections and new connections per second.",
   [(f'sum by (listener) (ferry_downstream_active_connections{{{L}}})', "{{listener}} open"),
    (f'sum by (listener) (rate(ferry_downstream_connections_total{{{L}}}[{RI}]))', "{{listener}} new / s")],
   "short", 0, w=8)
ts("Handshake and PROXY errors",
   "TLS handshakes that failed and connections dropped for a bad PROXY header.",
   [(f'sum by (listener) (rate(ferry_downstream_tls_errors_total{{{L}}}[{RI}]))', "{{listener}} TLS"),
    (f'sum by (listener) (rate(ferry_downstream_proxy_protocol_errors_total{{{L}}}[{RI}]))', "{{listener}} PROXY")],
   "short", 8, w=8)
ts("Config",
   "Snapshot version per proxy, seconds since the last successful load, and access-log lines dropped.",
   [(f'ferry_config_version{{job=~"$job"}}', "{{instance}} version"),
    (f'time() - ferry_config_last_reload_success_timestamp_seconds{{job=~"$job"}}', "{{instance}} seconds since load"),
    (f'sum(rate(ferry_access_log_dropped_total{{job=~"$job"}}[{RI}]))', "access log drops / s"),
    (f'max(ferry_draining{{job=~"$job"}})', "draining")],
   "short", 16, w=8)
advance(8)
ts("Certificate expiry",
   "Days until each certificate Ferry presents expires (listener, upstream client, controller stream). Alerts fire under 21 and 7 days.",
   [(f'min by (kind, owner, name) (ferry_tls_certificate_expiry_timestamp_seconds{{job=~"$job"}} - time()) / 86400', "{{kind}} {{owner}} {{name}}")],
   "d", 0, w=12, min0=False,
   thresholds=[("red", None), ("orange", 7), ("green", 21)])
ts("Controller link",
   "Proxies with a live snapshot stream to the controller, out of those following one.",
   [(f'sum(ferry_controller_connected{{job=~"$job"}})', "connected"),
    (f'count(ferry_controller_connected{{job=~"$job"}})', "following")],
   "short", 12, w=12)
advance(8)

def var(name, label, query, include_all=True):
    return {"name": name, "label": label, "type": "query", "datasource": DS,
            "query": {"query": query, "refId": "StandardVariableQuery"},
            "definition": query, "refresh": 2, "includeAll": include_all, "multi": True,
            "allValue": ".*", "current": {"selected": True, "text": "All", "value": "$__all"},
            "sort": 1, "hide": 0}

dashboard = {
    "uid": "ferry-overview",
    "title": "Ferry",
    "description": "Ferry L7 load balancer: SLOs, RED per route and per backend, health, resilience and config. Metrics from the admin API's /metrics.",
    "tags": ["ferry", "gateway", "slo"],
    "timezone": "browser",
    "schemaVersion": 39,
    "version": 1,
    "editable": True,
    "graphTooltip": 1,
    "refresh": "10s",
    "time": {"from": "now-1h", "to": "now"},
    "fiscalYearStartMonth": 0,
    "liveNow": False,
    "links": [],
    "annotations": {"list": [
        {"builtIn": 1, "datasource": {"type": "grafana", "uid": "-- Grafana --"}, "enable": True,
         "hide": True, "iconColor": "rgba(0, 211, 255, 1)", "name": "Annotations & Alerts", "type": "dashboard"},
        {"name": "Config reloads", "datasource": DS, "enable": True, "iconColor": "blue",
         "expr": 'changes(ferry_config_version{job=~"$job"}[1m]) > 0', "step": "30s",
         "titleFormat": "Config reload", "textFormat": "{{instance}}"},
        {"name": "Proxy starts", "datasource": DS, "enable": True, "iconColor": "orange",
         "expr": '(time() - process_start_time_seconds{job=~"$job"}) < 60',
         "step": "30s", "titleFormat": "Proxy started", "textFormat": "{{instance}}"},
    ]},
    "templating": {"list": [
        {"name": "datasource", "label": "Prometheus", "type": "datasource", "query": "prometheus",
         "current": {}, "hide": 0, "refresh": 1, "regex": "", "includeAll": False, "multi": False},
        var("job", "Job", "label_values(ferry_build_info, job)"),
        var("listener", "Listener", 'label_values(ferry_requests_total{job=~"$job"}, listener)'),
        var("route", "Route", 'label_values(ferry_requests_total{job=~"$job", listener=~"$listener"}, route)'),
        var("cluster", "Cluster", 'label_values(ferry_upstream_responses_total{job=~"$job"}, cluster)'),
    ]},
    "panels": panels,
}

out = sys.argv[1]
with open(out, "w", encoding="utf-8", newline="\n") as f:
    json.dump(dashboard, f, indent=2)
    f.write("\n")
print(f"{len([p for p in panels if p['type'] != 'row'])} panels -> {out}")
