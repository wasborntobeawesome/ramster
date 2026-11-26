//! ramster core: probing, analysis, rules, actions, history, MCP and the daemon.
//! Used by the `ramster` CLI and the `ramster-bar` menu bar app.

pub mod analyze;
pub mod config;
pub mod daemon;
pub mod history;
pub mod mcp;
pub mod probe;
pub mod ui;
pub mod util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
