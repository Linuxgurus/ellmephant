//! MCP stdio server (P7) on `rmcp`.
//!
//! Tools: `memory_add`, `memory_update`, `memory_delete`, `memory_search`,
//! `memory_get`, `memory_backlinks`, `memory_export`, `memory_import` — with
//! read-only annotations on the read tools and destructive on `delete`. The tool
//! descriptions carry the SPEC §9 write policy; that is where memory quality is
//! actually tuned.

use crate::error::{Error, Result};

/// Names of the tools this server will expose.
pub const TOOLS: &[&str] = &[
    "memory_add",
    "memory_update",
    "memory_delete",
    "memory_search",
    "memory_get",
    "memory_backlinks",
    "memory_export",
    "memory_import",
];

/// Serve MCP over stdio until the client disconnects.
pub fn serve() -> Result<()> {
    Err(Error::NotImplemented("mcp stdio server (PLAN.md P7)"))
}
