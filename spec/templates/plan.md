# Plan — <issue>

<!-- Written by the Planner (human or AI in a read-only role) BEFORE implementation.
     `python3 tools/ruse.py plan validate .ruse/work/<issue>/plan.md` checks that every
     section below is present and non-empty. It cannot judge whether the plan is *correct*
     — only whether a perspective was skipped. A human approves the plan before Execute. -->

## Goal
<the observable outcome; matches change.yaml goal>

## Non-goals
- <out of scope>

## Assumptions
- <what must hold for this plan to work>

## Affected spec IDs
- <F-* / C-* / CAP-* / INV-* / D-* — the same set the impact analysis surfaced>

## Expected files
- <files this change is expected to create or modify — the intended blast radius>

## Invariants to preserve
- <INV-* that must still hold after the change; how each is protected>

## Implementation steps
1. <ordered, reviewable steps>

## Tests
- <the concrete evidence that will prove it: unit / property / differential / fixture>

## Failure handling
- <what happens on the error paths; typed errors vs asserts (ENG-ERR-001)>

## Compatibility
- <public API / protocol / persistent format impact; migration story, or "none">

## Rollback
- <how to back this out if it regresses>

## Open questions
- <unresolved decisions for the human approver>
