//! Pit: CI that is never cold.
//!
//! One binary with a control plane (`server`), warm workers (`worker`), a CLI
//! that runs tasks on uncommitted changes (`cli`), and an MCP server for
//! coding agents (`mcp`).

pub mod cas;
pub mod cli;
pub mod client;
pub mod config;
pub mod cron;
pub mod excerpt;
pub mod git;
pub mod graph;
pub mod import;
pub mod init;
pub mod mcp;
pub mod model;
pub mod server;
pub mod slow;
pub mod snapshot;
pub mod worker;
pub mod workspace;
