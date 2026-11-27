//! Measures the folder size walk.
//!
//! ```text
//! cargo run --release -p orbit-macos --example folder_size -- PATH [std|bulk]
//! ```
//!
//! Prints the time, the bytes and the unreadable count. `bulk` is the
//! default, as in the app.

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use orbit_macos::platform::folder_size::{Reader, Walk, folder_size_by};

    let mut args = std::env::args_os().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: folder_size PATH [std|bulk]");
        return ExitCode::FAILURE;
    };
    let reader = match args.next().as_deref().and_then(|mode| mode.to_str()) {
        None | Some("bulk") => Reader::Bulk,
        Some("std") => Reader::Std,
        Some(other) => {
            eprintln!("unknown reader {other}: std or bulk");
            return ExitCode::FAILURE;
        }
    };
    let started = Instant::now();
    let walk = folder_size_by(
        reader,
        path.as_ref(),
        &AtomicBool::new(false),
        Duration::from_secs(1),
        |bytes| eprintln!("  {:.1?} … {bytes} bytes", started.elapsed()),
    );
    let elapsed = started.elapsed();
    match walk {
        Walk::Done(size) => {
            println!(
                "{reader:?}: {elapsed:.2?}, {} bytes, {} unreadable",
                size.bytes, size.unreadable
            );
            ExitCode::SUCCESS
        }
        Walk::Cancelled => {
            eprintln!("cancelled");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {}
