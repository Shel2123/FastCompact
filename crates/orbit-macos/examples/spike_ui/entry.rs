#[cfg(target_os = "macos")]
#[path = "main.rs"]
mod macos;
#[cfg(target_os = "macos")]
mod view;

#[cfg(target_os = "macos")]
fn main() {
    macos::launch()
}

#[cfg(not(target_os = "macos"))]
fn main() {}
