"""M3 exit checks against a running stack (see run-m3.sh).

1. Evaluates every query of every dashboard panel against Prometheus, the way
   Grafana would (variables set to "all"), and requires each SLO panel to
   return data.
2. Fetches each trace ID from Tempo and requires one trace to chain
   trace-client -> ferry (server -> client span) -> mock backend.

Usage: python m3-check.py DASHBOARD.json TRACE_IDS.txt OUT.json
"""
import base64
import json
import sys
import urllib.parse
import urllib.request

PROM = "http://localhost:9090"
TEMPO = "http://localhost:3200"


def prom(expr):
    url = f"{PROM}/api/v1/query?" + urllib.parse.urlencode({"query": expr})
    with urllib.request.urlopen(url, timeout=30) as r:
        body = json.load(r)
    if body.get("status") != "success":
        raise RuntimeError(body)
    return body["data"]["result"]


def substitute(expr, range_):
    for var in ("$job", "$listener", "$route", "$cluster"):
        expr = expr.replace(var, ".*")
    return expr.replace("$__rate_interval", "1m").replace("$__range", range_)


def check_dashboard(path, range_="5m"):
    d = json.load(open(path, encoding="utf-8"))
    panels, section, out = d["panels"], None, []
    for p in panels:
        if p["type"] == "row":
            section = p["title"]
            continue
        results = []
        for t in p.get("targets", []):
            expr = substitute(t["expr"], range_)
            series = prom(expr)
            results.append({
                "legend": t.get("legendFormat", ""),
                "series": len(series),
                "values": [round(float(s["value"][1]), 6) for s in series[:6]],
            })
        out.append({"section": section, "title": p["title"], "targets": results,
                    "has_data": any(r["series"] for r in results)})
    return out


def hexid(v):
    if not v:
        return ""
    try:
        int(v, 16)
        if len(v) in (16, 32):
            return v.lower()
    except ValueError:
        pass
    return base64.b64decode(v).hex()


def check_trace(trace_id):
    req = urllib.request.Request(f"{TEMPO}/api/traces/{trace_id}",
                                 headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        body = json.load(r)
    spans = []
    for batch in body.get("batches", body.get("resourceSpans", [])):
        attrs = batch.get("resource", {}).get("attributes", [])
        service = next((a["value"].get("stringValue") for a in attrs
                        if a["key"] == "service.name"), "?")
        for scope in batch.get("scopeSpans", batch.get("instrumentationLibrarySpans", [])):
            for s in scope["spans"]:
                spans.append({
                    "service": service,
                    "name": s["name"],
                    "kind": s.get("kind"),
                    "id": hexid(s["spanId"]),
                    "parent": hexid(s.get("parentSpanId", "")),
                })
    by_id = {s["id"]: s for s in spans}

    def chain(s):
        path = [s]
        while path[-1]["parent"] in by_id:
            path.append(by_id[path[-1]["parent"]])
        return [f'{x["service"]}:{x["kind"]}:{x["name"]}' for x in reversed(path)]

    leaves = [s for s in spans if not any(o["parent"] == s["id"] for o in spans)]
    chains = [chain(s) for s in leaves]
    services = sorted({s["service"] for s in spans})
    ok = any(len(c) >= 4 and c[0].startswith("trace-client:") and
             c[1].startswith("ferry:") and c[2].startswith("ferry:") and
             c[3].startswith("mock-") for c in chains)
    return {"trace_id": trace_id, "spans": len(spans), "services": services,
            "chains": chains, "client_ferry_backend": ok}


def main():
    dashboard, ids_path, out_path = sys.argv[1:4]
    panels = check_dashboard(dashboard)
    slo = [p for p in panels if p["section"] == "Service level objectives"]
    traces = []
    for line in open(ids_path, encoding="utf-8"):
        tid = line.split()[0] if line.strip() else ""
        if tid:
            traces.append(check_trace(tid))
    result = {"panels": panels, "traces": traces}
    json.dump(result, open(out_path, "w", encoding="utf-8", newline="\n"), indent=2)

    print("   SLO panels:")
    for p in slo:
        vals = "; ".join(f'{t["legend"] or "value"}={t["values"]}' for t in p["targets"])
        print(f'     {"OK  " if p["has_data"] else "NONE"} {p["title"]}: {vals}')
    empty = [p["title"] for p in panels if not p["has_data"]]
    print(f'   panels with data: {len(panels) - len(empty)}/{len(panels)}'
          + (f' (empty: {", ".join(empty)})' if empty else ""))
    for t in traces:
        print(f'   trace {t["trace_id"]}: {t["spans"]} spans, services {t["services"]}, '
              f'client->ferry->backend: {t["client_ferry_backend"]}')
        for c in t["chains"]:
            print("     " + "  ->  ".join(c))
    failed = [p["title"] for p in slo if not p["has_data"]]
    if failed or not traces or not all(t["client_ferry_backend"] for t in traces):
        print(f"   FAILED: SLO panels without data {failed}")
        sys.exit(1)
    print("   PASSED")


if __name__ == "__main__":
    main()
