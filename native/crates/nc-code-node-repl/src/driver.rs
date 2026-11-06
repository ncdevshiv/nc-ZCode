//! Locating and materializing the kernel driver script.
//!
//! `driver/kernel_driver.mjs` is a script **executed by the user's own Node** (the R2 boundary:
//! the shipped app contains no JS runtime at all). At compile time `include_str!` embeds the script text into
//! the binary; before each kernel starts it is materialized into its own temp directory and then launched through the `node <path> …` argument array —
//! this preserves the driver's own argv contract (kernel_driver.mjs:37-45 reads
//! `process.argv.slice(2)` for `--restrict-process`/`--module-dir`) and also guarantees that after release
//! there is no dependency on the source-tree layout.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The embedded kernel driver source (the very same file as src/driver/kernel_driver.mjs).
pub const KERNEL_DRIVER_SOURCE: &str = include_str!("driver/kernel_driver.mjs");

/// File name of the materialized driver.
pub const DRIVER_FILE_NAME: &str = "kernel_driver.mjs";

static DRIVER_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write the embedded driver into a unique subdirectory under `parent_dir` and return the script path.
///
/// The directory name carries pid + a monotonic counter + a nanosecond timestamp: concurrent kernels never overwrite each other,
/// and it also avoids colliding with stale directories left over from a previous run (the contents are identical, so overwriting
/// would be harmless, but separate directories keep Drop's cleanup from interfering).
pub fn materialize_driver(parent_dir: &Path) -> io::Result<PathBuf> {
    let unique = format!(
        "zcode-node-repl-{}-{}-{}",
        std::process::id(),
        DRIVER_DIR_COUNTER.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    let dir = parent_dir.join(unique);
    std::fs::create_dir_all(&dir)?;
    let script = dir.join(DRIVER_FILE_NAME);
    std::fs::write(&script, KERNEL_DRIVER_SOURCE)?;
    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_driver_is_the_protocol_declaring_script() {
        // The embedded text must be the real driver: both the protocol id and the stdin read loop are present (guards against embedding the wrong file).
        assert!(KERNEL_DRIVER_SOURCE.contains("zcode-node-repl-kernel/1"));
        assert!(KERNEL_DRIVER_SOURCE.contains("readStdin"));
        assert!(KERNEL_DRIVER_SOURCE.len() > 10_000);
    }

    #[test]
    fn materialize_writes_readable_script_in_unique_dirs() {
        let parent = std::env::temp_dir();
        let first = materialize_driver(&parent).expect("materialize first");
        let second = materialize_driver(&parent).expect("materialize second");
        assert_ne!(
            first, second,
            "The two materializations must fall in different directories"
        );
        let written = std::fs::read_to_string(&first).expect("read driver");
        assert_eq!(written, KERNEL_DRIVER_SOURCE);
        let _ = std::fs::remove_dir_all(first.parent().unwrap());
        let _ = std::fs::remove_dir_all(second.parent().unwrap());
    }
}
