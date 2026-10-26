//! Application scenarios, state and ports for Orbit.
//!
//! Does not depend on concrete macOS implementations: adapters send
//! [`messages::AppEvent`]s to the [`controller::AppController`] and carry out
//! the [`messages::Effect`]s it returns.

#![warn(missing_docs)]

pub mod catalog;
pub mod controller;
pub mod messages;
pub mod ports;
pub mod state;
