#[cfg(target_os = "macos")]
#[path = "../main_thread.rs"]
mod macos;

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    macos::launch()
}

#[cfg(not(target_os = "macos"))]
fn main() {}
