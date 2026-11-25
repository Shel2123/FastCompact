//! Logging to stderr and to a file.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Logs to stderr and to the file at `path`. When the file cannot be
/// opened, logs to stderr only and returns the error.
pub fn init(path: io::Result<PathBuf>) -> io::Result<()> {
    let stderr = tracing_subscriber::fmt::layer().with_writer(io::stderr);
    let file = path.and_then(|path| {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        OpenOptions::new().create(true).append(true).open(path)
    });
    match file {
        Ok(file) => {
            let file_layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(Mutex::new(file));
            tracing_subscriber::registry()
                .with(stderr)
                .with(file_layer)
                .init();
            Ok(())
        }
        Err(err) => {
            tracing_subscriber::registry().with(stderr).init();
            Err(err)
        }
    }
}
