---
doc: ai-assisted-development
project: ruse
title: "ruse — AI-Assisted Development Policy"
summary: >
  ruse's tool-agnostic AI policy. ruse uses AI-assisted development actively, but NO specific AI tool is
  required to contribute. AI use is optional; the PR author understands and is responsible for every change;
  AI output is never test evidence or design rationale; unverified generated code is not merged. Contributors
  disclose significant AI assistance in the PR (scope, not full logs), never feed private code/credentials/PII
  to a tool, and own license/provenance. Includes the PR-template "AI assistance" block.
audience: [maintainers, contributors, llm-agents]
status: draft
related:
  - README.md
  - change-paths.md
  - ../operations/definition-of-done.md
---

# AI-Assisted Development Policy

> **ruse uses AI-assisted development actively, but NO specific AI tool is required to contribute.**

This policy is deliberately **tool-agnostic**. It does not endorse, require, or forbid any particular AI
tool. It governs *responsibility and evidence* — not which keys you press.

## Rules

- **AI use is optional.** Contributing without any AI tool is fully supported and equally welcome.
- **The PR author understands and is responsible for every change** in the PR, regardless of how it was
  produced.
- **AI output is not test evidence or design rationale.** A passing claim from a model proves nothing; only
  real tests, benchmarks, and reasoning in the PR/spec count (see
  [../operations/definition-of-done.md](../operations/definition-of-done.md)).
- **Unverified generated code is not merged.** If the author cannot explain and has not verified it, it does
  not land.
- **Disclose significant AI assistance scope in the PR** using the block below. **Full prompt/conversation
  logs are not required** — scope is.
- **Never input private code, credentials, or PII to an AI tool.**
- **The author owns license and provenance responsibility** for any AI-assisted contribution.

> **AI may multiply implementation capacity; it does not replace engineering judgment.**

## What actually matters

The goal is not to police *whether* AI was used. The questions that gate a merge are:

1. **Who decided the design?** (A human contributor must own the design decision.)
2. **Can the author explain the code?** (If you cannot explain it, it is not ready.)
3. **How was it verified?** (Concrete evidence, not a model's assurance.)

## PR-template "AI assistance" block

Include this block in every PR. Pick one disclosure line and fill the fields if applicable:

```markdown
### AI assistance
- [ ] No significant AI assistance
- [ ] AI-assisted
  - Tool:            <tool / model, or "unspecified">
  - Scope:           <what the AI helped with — e.g. drafting tests, refactor, boilerplate>
  - Human verification performed: <how you verified — tests run, manual review, reasoning checked>
```

## Diagnose, don't re-delegate

When something fails — a test, a build, a subtle bug — **diagnose the failure yourself** rather than
repeatedly handing the unexplained failure back to an AI agent and hoping. Understanding the root cause is
the contribution; loops of "try again" without comprehension produce code the author cannot stand behind,
which by the rules above cannot be merged.
