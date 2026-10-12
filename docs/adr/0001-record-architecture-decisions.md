# ADR 0001: Record architecture decisions

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M0

## Context

Ferry has one human reviewer and is built milestone by milestone. Decisions made
early (config shape, error policy, crate boundaries) constrain every later
milestone, and the reasons are easy to lose.

## Decision

Every significant design choice gets a short ADR in `docs/adr/`, numbered in
order, using [`0000-template.md`](0000-template.md). When the plan is ambiguous we
pick what Envoy or the Gateway API does, write it down here, and continue.
Superseded ADRs stay in place with their status updated.

## Consequences

Reviewers can check code against a written intent. Small, reversible choices do
not need an ADR; anything touching the public config API, security, the hot path
or operations does.
