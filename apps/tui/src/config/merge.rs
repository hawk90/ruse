//! Layer precedence and the per-key merge strategies (CFG-MERGE-001).

use std::collections::BTreeMap;

use super::{Effective, KeyDef, LayerKind, Merge, Scope, Source, Value};

/// Fold `contributions` (lowest layer first) by `def`'s merge strategy.
pub(super) fn merge(def: &KeyDef, contributions: &[(Value, Source)]) -> Option<Effective> {
    let last = contributions.last()?;
    match def.merge {
        Merge::Replace => {
            // A machine value pins a locked or machine-scoped key above the user and the workspace.
            let pinned = (def.locked || def.scope == Scope::Machine)
                .then(|| {
                    contributions
                        .iter()
                        .find(|(_, s)| s.layer == LayerKind::Machine)
                })
                .flatten();
            let (value, source) = pinned.unwrap_or(last);
            Some(Effective {
                value: value.clone(),
                source: source.clone(),
            })
        }
        Merge::Append | Merge::SetUnion => {
            let mut out: Vec<String> = def.list_default.iter().map(|s| (*s).to_string()).collect();
            for (v, _) in contributions {
                let Value::List(items) = v else { continue };
                for item in items {
                    if let Some(gone) = item.strip_prefix('!') {
                        out.retain(|x| x != gone);
                    } else {
                        out.push(item.clone());
                    }
                }
            }
            if def.merge == Merge::SetUnion {
                out.sort();
                out.dedup();
            }
            Some(Effective {
                value: Value::List(out),
                source: last.1.clone(),
            })
        }
        Merge::Deep => {
            let mut out = BTreeMap::new();
            for (v, _) in contributions {
                if let Value::Map(m) = v {
                    deep_merge(&mut out, m);
                }
            }
            Some(Effective {
                value: Value::Map(out),
                source: last.1.clone(),
            })
        }
    }
}

fn deep_merge(into: &mut BTreeMap<String, Value>, from: &BTreeMap<String, Value>) {
    for (k, v) in from {
        match (into.get_mut(k), v) {
            (Some(Value::Map(a)), Value::Map(b)) => deep_merge(a, b),
            _ => {
                into.insert(k.clone(), v.clone());
            }
        }
    }
}
