//! Typed agent-service errors. Typed, not stringly (D-041 stability §2): a service returns this enum, not a
//! bare `String`. On the wire it collapses to `{ error: { message } }` via [`fmt::Display`] — the protocol
//! carries a human-readable string, this taxonomy is the agent's internal, matchable failure model.

use std::fmt;

/// Why an agent request could not be served.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AgentError {
    /// The request named a method the agent does not serve.
    UnknownMethod(String),
    /// A required parameter was absent from (or the wrong type in) the request.
    MissingParam {
        method: &'static str,
        field: &'static str,
    },
    /// A service's underlying operation failed (e.g. an IO error while reading a file). `detail` is the
    /// human-readable cause (an OS message), already stringified at the failure site.
    Service {
        method: &'static str,
        detail: String,
    },
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::UnknownMethod(m) => write!(f, "unknown method: {m}"),
            AgentError::MissingParam { method, field } => write!(f, "{method}: missing `{field}`"),
            AgentError::Service { method, detail } => write!(f, "{method}: {detail}"),
        }
    }
}

impl std::error::Error for AgentError {}
