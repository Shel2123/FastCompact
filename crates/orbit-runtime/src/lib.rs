//! Orbit runtime: the controller, the threads that carry out its effects,
//! and the [`Platform`] that supplies everything specific to an operating
//! system.
//!
//! Knows nothing about any operating system: a platform crate implements
//! [`Platform`] and the binary hands it to [`run`].

#![warn(missing_docs)]

mod effects;
mod jobs;
mod platform;
mod resolver;
mod runtime;
#[cfg(test)]
mod testing;

pub use jobs::JobsConfig;
pub use platform::{Handler, Platform, PlatformError, ResolverFactory, Views};
pub use runtime::{RunError, Runtime, RuntimeConfig, run};
