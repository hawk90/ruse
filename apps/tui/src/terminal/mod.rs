//! Terminal lifecycle: the raw-mode/alt-screen guard and the F-010 capability ledger. The rest of
//! the frontend talks to the terminal only through [`guard::TermGuard`]; capability detection is a
//! private detail behind it.

pub(crate) mod capabilities;
pub(crate) mod guard;
