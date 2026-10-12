// k6 load test for Ferry's HTTP path.
//
// Start three mock backends and the proxy, then:
//   k6 run -e TARGET=http://127.0.0.1:8080 -e RATE=10000 -e DURATION=60s bench/k6/proxy.js
// or with Docker:
//   docker run --rm -i --network host grafana/k6 run - < bench/k6/proxy.js
//
// The scenario holds a constant arrival rate, so latency is measured without
// coordinated omission. Thresholds encode the M1 exit criterion (zero errors);
// the added-latency SLO is computed separately by comparing against a run
// that targets a backend directly (see bench/README.md).

import http from 'k6/http';
import { check } from 'k6';

const TARGET = __ENV.TARGET || 'http://127.0.0.1:8080';
const RATE = parseInt(__ENV.RATE || '1000', 10);
const DURATION = __ENV.DURATION || '30s';
const PATH = __ENV.PATH_ || '/bench';

export const options = {
  discardResponseBodies: true,
  scenarios: {
    constant: {
      executor: 'constant-arrival-rate',
      rate: RATE,
      timeUnit: '1s',
      duration: DURATION,
      preAllocatedVUs: Math.max(50, Math.ceil(RATE / 50)),
      maxVUs: Math.max(200, Math.ceil(RATE / 10)),
    },
  },
  thresholds: {
    http_req_failed: ['rate==0'],
    dropped_iterations: ['count==0'],
  },
  summaryTrendStats: ['min', 'med', 'p(90)', 'p(99)', 'p(99.9)', 'max'],
};

export default function () {
  const res = http.get(`${TARGET}${PATH}`);
  check(res, { 'status is 200': (r) => r.status === 200 });
}
