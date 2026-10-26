# Changelog

## [0.2.0](https://github.com/ashmit27j/ferry/compare/v0.1.0...v0.2.0) (2026-10-26)


### Features

* add M0 foundations workspace ([59a49e8](https://github.com/ashmit27j/ferry/commit/59a49e82a2330552ac3416804061918746bfb58d))
* **cli:** admin, access-log and OTLP flags; check-config JSON output ([8f6c162](https://github.com/ashmit27j/ferry/commit/8f6c162f1d0a3c283e508c9e8e7295d241457386))
* **cli:** hot-reload the config file in ferry proxy ([4fe8598](https://github.com/ashmit27j/ferry/commit/4fe85987b73ddb902230ad64d780e9045f95a0a1))
* **config:** full Gateway API match and filter model, cluster policies; freeze API ([884205c](https://github.com/ashmit27j/ferry/commit/884205cce16420cb0122ae66a6b9ad8e155bd52b))
* **config:** invalid-backend weight, advertised port, empty listeners and clusters ([66e4861](https://github.com/ashmit27j/ferry/commit/66e4861fe1a9f5ba0e33a193d41459ebe9c82e5a))
* **config:** TLS, limits, timeouts, retries, rate limits and resilience settings ([80cc62e](https://github.com/ashmit27j/ferry/commit/80cc62ed58e5d8d90bdfc3215be0e7e3a7eb4a3a))
* **controller:** claim the 303, 307 and 308 redirect status features ([01ecd7e](https://github.com/ashmit27j/ferry/commit/01ecd7e72acd548566d9ce45ca9b6f6f9090fcf6))
* **controller:** Gateway API types, FerryPolicy, translator and snapshot server ([046df2a](https://github.com/ashmit27j/ferry/commit/046df2a371ddb143fdeaaebcfa0471706dfac6ee))
* **controller:** Prometheus metrics on the admin port ([1485fd8](https://github.com/ashmit27j/ferry/commit/1485fd864eb3a0e60ec76f914e009c31e2b73328))
* **controller:** reconcile loop, status, provisioning, leader election and CLI ([943e966](https://github.com/ashmit27j/ferry/commit/943e966710c396773c69b19fb0c9e5383ae41bfb))
* **deploy:** alerting rules, runbooks and a controller PodMonitor ([7e4ae45](https://github.com/ashmit27j/ferry/commit/7e4ae458b9b3643165957228ccb9703ca73bc319))
* **deploy:** distroless image and Helm chart ([317925a](https://github.com/ashmit27j/ferry/commit/317925a288bc4b37c6cf4ed1f9f0421ae5ee5de1))
* **deploy:** EKS staging bring-up script ([a24ce7f](https://github.com/ashmit27j/ferry/commit/a24ce7f1a9083e6abf0098250858e83ada9d3576))
* **deploy:** Grafana dashboard with the SLOs first ([7db5754](https://github.com/ashmit27j/ferry/commit/7db575406a8a8e67119d25576404f3ff2b0c6f3b))
* **deploy:** NetworkPolicies for the controller and every proxy fleet ([804f0fb](https://github.com/ashmit27j/ferry/commit/804f0fbb6cd6bb5e229b3476398b18b21121b837))
* **lb:** expose retry-budget headroom and lifetime ejection counts ([40d7bc5](https://github.com/ashmit27j/ferry/commit/40d7bc513b9a5b3a8a140adc48fab07f4689500a))
* **lb:** health-aware cluster balancer, five policies, health checks, pools ([8699375](https://github.com/ashmit27j/ferry/commit/86993751cc5fdc5465db279f7763adef83a7cf9d))
* **proxy,config:** listener connection cap and tunnel idle timeout ([0b63da4](https://github.com/ashmit27j/ferry/commit/0b63da4528da049a2dc6f6f1c2a5798b952a52f4))
* **proxy,lb,testkit:** upstream TLS and mutual TLS per cluster ([63ae08d](https://github.com/ashmit27j/ferry/commit/63ae08d577c6bd823e3adcffa6c6c9d3bed2f86d))
* **proxy,lb:** passive outlier ejection ([c9c154a](https://github.com/ashmit27j/ferry/commit/c9c154afceaf5bc6b72894bb166e2a2af223accd))
* **proxy,lb:** per-cluster circuit breakers ([71001bf](https://github.com/ashmit27j/ferry/commit/71001bf6bef524ff6df010034617f46ee17f9ad2))
* **proxy,lb:** retry policies with a sliding-window retry budget ([5d9978d](https://github.com/ashmit27j/ferry/commit/5d9978d97bd72b618387091c3337a6c45c802709))
* **proxy,telemetry:** certificate expiry and controller link metrics ([48bdbd3](https://github.com/ashmit27j/ferry/commit/48bdbd3ae28d7274da9a5c2e19e000a30c9b8f7b))
* **proxy:** indexed router, filters, health-aware balancing, safe retries, reload state ([498558b](https://github.com/ashmit27j/ferry/commit/498558b617f9aa0b7d8988f98902f707f818a7fc))
* **proxy:** per-route local rate limits ([8db7d19](https://github.com/ashmit27j/ferry/commit/8db7d19c88af639a94899cc0fbdef4072da3f848))
* **proxy:** PROXY protocol v1/v2, X-Forwarded-For/Proto and X-Request-Id ([21ab62b](https://github.com/ashmit27j/ferry/commit/21ab62b5af7508b81eb2fa5dc3a5b262988c30a7))
* **proxy:** request limits, HTTP/2 hardening and smuggling defences ([168d713](https://github.com/ashmit27j/ferry/commit/168d71349cd7485e5e7360261bfd25a4808a930c))
* **proxy:** request metrics, access logs, tracing and an admin API ([90e0089](https://github.com/ashmit27j/ferry/commit/90e0089186d0c46c766db055e57ac7ec22bcacbc))
* **proxy:** request, per-attempt and idle timeouts; gRPC deadlines ([85051e7](https://github.com/ashmit27j/ferry/commit/85051e796aa8b3c8796008ba41e8a4b9d68d91b7))
* **proxy:** runtime listeners, readiness before the first snapshot, controller client ([5cf16fb](https://github.com/ashmit27j/ferry/commit/5cf16fb836afa2065cf75b08b5ba1f1b40fcbaee))
* **proxy:** TLS termination with SNI, ALPN and hot certificate rotation ([593010f](https://github.com/ashmit27j/ferry/commit/593010f3a89095092388fdc32efb5a01277aa294))
* **proxy:** two-stage graceful drain for zero-drop rolling restarts ([cc656c1](https://github.com/ashmit27j/ferry/commit/cc656c12ccd795522f52f75edeb0294faac4da42))
* **proxy:** WebSocket and other HTTP/1.1 upgrades ([6d535d0](https://github.com/ashmit27j/ferry/commit/6d535d01cfed17c3686e2a870a1b05cffda6eb7e))
* **telemetry:** lock-free metrics, OTLP tracing and async access logs ([fa86a66](https://github.com/ashmit27j/ferry/commit/fa86a6657de0afc62002fe4dd415112e95e509a6))
* **testkit:** health endpoints on the HTTP and gRPC mocks ([9cf609a](https://github.com/ashmit27j/ferry/commit/9cf609af1460f190479eb7ae23934352b4b96247))
* **testkit:** traced HTTP mock and a trace-client binary ([35dda00](https://github.com/ashmit27j/ferry/commit/35dda00b3248aeda6f8a33b8b7d6725e4ee8cf0e))


### Bug fixes

* a controller snapshot that validated but failed to swap was counted twice in ferry_config_reloads_total{result="failure"}. remote.rs gains unit tests; the snapshot-stream test checks the count. ([48bdbd3](https://github.com/ashmit27j/ferry/commit/48bdbd3ae28d7274da9a5c2e19e000a30c9b8f7b))
* **config:** reject durations the canonical form cannot print ([e75419d](https://github.com/ashmit27j/ferry/commit/e75419d8602fb650ed2c81dd9e879e5a549ecb9e))
* **proxy:** count open tunnels as connections; grow relay buffers on demand ([b621c60](https://github.com/ashmit27j/ferry/commit/b621c60d617ee82a1219a384e8e4c4b6b1b54311))
* **proxy:** grpc-timeout parsing panicked on a multi-byte last character ([cb80e0f](https://github.com/ashmit27j/ferry/commit/cb80e0f74831e45230d6b220a7a162bfaf937db8))
* **proxy:** make rate-limit key tests independent of the hash seed ([d59e730](https://github.com/ashmit27j/ferry/commit/d59e730f57beacc3cdff6f71767d6d0d31275725))


### Performance

* **bench:** controller translation and snapshot serialisation benchmarks ([c61bfb9](https://github.com/ashmit27j/ferry/commit/c61bfb959e283c729d28b4a2835b01ab9d118526))
* **bench:** M1 exit test in Docker and recorded results ([45d1476](https://github.com/ashmit27j/ferry/commit/45d14761ca28155360816803eeab54e4189cdc21))
* **bench:** telemetry micro-benchmarks ([7cf89e9](https://github.com/ashmit27j/ferry/commit/7cf89e9c8e9185616babaa61d5197f6d0d6f5645))
