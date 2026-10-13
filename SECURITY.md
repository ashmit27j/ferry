# Security policy

Ferry sits on the edge of a cluster: it terminates TLS for untrusted clients
and holds the private keys of every certificate it serves. Security reports
get priority over all other work.

## Supported versions

Ferry is pre-1.0. Only the latest release and `main` receive security fixes.
From v1.0.0, the latest minor release is supported, and the previous minor
for 90 days after a new one ships.

## Reporting a vulnerability

**Do not open a public issue, pull request or discussion.**

- Use GitHub's private reporting: on the repository's **Security** tab, choose
  **Report a vulnerability**. While the repository is private, collaborators
  can open a draft security advisory there instead.
- Include the Ferry version (`ferry --version`, or the image digest), the
  configuration or Kubernetes manifests involved (with secrets removed), and
  the steps or traffic needed to reproduce.

What to expect:

| Step | Target |
| --- | --- |
| Acknowledgement | 3 working days |
| Triage and severity (CVSS 4.0) | 7 days |
| Fix for critical or high severity | 30 days |
| Fix for medium or low severity | the next release |
| Public disclosure | coordinated with the reporter; 90 days at the latest |

Fixes ship as a patch release with a GitHub Security Advisory and, where
applicable, a CVE. Reporters are credited unless they ask not to be.

## Scope

In scope: the `ferry` binary (proxy, controller, certgen), the container
image, the Helm chart, and the default configuration they produce. Examples
of what counts:

- Request smuggling, header injection, or routing that bypasses a route's
  match, rate limit or filters.
- Crashes, panics or unbounded memory, CPU or file-descriptor use triggered
  by a client, a backend or a route author.
- TLS weaknesses: accepting what rustls should reject, serving the wrong
  certificate, leaking key material through the admin API, logs or metrics.
- A route author in one namespace reaching backends or certificates they
  were not granted (`ReferenceGrant`, `allowedRoutes`).
- Privilege escalation through the controller's Kubernetes permissions.

Out of scope: volumetric DDoS beyond what the configured limits can absorb
(handle it at the NLB or upstream of it), vulnerabilities in a backend Ferry
proxies to, and findings that need write access to the Ferry namespace or
cluster-admin.

## How Ferry is hardened

The [threat model](docs/security/threat-model.md) lists assets, trust
boundaries, threats and the mitigation for each, including the residual
risks we accept. In short:

- Memory-safe Rust with `unsafe` forbidden, and no `unwrap`/`expect`/`panic!`
  in library code (enforced by lints). A bad request or config must never
  crash a proxy.
- Every parser of untrusted input is fuzzed nightly (`fuzz/`).
- rustls with the aws-lc-rs provider, TLS 1.2 and 1.3 only.
- Request limits, HTTP/2 Rapid Reset, MadeYouReset and CONTINUATION
  mitigations ([ADR 0016](docs/adr/0016-request-limits-and-smuggling.md)).
- Distroless, non-root image with a read-only root filesystem. Proxies have
  no Kubernetes API access, and the controller has least-privilege RBAC
  ([ADR 0035](docs/adr/0035-controller-runtime-and-provisioning.md)).
- Controller and proxies talk over mutual TLS
  ([ADR 0033](docs/adr/0033-snapshot-delivery.md)).
- Dependencies are checked by `cargo deny` and `cargo audit` on every commit.
  GitHub Actions are pinned by commit SHA.

## Verifying a release

Release images (linux/amd64 and linux/arm64) and the Helm chart (an OCI
artifact under `ghcr.io/ashmit27j/charts/ferry`) are signed with Cosign
keyless signing (Sigstore) by the release workflow, which runs on `main`
after a release PR merges. Each image also carries an SPDX SBOM attestation
and SLSA build provenance:

```sh
IMAGE=ghcr.io/ashmit27j/ferry:1.0.0
cosign verify "$IMAGE" \
  --certificate-identity https://github.com/ashmit27j/ferry/.github/workflows/release.yml@refs/heads/main \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
cosign verify-attestation "$IMAGE" --type spdxjson \
  --certificate-identity https://github.com/ashmit27j/ferry/.github/workflows/release.yml@refs/heads/main \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The SBOM is also attached to each GitHub release. Provenance can be checked
with `gh attestation verify oci://$IMAGE --owner ashmit27j`.

Kubernetes clusters can enforce signatures at admission, for example with the
Sigstore policy-controller or Kyverno's `verifyImages`.
