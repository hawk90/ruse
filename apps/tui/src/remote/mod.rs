//! F-017 remote runtime, slice 1: the client↔agent transport foundation.
//!
//! The end-state (RFC-0006 / D-029..031) is a thin LOCAL client driving a headless Workspace Agent that runs
//! on a REMOTE host and supervises fs/watch/search/git/pty/lsp/debug services — UI local, execution remote.
//! Slice 1 proves only the load-bearing seam everything else rides on: a versioned, capability-negotiated
//! request/response protocol, with the agent run LOCALLY as a `ruse agent` subprocess over a pipe (SSH
//! transport, agent bootstrap, and remote services are later slices).
//!
//! It deliberately lives as a MODULE here, mirroring `lsp/` (which is structurally the same — framed JSON over
//! a child process's stdio): per RFC-0012 §Re-evaluation, the `workspace-runtime` crate boundary returns only
//! when the agent becomes a separately-deployed remote binary (the SSH/bootstrap slice), not at this local
//! proof.
//!
//! - [`protocol`] — the wire format (`Content-Length` framing, same as `lsp/codec.rs`) + negotiation.
//! - [`agent`] — the headless serve loop (`ruse agent`): read a request, dispatch to a service, reply.
//! - [`client`] — the local side: spawn the agent, handshake, issue blocking calls.
//! - [`transport`] — how the agent is launched: a local subprocess (slice 1) or `ssh host ruse agent` (2a).

pub mod agent;
pub mod client;
pub mod error;
pub mod protocol;
pub mod transport;
