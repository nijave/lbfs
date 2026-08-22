//! The lbfs client, as a library.
//!
//! A library and not only a binary because the multiplexer's tests drive it
//! from outside — a scripted server on a loopback socket, feeding the client
//! frames a real server never would (spec §10 layer 1). An integration test is
//! its own crate, so it can only reach a library target.

#![deny(unsafe_code)]

pub mod conn;
pub mod fuse;
