# ADR 0008: Apply the design system to docs and Grafana, build no UI in v1

- **Status:** Accepted
- **Date:** 2026-10-13
- **Milestone:** M0

## Context

The project adopts a design system ([`DESIGN.md`](../../DESIGN.md)): white
canvas, near-black ink, a single emerald accent `#3ecf8e`, a grey ladder, Inter
at weight 500, and 6/8/12 px radii. Ferry v1 has no web UI: the plan defers the
web dashboard until after v1.0.

## Decision

The design system applies, in v1, to:

1. **The Grafana dashboard (M3).** Emerald for the primary success/throughput
   series and "healthy" states; the grey ladder for secondary series, gridlines
   and text; the chart-only accents (purple, violet, yellow) for further series.
   Errors use a red outside the brand palette because status colour must not
   depend on brand colour.
2. **Docs and diagrams.** Mermaid diagrams in the README and docs use the
   `base` theme with ink text, hairline borders and an emerald stroke on Ferry
   itself.
3. **Any future web dashboard (post-v1)** starts from `DESIGN.md` as-is.

No admin HTML pages or marketing site are built in v1.

## Consequences

Visual work stays out of the critical path. The dashboard JSON in M3 is checked
against this ADR at the M3 gate.
