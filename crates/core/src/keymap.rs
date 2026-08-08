//! Keymap resolution: one ordered layer stack (D-045, `spec/parity/contracts/keymap-layers.yaml`).
//!
//! # Why a stack and not a map per mode
//!
//! Three upstream censuses disagree about keymaps in a way that a flat map per mode cannot absorb:
//!
//! - **Neovim** declares eight *disjoint* namespaces (`runtime/doc/map.txt`); exactly one is active
//!   and it is selected by editor **state**.
//! - **Emacs** consults a *nine-tier precedence stack*, selected by what the **buffer** is — and
//!   613 of its 1,952 keyboard bindings live in major-mode maps, a tier Vim's model has no seat for.
//! - **Helix** builds `Select` as `normal.clone()` + `merge_nodes(overrides)`: 301 inherited
//!   bindings under a 33-binding diff.
//!
//! All three are cases of one ordered layer stack — Vim is depth-1 sealed, Emacs is depth-9
//! unsealed, and Helix is a depth-2 `[override, base]` collapsed at *build* time. The layered model
//! strictly contains the other two, so it costs one implementation where the disjoint model costs
//! two: a disjoint router must either duplicate Helix's 301 inherited bindings or grow a bespoke
//! inheritance feature, and has nowhere at all to put Emacs's major-mode tier.
//!
//! # The defect this replaces
//!
//! `apps/tui/src/input.rs` special-cases Insert ahead of a single `Feed::Ignored` fallthrough. That
//! is one `closed/ignore` policy standing in for the five *open* policies the Vim contract
//! enumerates — the same shape as the original catalog defect, where modes were recorded as
//! transitions and the unmatched-key axis had nowhere to live. Here the policy is a required field
//! of every layer ([`UnmatchedKey`]), so a layer cannot exist without declaring one.
//!
//! # What this module does NOT yet do
//!
//! Stated rather than implied, because a partially-built primitive that reads as complete is worse
//! than an obviously partial one:
//!
//! - **KL-OBL-4** (a layer *owns* its state and dies with it) is not modelled. Layers here carry
//!   bindings, not state; the engine still holds count/operator/awaiting centrally.
//! - **KL-OBL-5** (return is a stack with an address) is not modelled. `i_CTRL-O` needs a return
//!   address on the *activation* stack, which is a separate structure from resolution order.
//! - **KL-Q-LANG-ARG** — `Translate` is representable as a policy but resolution stays total here;
//!   a layer that rewrites an event and re-dispatches is not implemented, and that is D-045's own
//!   recorded re-evaluation trigger.

use std::fmt;

/// What a layer does with a key it does not bind.
///
/// The vocabulary is the census's, not ours: `spec/parity/contracts/vim-style.yaml` derives these
/// eight from `runtime/doc/map.txt`, and five of the eight Vim namespaces are *open* — the key still
/// does something. An engine-wide default would silently make all eight `Ignore`, which is exactly
/// the shipped bug (KL-OBL-2), so this is a required field with no `Default`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnmatchedKey {
    /// `closed/ignore` — nothing happens (a bell at most). Vim Normal/Visual.
    Ignore,
    /// `closed/abort` — cancel the pending construct and return to the base layer. Vim
    /// operator-pending: the distinction from [`Ignore`](Self::Ignore) is the one the current
    /// engine cannot express, because operator-pending is a field rather than a layer.
    Abort,
    /// `open/insert` — a printable key is inserted literally. Vim Insert.
    Insert,
    /// `open/append` — a printable key is appended to the command line. Vim Cmdline.
    Append,
    /// `open/overwrite` — a printable key overwrites; backspace restores the original. Vim Replace.
    Overwrite,
    /// `open/replace-selection` — delete the selection, insert, enter Insert. Vim Select.
    ReplaceSelection,
    /// `open/forward` — forward the key verbatim to a job, except one escape prefix. Vim Terminal.
    Forward,
    /// `open/translate` — rewrite through the active language map, then re-dispatch. Vim Lang-Arg.
    /// Representable but not yet resolvable; see the module note on KL-Q-LANG-ARG.
    Translate,
}

impl UnmatchedKey {
    /// Whether an unmatched key still *does* something. The five open policies are the ones a shared
    /// fallthrough erases.
    #[must_use]
    pub fn is_open(self) -> bool {
        !matches!(self, UnmatchedKey::Ignore | UnmatchedKey::Abort)
    }
}

/// Why a stack could not be built. Typed, not stringly (D-041).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum KeymapError {
    /// Two layers claim the same rank. The contract calls this a *definition* error rather than a
    /// runtime tiebreak: resolution order that depends on insertion order is order nobody declared.
    DuplicateRank {
        rank: u16,
        existing: &'static str,
        incoming: &'static str,
    },
    /// A layer id was registered twice.
    DuplicateId { id: &'static str },
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapError::DuplicateRank {
                rank,
                existing,
                incoming,
            } => write!(
                f,
                "layers `{existing}` and `{incoming}` both claim rank {rank}; \
                 resolution order must be declared, not decided by insertion order"
            ),
            KeymapError::DuplicateId { id } => write!(f, "layer `{id}` registered twice"),
        }
    }
}

/// One keymap layer: bindings plus the two fields that make the stack meaningful — whether it
/// *seals* resolution, and what it does with a key it does not bind.
#[derive(Clone, Debug)]
pub struct Layer<K, V> {
    id: &'static str,
    rank: u16,
    sealed: bool,
    unmatched: UnmatchedKey,
    bindings: Vec<(K, V)>,
}

impl<K: PartialEq, V> Layer<K, V> {
    /// A layer must name its policy at construction — there is no builder default (KL-OBL-2).
    #[must_use]
    pub fn new(id: &'static str, rank: u16, sealed: bool, unmatched: UnmatchedKey) -> Layer<K, V> {
        Layer {
            id,
            rank,
            sealed,
            unmatched,
            bindings: Vec::new(),
        }
    }

    /// Bind a key *in this layer*. There is no unqualified "bind key K" anywhere in this API, which
    /// is KL-OBL-1 expressed as a type rather than as a convention.
    #[must_use]
    pub fn bind(mut self, key: K, value: V) -> Layer<K, V> {
        self.bindings.push((key, value));
        self
    }

    #[must_use]
    pub fn id(&self) -> &'static str {
        self.id
    }

    #[must_use]
    pub fn rank(&self) -> u16 {
        self.rank
    }

    #[must_use]
    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    #[must_use]
    pub fn unmatched(&self) -> UnmatchedKey {
        self.unmatched
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.bindings.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// The outcome of resolving one key against a stack.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Resolved<'a, V> {
    /// A layer bound the key. The layer id travels with the value so a caller can never attribute a
    /// binding to the wrong namespace.
    Bound { layer: &'static str, value: &'a V },
    /// No layer bound it. `layer` is the last one consulted — the sealed layer that stopped the
    /// walk, or the bottom of the stack — and `policy` is *that layer's* declared policy, never a
    /// shared default.
    Unmatched {
        layer: &'static str,
        policy: UnmatchedKey,
    },
    /// The stack is empty. A distinct variant rather than silently reporting `Ignore`: "no layers
    /// are active" is a configuration bug, and reporting it as a policy would hide it.
    NoLayer,
}

/// An ordered stack of layers, consulted highest rank first.
#[derive(Clone, Debug, Default)]
pub struct LayerStack<K, V> {
    /// Kept sorted by descending rank, so resolution is a plain walk and the invariant lives in one
    /// place rather than in every reader.
    layers: Vec<Layer<K, V>>,
}

impl<K: PartialEq, V> LayerStack<K, V> {
    #[must_use]
    pub fn new() -> LayerStack<K, V> {
        LayerStack { layers: Vec::new() }
    }

    /// Add a layer, keeping the stack sorted by descending rank.
    ///
    /// # Errors
    /// [`KeymapError::DuplicateRank`] or [`KeymapError::DuplicateId`] — both are definition errors.
    pub fn push(&mut self, layer: Layer<K, V>) -> Result<(), KeymapError> {
        if let Some(other) = self.layers.iter().find(|l| l.rank == layer.rank) {
            return Err(KeymapError::DuplicateRank {
                rank: layer.rank,
                existing: other.id,
                incoming: layer.id,
            });
        }
        if self.layers.iter().any(|l| l.id == layer.id) {
            return Err(KeymapError::DuplicateId { id: layer.id });
        }
        let at = self
            .layers
            .iter()
            .position(|l| l.rank < layer.rank)
            .unwrap_or(self.layers.len());
        self.layers.insert(at, layer);
        Ok(())
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        self.layers.len()
    }

    #[must_use]
    pub fn layer(&self, id: &str) -> Option<&Layer<K, V>> {
        self.layers.iter().find(|l| l.id == id)
    }

    /// Layer ids in resolution order (highest rank first).
    pub fn order(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.layers.iter().map(|l| l.id)
    }

    /// Resolve one key: walk highest-rank-first until a layer binds; a sealed layer stops the walk
    /// whether or not it bound.
    pub fn resolve(&self, key: &K) -> Resolved<'_, V> {
        let mut last: Option<&Layer<K, V>> = None;
        for layer in &self.layers {
            if let Some(value) = layer.get(key) {
                return Resolved::Bound {
                    layer: layer.id,
                    value,
                };
            }
            last = Some(layer);
            if layer.sealed {
                break;
            }
        }
        match last {
            Some(l) => Resolved::Unmatched {
                layer: l.id,
                policy: l.unmatched,
            },
            None => Resolved::NoLayer,
        }
    }
}

impl<K: PartialEq + Clone, V: Clone> LayerStack<K, V> {
    /// Collapse the stack into one layer with identical resolution — the *build-time* form of a
    /// derived map (Helix's `normal.clone()` + `merge_nodes`).
    ///
    /// KL-OBL-6 requires this to be observably identical to consulting the layers at dispatch, which
    /// is what makes it an optimisation rather than a second dispatch model. The collapsed layer
    /// inherits the identity and policy of the layer that would have terminated the walk — the
    /// sealed layer, or the bottom — because that is the layer whose policy a miss would have hit.
    #[must_use]
    pub fn collapse(&self, id: &'static str) -> Layer<K, V> {
        let terminal = self
            .layers
            .iter()
            .find(|l| l.sealed)
            .or_else(|| self.layers.last());
        let (rank, sealed, unmatched) = match terminal {
            Some(l) => (
                self.layers.first().map_or(l.rank, |f| f.rank),
                true,
                l.unmatched,
            ),
            None => (0, true, UnmatchedKey::Ignore),
        };
        let mut out: Layer<K, V> = Layer::new(id, rank, sealed, unmatched);
        // Highest rank first, and `get` takes the FIRST match, so pushing in resolution order
        // reproduces precedence without needing to dedupe.
        for layer in &self.layers {
            for (k, v) in &layer.bindings {
                out.bindings.push((k.clone(), v.clone()));
            }
            if layer.sealed {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack(layers: Vec<Layer<char, &'static str>>) -> LayerStack<char, &'static str> {
        let mut s = LayerStack::new();
        for l in layers {
            s.push(l).expect("test stack must be well-defined");
        }
        s
    }

    /// Vim: eight sealed layers, exactly one active. Resolution must never reach past it, which is
    /// the property every VS-OBL rests on (KL-OBL-3).
    #[test]
    fn depth_one_sealed_never_falls_through() {
        let s = stack(vec![
            Layer::new("normal", 10, true, UnmatchedKey::Ignore).bind('d', "delete")
        ]);
        assert_eq!(s.depth(), 1);
        assert!(matches!(
            s.resolve(&'d'),
            Resolved::Bound {
                layer: "normal",
                ..
            }
        ));
        assert_eq!(
            s.resolve(&'q'),
            Resolved::Unmatched {
                layer: "normal",
                policy: UnmatchedKey::Ignore
            }
        );
    }

    /// A sealed layer stops the walk even when a lower layer WOULD have bound the key. Without this
    /// the eight Vim namespaces stop being disjoint the moment anything is installed beneath them.
    #[test]
    fn sealed_layer_hides_lower_bindings() {
        let s = stack(vec![
            Layer::new("insert", 20, true, UnmatchedKey::Insert),
            Layer::new("global", 10, false, UnmatchedKey::Ignore).bind('d', "delete"),
        ]);
        assert_eq!(
            s.resolve(&'d'),
            Resolved::Unmatched {
                layer: "insert",
                policy: UnmatchedKey::Insert
            }
        );
    }

    /// Emacs: unsealed tiers, all consulted in order until one binds.
    #[test]
    fn unsealed_stack_falls_through_in_rank_order() {
        let s = stack(vec![
            Layer::new("minor", 30, false, UnmatchedKey::Ignore).bind('a', "minor-a"),
            Layer::new("major", 20, false, UnmatchedKey::Ignore)
                .bind('a', "major-a")
                .bind('b', "major-b"),
            Layer::new("global", 10, false, UnmatchedKey::Ignore)
                .bind('b', "global-b")
                .bind('c', "global-c"),
        ]);
        assert!(matches!(
            s.resolve(&'a'),
            Resolved::Bound {
                value: &"minor-a",
                ..
            }
        ));
        assert!(matches!(
            s.resolve(&'b'),
            Resolved::Bound {
                value: &"major-b",
                ..
            }
        ));
        assert!(matches!(
            s.resolve(&'c'),
            Resolved::Bound {
                value: &"global-c",
                ..
            }
        ));
        // A miss reports the BOTTOM layer's policy — the last one actually consulted.
        assert_eq!(
            s.resolve(&'z'),
            Resolved::Unmatched {
                layer: "global",
                policy: UnmatchedKey::Ignore
            }
        );
    }

    /// Insertion order must not decide resolution order (KL-OBL-3 / `DuplicateRank`).
    #[test]
    fn rank_decides_order_not_insertion() {
        let mut a = LayerStack::new();
        a.push(Layer::new("low", 10, false, UnmatchedKey::Ignore).bind('x', "low"))
            .unwrap();
        a.push(Layer::new("high", 20, false, UnmatchedKey::Ignore).bind('x', "high"))
            .unwrap();
        let mut b = LayerStack::new();
        b.push(Layer::new("high", 20, false, UnmatchedKey::Ignore).bind('x', "high"))
            .unwrap();
        b.push(Layer::new("low", 10, false, UnmatchedKey::Ignore).bind('x', "low"))
            .unwrap();
        assert_eq!(a.order().collect::<Vec<_>>(), b.order().collect::<Vec<_>>());
        assert_eq!(a.resolve(&'x'), b.resolve(&'x'));
    }

    #[test]
    fn duplicate_rank_is_a_definition_error() {
        let mut s: LayerStack<char, &str> = LayerStack::new();
        s.push(Layer::new("a", 10, false, UnmatchedKey::Ignore))
            .unwrap();
        assert_eq!(
            s.push(Layer::new("b", 10, false, UnmatchedKey::Ignore)),
            Err(KeymapError::DuplicateRank {
                rank: 10,
                existing: "a",
                incoming: "b"
            })
        );
    }

    #[test]
    fn empty_stack_is_not_silently_ignore() {
        let s: LayerStack<char, &str> = LayerStack::new();
        assert_eq!(s.resolve(&'x'), Resolved::NoLayer);
    }

    /// KL-OBL-6: Helix's derived Select — `[override, base]` — must resolve identically whether the
    /// merge happens at build time or at dispatch. This is the property that keeps build-time
    /// collapse an optimisation instead of a second dispatch model.
    #[test]
    fn collapse_is_observably_identical_to_dispatch() {
        let s = stack(vec![
            Layer::new("select-override", 20, false, UnmatchedKey::Ignore)
                .bind('h', "extend_char_left"),
            Layer::new("normal-base", 10, true, UnmatchedKey::Ignore)
                .bind('h', "move_char_left")
                .bind('w', "move_next_word_start"),
        ]);
        let collapsed = stack(vec![s.collapse("select")]);
        for key in ['h', 'w', 'z'] {
            match (s.resolve(&key), collapsed.resolve(&key)) {
                (Resolved::Bound { value: a, .. }, Resolved::Bound { value: b, .. }) => {
                    assert_eq!(a, b, "collapse changed the binding for {key:?}");
                }
                (Resolved::Unmatched { policy: a, .. }, Resolved::Unmatched { policy: b, .. }) => {
                    assert_eq!(a, b, "collapse changed the policy for {key:?}")
                }
                (x, y) => panic!("collapse changed the outcome kind for {key:?}: {x:?} vs {y:?}"),
            }
        }
        // The whole point of the derived shape: inherited bindings are not duplicated by hand.
        assert!(matches!(
            collapsed.resolve(&'w'),
            Resolved::Bound {
                value: &"move_next_word_start",
                ..
            }
        ));
    }

    /// The five open policies must be distinguishable from the two closed ones — the axis a shared
    /// `Feed::Ignored` fallthrough erases.
    #[test]
    fn open_and_closed_policies_are_distinct() {
        for p in [UnmatchedKey::Ignore, UnmatchedKey::Abort] {
            assert!(!p.is_open(), "{p:?} must be closed");
        }
        for p in [
            UnmatchedKey::Insert,
            UnmatchedKey::Append,
            UnmatchedKey::Overwrite,
            UnmatchedKey::ReplaceSelection,
            UnmatchedKey::Forward,
            UnmatchedKey::Translate,
        ] {
            assert!(p.is_open(), "{p:?} must be open");
        }
    }
}
