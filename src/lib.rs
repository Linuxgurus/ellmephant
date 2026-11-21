pub mod bench;
pub mod cli;
pub mod config;
pub mod db;
pub mod embed;
pub mod error;
pub mod hex;
pub mod http;
pub mod http_surface;
pub mod identity;
pub mod index;
pub mod mcp;
pub mod model;
pub mod names;
pub mod ops;
pub mod search;
pub mod store;
pub mod wire;

pub use config::{Config, Paths};
pub use error::{Error, Result};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
