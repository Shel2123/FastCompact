#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod jobs;

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    app::launch()
}

#[cfg(not(target_os = "macos"))]
fn main() {}
