#[cfg(target_os = "macos")]
mod applescript;
#[cfg(target_os = "macos")]
mod ax;
#[cfg(target_os = "macos")]
#[path = "main.rs"]
mod macos;
#[cfg(target_os = "macos")]
mod panel;
#[cfg(target_os = "macos")]
mod secure;
#[cfg(target_os = "macos")]
mod selection;
#[cfg(target_os = "macos")]
mod tap;

#[cfg(target_os = "macos")]
fn main() {
    macos::launch()
}

#[cfg(not(target_os = "macos"))]
fn main() {}
