//! `midnight-node-updater` — a process manager for [midnight-node] that automates node
//! binary switches at chain upgrades.
//!
//! Midnight is a Substrate chain: runtime (WASM) upgrades are enacted on-chain,
//! but some upgrades also need a new *node* binary (new host functions, client
//! changes, hard forks). Nothing on chain signals when to swap the binary, so
//! `midnight-node-updater` watches the node itself over JSON-RPC and switches
//! binaries when an operator-registered upgrade plan's trigger (block height
//! and/or runtime `spec_version`) is reached.
//!
//! [midnight-node]: https://github.com/midnightntwrk/midnight-node

pub mod backup;
pub mod commands;
pub mod config;
pub mod download;
pub mod layout;
pub mod plan;
pub mod rpc;
pub mod state;
pub mod supervisor;
