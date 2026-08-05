# Templates

Copy the right template when adding an entry. Keep IDs stable, enums closed, summaries to 1–2 sentences,
and never hand-edit generated files.

| Template | Use when | Target file |
| --- | --- | --- |
| [prd-feature.yaml](prd-feature.yaml) | Adding a requirement/feature | `spec/PRD.yaml` (`features:` map) |
| [policy-principle.yaml](policy-principle.yaml) | Adding an enforced rule | `spec/POLICY.yaml` (`principles:` map) |
| [decision.md](decision.md) | Recording a hard-to-reverse decision | `spec/DECISIONS.md` |
| [rfc.md](rfc.md) | Proposing a big/irreversible change | `docs/rfc/proposed/RFC-xxxx-*.md` |
| [design-doc.md](design-doc.md) | Writing a new design/reference doc | `docs/design/*` |

RFCs are only for hard-to-reverse decisions (save format, plugin protocol, command semantics, document/
transaction boundary, remote protocol, compatibility policy). Small changes → Git commit/PR description.
