pub mod buffers;

// The io_uring bridge is the single sanctioned home for `unsafe` in this
// workspace: raw SQE submission and raw pointers into slab-owned payloads
// cannot be expressed safely. Every other module inherits the crate root's
// `#![deny(unsafe_code)]`.
#[allow(unsafe_code)]
pub mod uring;
