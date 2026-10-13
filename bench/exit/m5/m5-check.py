"""Checks the M5 soak (bench/exit/run-m5.sh) against the SLOs.

Reads Prometheus (Ferry scraped every 5 s), the k6 segment summaries and the
ws-flood report, and writes a JSON report. Exit status 1 if any check fails.

Checks:
- success: 99.95% of requests succeed (Ferry's counters over the whole run,
  chaos included; k6's failure rate as a cross-check).
- added latency: Ferry's own histogram, p50 < 0.2 ms and p99 < 1 ms over the
  run (the k6 numbers include Docker networking and are reported only).
- flat memory: a least-squares line through resident memory after a
  30-minute warm-up (or the first quarter of a shorter run); its slope
  projected over 72 hours must stay under max(10% of the median, 16 MiB).
  Open file descriptors get the same treatment.
- WebSockets: every socket opened and was still echoing at the end.
- k6 delivered the offered load: under 0.01% of iterations dropped. Chaos
  (a killed backend stalls connects until it is ejected) makes k6 allocate
  VUs mid-run, which drops a few iterations; the count is reported.

Usage: m5-check.py --prometheus URL --start EPOCH --end EPOCH --k6 DIR
                   --ws FILE --rate N --out FILE
"""

import argparse
import glob
import json
import os
import statistics
import sys
import urllib.parse
import urllib.request

MIB = 1024 * 1024


def prom(base, path, **params):
    url = f"{base}/api/v1/{path}?{urllib.parse.urlencode(params)}"
    with urllib.request.urlopen(url, timeout=60) as r:
        body = json.load(r)
    if body.get("status") != "success":
        raise RuntimeError(f"{url}: {body}")
    return body["data"]["result"]


def instant(base, query, at):
    result = prom(base, "query", query=query, time=at)
    return float(result[0]["value"][1]) if result else None


def series(base, query, start, end, step=30):
    result = prom(base, "query_range", query=query, start=start, end=end, step=step)
    if not result:
        return []
    return [(float(t), float(v)) for t, v in result[0]["values"]]


def slope(points):
    """Least-squares slope (units per second) and intercept."""
    n = len(points)
    if n < 3:
        return 0.0, points[0][1] if points else 0.0
    mx = sum(t for t, _ in points) / n
    my = sum(v for _, v in points) / n
    sxx = sum((t - mx) ** 2 for t, _ in points)
    sxy = sum((t - mx) * (v - my) for t, v in points)
    b = sxy / sxx if sxx else 0.0
    return b, my - b * mx


def flatness(name, points, warmup_until, floor):
    steady = [(t, v) for t, v in points if t >= warmup_until]
    if len(steady) < 3:
        return {"name": name, "ok": False, "reason": "not enough samples"}
    b, _ = slope(steady)
    median = statistics.median(v for _, v in steady)
    projected_72h = b * 72 * 3600
    allowed = max(0.10 * median, floor)
    return {
        "name": name,
        "ok": projected_72h < allowed,
        "samples": len(steady),
        "first": steady[0][1],
        "last": steady[-1][1],
        "min": min(v for _, v in steady),
        "max": max(v for _, v in steady),
        "median": median,
        "slope_per_hour": b * 3600,
        "projected_72h_growth": projected_72h,
        "allowed_72h_growth": allowed,
    }


def k6_totals(directory):
    files = sorted(glob.glob(os.path.join(directory, "m5-k6-*.json")))
    reqs = fails = dropped = 0.0
    p99s, meds = [], []
    for f in files:
        with open(f, encoding="utf-8") as fh:
            m = json.load(fh)["metrics"]
        n = m.get("http_reqs", {}).get("count", 0)
        reqs += n
        fails += m.get("http_req_failed", {}).get("value", 0) * n
        dropped += m.get("dropped_iterations", {}).get("count", 0)
        d = m.get("http_req_duration", {})
        if "p(99)" in d:
            p99s.append(d["p(99)"])
            meds.append(d["med"])
    return {
        "segments": len(files),
        "requests": int(reqs),
        "failed": int(round(fails)),
        "failed_ratio": fails / reqs if reqs else None,
        "dropped_iterations": int(dropped),
        "worst_segment_p99_ms": max(p99s) if p99s else None,
        "median_segment_p50_ms": statistics.median(meds) if meds else None,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--prometheus", required=True)
    ap.add_argument("--start", type=int, required=True)
    ap.add_argument("--end", type=int, required=True)
    ap.add_argument("--k6", required=True)
    ap.add_argument("--ws", required=True)
    ap.add_argument("--rate", type=int, required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    base, start, end = a.prometheus.rstrip("/"), a.start, a.end
    span = end - start
    rng = f"{span}s"
    warmup = start + min(1800, span // 4)
    checks = []

    total = instant(base, f"sum(increase(ferry_requests_total[{rng}]))", end) or 0
    errors = instant(base, f'sum(increase(ferry_requests_total{{code_class="5xx"}}[{rng}]))', end) or 0
    success = 1 - errors / total if total else 0
    checks.append({"name": "success ratio >= 99.95%", "ok": success >= 0.9995,
                   "value": success, "requests": total, "5xx": errors})

    for q, limit in (("0.5", 0.0002), ("0.99", 0.001)):
        v = instant(base, "histogram_quantile(%s, sum by (le) (increase(ferry_request_overhead_seconds_bucket[%s])))" % (q, rng), end)
        checks.append({"name": f"added latency p{int(float(q) * 100)} < {limit * 1000:g} ms",
                       "ok": v is not None and v < limit, "value_ms": None if v is None else v * 1000})

    rss = series(base, "max(process_resident_memory_bytes)", start, end)
    checks.append(dict(flatness("flat resident memory", rss, warmup, 16 * MIB)))
    fds = series(base, "max(process_open_fds)", start, end)
    checks.append(dict(flatness("flat open file descriptors", fds, warmup, 256)))

    with open(a.ws, encoding="utf-8") as fh:
        ws = json.loads(fh.read().strip() or "{}")
    checks.append({"name": "every WebSocket opened and stayed alive",
                   "ok": ws.get("alive_at_end") == ws.get("count") and ws.get("count", 0) > 0, **ws})

    k6 = k6_totals(a.k6)
    offered = k6["requests"] + k6["dropped_iterations"]
    k6["dropped_ratio"] = k6["dropped_iterations"] / offered if offered else None
    checks.append({"name": "k6 delivered the offered load (< 0.01% dropped iterations)",
                   "ok": offered > 0 and k6["dropped_iterations"] / offered < 1e-4, **k6})

    extra = {
        "duration_seconds": span,
        "rate": a.rate,
        "reloads": instant(base, f'sum(increase(ferry_config_reloads_total{{result="success"}}[{rng}]))', end),
        "reload_failures": instant(base, f'sum(increase(ferry_config_reloads_total{{result="failure"}}[{rng}]))', end),
        "local_replies": {r["metric"].get("reason", "?"): float(r["value"][1]) for r in prom(
            base, "query", query=f"sum by (reason) (increase(ferry_local_replies_total[{rng}])) > 0", time=end)},
        "retries": instant(base, f"sum(increase(ferry_upstream_retries_total[{rng}]))", end),
        "ejections": instant(base, f"sum(increase(ferry_endpoint_ejections_total[{rng}]))", end),
        "peak_active_connections": instant(base, f"max_over_time(sum(ferry_downstream_active_connections)[{rng}:30s])", end),
        "peak_rss_bytes": max((v for _, v in rss), default=None),
    }
    report = {"ok": all(c["ok"] for c in checks), "checks": checks, "run": extra}
    with open(a.out, "w", encoding="utf-8", newline="\n") as fh:
        json.dump(report, fh, indent=2)
    for c in checks:
        print(f"   {'PASS' if c['ok'] else 'FAIL'}  {c['name']}")
    print(f"   report: {a.out}")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
