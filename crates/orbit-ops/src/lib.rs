//! Orbit file operations that work on every platform.
//!
//! Knows nothing about any operating system beyond `cfg(unix)`: a platform
//! crate wraps these operations in its executor and adds what only it can
//! do.

#![warn(missing_docs)]

pub mod archive;
pub mod executor;
pub mod publish;
