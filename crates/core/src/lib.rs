//! ruse-core — kernel: Document/Transaction/Command/Query/Anchor/Undo/Register/Context/Health/EditLang
//! Stub (spec-first, pre-implementation). See docs/design/ + spec/PRD.yaml components.
#![allow(dead_code)]

/// TODO: document — see design docs.
pub mod document {}

/// TODO: transaction — see design docs.
pub mod transaction {}

/// TODO: anchor — see design docs.
pub mod anchor {}

/// TODO: undo — see design docs.
pub mod undo {}

/// TODO: register — see design docs.
pub mod register {}

/// TODO: context — see design docs.
pub mod context {}

/// TODO: command — see design docs.
pub mod command {}

/// TODO: query — see design docs.
pub mod query {}

/// TODO: editlang — see design docs.
pub mod editlang {}

/// TODO: health — see design docs.
pub mod health {}

/// TODO: scheduler — see design docs.
pub mod scheduler {}

/// Reject a transaction whose base revision is stale (INV-TXN, ENG-TXN-001).
/// Stub: real logic lands with the Transaction engine (RFC-0007 / C-TRANSACTION).
pub fn is_stale_revision(base: u64, current: u64) -> bool {
    base < current
}
