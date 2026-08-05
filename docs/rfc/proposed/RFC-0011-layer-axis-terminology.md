---
doc: rfc
project: ruse
title: "RFC-0011: Disambiguate the overloaded \"layer\" terminology"
summary: >
  "layer" is used for at least four independent axes across the spec — the architecture tier a capability
  IS (capabilities `architecture_layer`), the build-stage bring-up ORDER of components (PRD component
  `layer`), the runtime LOCATION code runs in (`runtime`), and the trust DOMAIN (`trust`). A dependency
  conclusion is only valid within ONE axis, so the shared word invites wrong conclusions by both humans and
  LLMs. This RFC renames the two machine fields that literally use the word "layer" to their axis name —
  PRD component `layer` → `build_stage`, capabilities `architecture_layer` → `architecture_tier` — and adds
  the four axis terms to the glossary. `runtime`/`trust` already have unambiguous names and are unchanged;
  `product_layer` (delivery axis) is a distinct compound and is left as-is for now.
audience: [maintainers, contributors, llm-agents]
status: proposed
related:
  - ../../../spec/glossary.yaml
  - ../../../spec/PRD.yaml
  - ../../../spec/capabilities.yaml
  - ../../operations/spec-validate.md
---

# RFC-0011: Disambiguate the overloaded "layer" terminology

- **Status:** proposed
- **Decision link:** D-036

## Summary

Rename the two machine fields that literally spell "layer" to the axis they mean — PRD component `layer` →
`build_stage`, capabilities `architecture_layer` → `architecture_tier` — and register the four axes in the
glossary. This is a mechanical, behavior-preserving rename of spec field keys plus the validator/model that
read them; it changes no invariant and no runtime behavior.

## Motivation / Problem

The word "layer" names at least four independent axes:

| Axis | Question | Field today | Values |
| --- | --- | --- | --- |
| architecture tier | what a capability *is* | `architecture_layer` | kernel / service / bundled-extension / external-plugin / external-tool |
| build stage | bring-up *order* of components | PRD component `layer` | kernel < input < tui < workspace < plugin < remote |
| runtime location | *where* it runs | `runtime` (ok) | client / workspace / both |
| trust domain | *who* is trusted | `trust` (ok) | core / official / untrusted |

A dependency or ordering conclusion is only valid *within one axis*. Sharing the word "layer" across three
of them (`architecture_layer`, PRD `layer`, and prose "layer") makes it easy — for a contributor or an
agent reading the spec — to draw a cross-axis conclusion that is simply invalid (e.g. treating a build-stage
ordering as an architecture-tier isolation boundary). As the project moves to **multiple contributors**, the
cost of this ambiguity rises.

## Guide-level explanation

- A **component**'s `build_stage` is its position in the reference implementation's bring-up order
  (`kernel < input < tui < workspace < plugin < remote`); a component may not depend on a later stage.
- A **capability**'s `architecture_tier` is what it *is* by isolation/trust boundary
  (`kernel | service | bundled-extension | external-plugin | external-tool`).
- `runtime` (location) and `trust` (domain) keep their names.
- The glossary's `layer` entry is marked ambiguous and points at the four axis terms.

## Reference-level explanation

Field-key rename only (values unchanged):
- `spec/PRD.yaml`: every component `layer:` → `build_stage:`.
- `spec/capabilities.yaml`: `architecture_layer:` → `architecture_tier:`.
- `tools/spec-validate.py`: read `build_stage`; the build-stage order check and the `CAP_ENUMS`
  `architecture_tier` key are renamed accordingly.
- `tools/rusekit/model.py`: component node `meta` uses `build_stage`.
- `spec/glossary.yaml`, `docs/README.md`, `docs/operations/spec-validate.md`: reference the new field names.

## Reference Invariants

None introduced or changed. `INV-NO-GLOBAL-STATE`, `INV-PLUGIN-NO-CORE` and the ARCH forbidden-dependency
rules are unaffected — this only renames the keys used to *describe* structure, not the structure.

## Failure modes & Recovery

The rename is atomic: the spec fields and the validator that reads them land in one commit, so
`spec-validate` stays green at every commit. A missed reference surfaces as a `spec validate` failure (bad
`build_stage`) rather than a silent mis-parse.

## Security impact
None.

## Performance impact
None.

## Compatibility & Migration

Pre-1.0, spec-internal. No external consumer depends on these field names yet. `product_layer` is
intentionally left unchanged (distinct compound, low ambiguity); a later RFC may revisit it and the
`dependencies.yaml` `allowed_layers` key (which actually lists *crates*).

## Observability
`spec validate` reports any unresolved reference; the glossary is the one home for the axis definitions.

## Alternatives

- **Keep "layer", document the axes only.** Rejected: the ambiguity is in the field keys themselves, which
  LLMs pattern-match on; a glossary note alone (already added) helps but does not remove the trap.
- **Rename all five including `product_layer` and `allowed_layers`.** Deferred to keep this change minimal
  and reviewable; those are lower-ambiguity and can follow.

## Trade-offs

A one-time rename churn (spec + two tool files) buys a vocabulary where every "layer"-shaped conclusion is
forced to name its axis.
