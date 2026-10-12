# Ferry Design System

> **Scope in v1** ([ADR 0008](docs/adr/0008-design-system-scope.md)): the Grafana
> dashboard, README/docs diagrams, and any post-v1 web dashboard. Ferry ships no
> web UI in v1. See [Applying it to Ferry](#applying-it-to-ferry) at the end.

## Overview

Ferry's design language is engineered for clarity above all else. Surfaces sit on `{colors.canvas}` (pure white), with text rendered in `{colors.ink}` (`#171717` — near-black, never pure black). Across the entire system the only consistent chromatic event is the **emerald green primary** (`{colors.primary}` — `#3ecf8e`) — used as the filled CTA, occasional accent dot, and the signature highlight color in the wordmark. Everything else is a calibrated grey ladder from `#ededed` hairline-cool to `#171717` ink, with thin black-on-white typography doing most of the visual work.

Typography runs **Circular** at weight 500 for display and 400 for body. The display tier uses tight negative letter-spacing (-1.92px at 64px) to pull the rounded humanist letterforms into editorial density. There's no atmospheric gradient, no full-bleed photography, no dark-canvas marketing track — the brand commits to white.

The product itself appears as composited UI screenshots: dashboard tables, config editors, log streams. These screenshots are the brand's argument. They sit inside `{rounded.lg}` 12px containers with subtle 1px hairlines, often arranged 2-up or in a floating "stacked panes" composition.

**Key Characteristics:**
- Single emerald primary (`{colors.primary}` `#3ecf8e`) as the only chromatic event; everything else is monochrome.
- White canvas with greyscale hierarchy from `{colors.hairline-cool}` to `{colors.ink}`.
- Humanist sans display tier at weight 500 with negative letter-spacing of -1.92px to -0.42px.
- Composited product UI screenshots are the dominant decorative element — never photography, never illustrations.
- Tight 6px / 8px button radii — square-ish, technical, never pill-shaped.
- Code blocks rendered in deep `{colors.canvas-night}` (`#1c1c1c`) with monospace inline code.
- Featured tiers use a dark inverted `{colors.canvas-night}` surface, not a green one — the green is reserved for buttons and dot accents.

## Colors

### Brand & Accent
- **Emerald** (`{colors.primary}` — `#3ecf8e`): The signature CTA color. Filled-button background, wordmark accent, dot indicator.
- **Emerald Deep** (`{colors.primary-deep}` — `#24b47e`): Pressed-state lift of the primary.
- **Emerald Soft** (`{colors.primary-soft}` — `#4ade80`): Lighter emerald used in chart accents and product UI.
- **Accent Purple** (`{colors.accent-purple}` — `#6b01c2`): Rare accent used in integration logos and chart points; never a button.
- **Accent Violet** (`{colors.accent-violet}` — `#644fc1`): Secondary accent in the same role as accent purple.
- **Accent Yellow** (`{colors.accent-yellow}` — `#ffdb13`): Chart accent / status indicator only.
- **Accent Pink / Crimson / Indigo / Tomato**: Reserved for integration logos and rare chart highlights, never as system colors.

### Surface
- **Canvas** (`{colors.canvas}` — `#ffffff`): Default page background.
- **Canvas Soft** (`{colors.canvas-soft}` — `#fafafa`): Barely-tinted off-white for alternating section bands.
- **Canvas Night** (`{colors.canvas-night}` — `#1c1c1c`): Deep near-black used in code blocks, dashboard mockups, featured tiers.
- **Canvas Night Soft** (`{colors.canvas-night-soft}` — `#202020`): Slightly lifted dark for nested chrome.
- **Hairline** (`{colors.hairline}` — `#dfdfdf`): 1px borders on cards and tables.
- **Hairline Strong** (`{colors.hairline-strong}` — `#c7c7c7`): Slightly darker border for emphasis.
- **Hairline Cool** (`{colors.hairline-cool}` — `#ededed`) / **Hairline Cool 2** (`#efefef`) / **Hairline Cool 3** (`#d4d4d4`): The grey ladder for fine chrome work.

### Text
- **Ink** (`{colors.ink}` — `#171717`): Default body text. Near-black, never pure.
- **Ink Secondary** (`{colors.ink-secondary}` — `#212121`): Slightly cooler near-black for body emphasis.
- **Ink Mute** (`{colors.ink-mute}` — `#707070`): Secondary text and helper copy.
- **Ink Mute 2** (`{colors.ink-mute-2}` — `#9a9a9a`): Tertiary text.
- **Ink Faint** (`{colors.ink-faint}` — `#b2b2b2`): Disabled / placeholder text.
- **On Primary** (`{colors.on-primary}` — `#171717`): Text on the emerald primary fill — near-black, not white. The button reads as a "lit" surface with dark type, not a colored chip.
- **On Dark** (`{colors.on-dark}` — `#ffffff`): Text on canvas-night surfaces.

## Typography

### Font Family

The display and UI tier is **Circular**, a proprietary geometric humanist sans by Lineto. Fallback chain: `'Helvetica Neue', Helvetica, Arial`.

When Circular isn't licensed, use **Inter** (open source, via Google Fonts) at weight 500 for display, with `letter-spacing: -1.92px` at 64px. Inter is the closest open-source analogue to Circular's geometric humanist character.

Code blocks use **system mono** (`ui-monospace`, with Menlo / Monaco / Consolas fallbacks).

### Hierarchy

| Token | Size | Weight | Line Height | Letter Spacing | Use |
|---|---|---|---|---|---|
| `{typography.display-xxl}` | 64px | 500 | 1.1 | -1.92px | Hero headline |
| `{typography.display-xl}` | 48px | 500 | 1.1 | -1.44px | Section opener |
| `{typography.display-lg}` | 36px | 500 | 1.15 | -0.72px | Sub-section / tier |
| `{typography.display-md}` | 28px | 500 | 1.2 | -0.42px | Card title |
| `{typography.heading-lg}` | 22px | 500 | 1.2 | 0 | Compact heading |
| `{typography.heading-md}` | 18px | 500 | 1.4 | 0 | Section sub-heading |
| `{typography.body-lg}` | 18px | 400 | 1.55 | 0 | Lead body |
| `{typography.body-md}` | 16px | 400 | 1.5 | 0 | Default UI body |
| `{typography.button-md}` | 14px | 500 | 1.0 | 0 | Button label |
| `{typography.caption}` | 13px | 400 | 1.45 | 0 | Helper, footnote |
| `{typography.micro}` | 12px | 400 | 1.45 | 0 | Pill label, fine print |
| `{typography.code}` | 14px | 400 | 1.5 | 0 | Code block content |

### Principles
- **Weight 500 across display.** Mid-weight reads as engineered, not decorative.
- **Negative tracking on display.** -1.92px at 64px, scaling proportionally down.
- **Mono for code.** System mono families (Menlo / Monaco); no proprietary mono webfont.

### Note on Font Substitutes
Circular is proprietary. Use **Inter** at weight 500 with `letter-spacing: -1.92px` for display tiers. **Geist Sans** (open source, from Vercel) is another close alternative for both display and body. Avoid Helvetica defaults; they're heavier and lack the geometric warmth.

## Layout

### Spacing System
- **Base unit**: 8px (with 2 / 4 / 12 sub-tokens for fine work).
- **Tokens**: `{spacing.xxs}` 2px · `{spacing.xs}` 4px · `{spacing.sm}` 8px · `{spacing.md}` 12px · `{spacing.lg}` 16px · `{spacing.xl}` 24px · `{spacing.xxl}` 32px · `{spacing.huge}` 64px.
- **Section padding**: 64–96px on large surfaces.
- **Card internal padding**: 32px on feature cards.

### Grid & Container
- Pages center in a ~1280px container with no edge-bleed.
- Tiered card grids collapse 4-up → 2-up → 1-up at 1024 / 768 breakpoints.
- Product UI mockups stack 2-up or render as overlapping panes inside the same container.

### Whitespace Philosophy
Generous 64–96px section padding without atmospheric gradients filling the space. The white canvas is the design.

## Elevation & Depth

| Level | Treatment | Use |
|---|---|---|
| 0 | Flat, 1px hairline | Default cards |
| 1 | `box-shadow: 0 1px 3px rgba(0,0,0,0.06)` | Subtle card lift |
| 2 | `box-shadow: 0 8px 24px rgba(0,0,0,0.08)` | Floating composited UI mockups |
| 3 | `box-shadow: 0 16px 48px rgba(0,0,0,0.12)` | Modal overlays, deep elevation |

Depth comes from **product UI mockups** rather than gradients: stacked dashboard / editor / log panes composite together with subtle Level 2 shadows.

## Shapes

| Token | Value | Use |
|---|---|---|
| `{rounded.xs}` | 4px | Form inputs, hairline tags |
| `{rounded.sm}` | 6px | Buttons (the signature radius), code blocks |
| `{rounded.md}` | 8px | Compact cards, alerts |
| `{rounded.lg}` | 12px | Feature cards, product mockups, dashboard panels |
| `{rounded.xl}` | 16px | Modal dialogs, large container chrome |
| `{rounded.full}` | 9999px | Pill tags, avatars |

## Components

### Buttons
- **`button-primary-green`**: background `{colors.primary}`, text `{colors.on-primary}` (near-black, NOT white), `{typography.button-md}`, padding 8px 16px, `{rounded.sm}`. Pressed: `{colors.primary-deep}`.
- **`button-secondary-outline`**: background `{colors.canvas}`, text `{colors.ink}`, 1px `{colors.hairline-strong}` border, same shape.
- **`button-on-dark`**: background `{colors.canvas-night}`, text `{colors.on-dark}`, same shape.
- **`button-link`**: transparent, `{colors.ink}`, `{typography.button-md}`, underline on hover.

### Cards & Containers
- **`card-feature-light`**: `{colors.canvas}`, padding 32px, `{rounded.lg}`, 1px `{colors.hairline}`.
- **`card-feature-dark`**: `{colors.canvas-night}`, `{colors.on-dark}`, padding 32px, `{rounded.lg}`; for code-heavy explanations.
- **`card-featured`**: inverted dark tier, `{colors.canvas-night}` with `{colors.on-dark}`.
- **`code-block`**: `{colors.canvas-night}`, `{colors.on-dark}`, `{typography.code}`, padding 16px, `{rounded.sm}`.

### Inputs, Navigation, Pills
- **`text-input`**: `{colors.canvas}`, `{colors.ink}`, `{typography.body-md}`, padding 8px 12px, `{rounded.sm}`, 1px `{colors.hairline}`.
- **`nav-bar-light`**: `{colors.canvas}`, padding 16px 24px; logo left, nav center, one filled `button-primary-green` right.
- **`pill-tag-green`**: `{colors.primary}` / `{colors.on-primary}`, `{typography.micro}`, padding 2px 8px, `{rounded.full}`.
- **`pill-tag-soft`**: `{colors.canvas-soft}` / `{colors.ink}`, same shape.
- **`link-on-light`**: `{colors.ink}` with a persistent underline.
- **`footer-light`**: `{colors.canvas}`, `{colors.ink-mute}`, `{typography.caption}`, padding 64px 24px.

## Do's and Don'ts

### Do
- Reserve `{colors.primary}` emerald for filled CTAs, the wordmark accent and the primary data series; it should appear sparingly.
- Render display tiers at weight 500 with negative letter-spacing.
- Use `{rounded.sm}` 6px for buttons, never pill-shaped.
- Composite product UI mockups inside `{rounded.lg}` containers with subtle Level 2 shadows.
- Use near-black `{colors.ink}` on the emerald button, not white.
- Use system mono for every code block.

### Don't
- Don't introduce additional accent colors as system colors; purples, yellows and pinks belong to chart points only.
- Don't push display weight above 500.
- Don't use white text on the emerald button.
- Don't add atmospheric gradients; the white canvas is the design.

## Responsive Behavior

| Name | Width | Key Changes |
|---|---|---|
| Wide | ≥ 1440px | Full container width |
| Desktop | 1024–1440px | Default content max-width; grids 4-up |
| Tablet | 768–1023px | Grids 2-up; mockups simplify to a single panel |
| Mobile | < 768px | Grids 1-up; hamburger nav; display drops 64 → 36px |

- Buttons and form fields keep a ≥ 36×36px touch target.
- Display tiers stair-step 64 → 48 → 36 → 28 → 22px.

## Iteration Guide

1. Focus on one component at a time.
2. Reference component names and tokens directly.
3. Run `npx @google/design.md lint DESIGN.md` after edits.
4. Default body to `{typography.body-md}`; use `{typography.code}` for any developer-facing snippet.
5. Keep emerald scarce: one filled green element per viewport.
6. The white-canvas commitment is non-negotiable.

---

## Applying it to Ferry

### Grafana dashboard (M3)

Grafana panels take explicit colours. Use these mappings so the dashboard reads
as one system:

| Role | Token | Hex | Used for |
| --- | --- | --- | --- |
| Primary series | `primary` | `#3ecf8e` | Request rate, success rate, healthy endpoints, "Programmed" |
| Primary series, secondary | `primary-deep` | `#24b47e` | Second success series (e.g. p50 next to p99) |
| Neutral series | `ink-mute` | `#707070` | Upstream latency, baselines, previous period |
| Neutral series, light | `hairline-strong` | `#c7c7c7` | Thresholds, reference lines |
| Extra series 1–3 | `accent-violet`, `accent-purple`, `accent-yellow` | `#644fc1`, `#6b01c2`, `#ffdb13` | Additional backends or routes in one panel |
| Error / alert | *(status, not brand)* | `#e5484d` | 5xx rate, ejected endpoints, NACKed config |
| Warning | *(status, not brand)* | `#f5a524` | Retry-budget exhaustion, cert expiry < 14 days |

Rules: one emerald series per panel; errors are never emerald; stat panels use
`ink` text on white with an emerald or red value only when it carries status.
Panels use the light theme by default.

### Diagrams (Mermaid in Markdown)

```text
%%{init: {"theme": "base", "themeVariables": {
  "fontFamily": "Inter, Helvetica Neue, Arial",
  "primaryColor": "#ffffff", "primaryTextColor": "#171717",
  "primaryBorderColor": "#dfdfdf", "lineColor": "#707070",
  "secondaryColor": "#fafafa", "tertiaryColor": "#ffffff"}}}%%
```

Give the Ferry node itself `stroke:#3ecf8e,stroke-width:2px` via a `classDef`.
Everything else stays monochrome.
