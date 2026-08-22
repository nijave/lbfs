//! The lbfs server, as a library.
//!
//! The binary in `main.rs` is a thin wrapper: parse arguments, read the config,
//! bind the listener, hand it to [`rpc::serve`]. Everything else lives here so
//! the integration suite can start a server in-process on an OS-assigned port
//! and speak frames to it, instead of shelling out to a binary and guessing
//! when it is ready.
//!
//! The three layers, outside in:
//!
//! * [`config`] — the TOML file and the export allowlist.
//! * [`rpc`] — the wire: accept loop, per-connection session, opcode dispatch.
//! * [`fs`] — the backend boundary ([`fs::FileSystem`]) and the one
//!   implementation of it ([`fs::local::LocalFs`]).

#![deny(unsafe_code)]

pub mod config;
pub mod fs;
pub mod rpc;
