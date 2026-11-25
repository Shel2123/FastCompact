#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod view;

#[cfg(target_os = "macos")]
fn main() {
    app::launch()
}

#[cfg(not(target_os = "macos"))]
fn main() {}
