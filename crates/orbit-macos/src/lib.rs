//! AppKit UI and macOS system adapters for Orbit.

pub mod debug;
pub mod mac_platform;
pub mod main_thread;
pub mod operations;
pub mod platform;
pub mod ui;

pub use debug::MacOptions;
pub use mac_platform::{MacPlatform, MacPlatformError};
