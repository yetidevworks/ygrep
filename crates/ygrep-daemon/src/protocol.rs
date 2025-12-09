//! IPC protocol for daemon communication
//!
//! TODO: Implement in Phase 4

use serde::{Deserialize, Serialize};

/// Request from client to daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Search the index
    Search {
        query: String,
        limit: Option<usize>,
    },
    /// Get daemon status
    Status,
    /// Ping/health check
    Ping,
    /// Shutdown daemon
    Shutdown,
}

/// Response from daemon to client
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Success { data: serde_json::Value },
    Error { code: String, message: String },
}
