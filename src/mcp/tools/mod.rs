//! The `#[tool]` groups: one `#[tool_router]` impl per vocabulary area.
//!
//! rmcp builds a router from ONE impl block, so `mcp::MemoryServer::tool_router` composes these
//! (soma/ellmephant#50). Moving a tool between groups is moving its block.

pub(super) mod channel;
pub(super) mod claim;
pub(super) mod mail;
pub(super) mod pen;
pub(super) mod project;
pub(super) mod read;
pub(super) mod write;
