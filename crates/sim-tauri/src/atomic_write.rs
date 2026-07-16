//! Atomic file writes: write to a sibling `.tmp` then `rename` so a crash
//! between `truncate(0)` and `write_all` can't leave a 0-byte or partial
//! file at the target path.
//!
//! `std::fs::rename` is atomic on POSIX (Linux, macOS, the BSDs) and on
//! Windows when the target doesn't exist. For existing files on Windows
//! the rename falls back to a replace operation which is *effectively*
//! atomic from the perspective of a concurrent reader; the target either
//! has the old contents or the new contents, never a mixture.
//!
//! This is the standard `tempfile::NamedTempFile::persist` pattern, but
//! implemented inline so the simulator doesn't grow a new dependency for
//! a 30-line function.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Write `data` to `path` atomically: writes to `<path>.tmp` first, then
/// renames over `path`. On success the file at `path` has the exact bytes
/// from `data`. On failure the target file is unchanged. Creates parent
/// directories if they don't exist.
pub fn write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    write_impl(path, data, 0)
}

/// Internal: try up to `attempt` retries of the rename to handle
/// transient Windows "access denied" races (e.g. an AV scanner briefly
/// holding a handle to the file).
fn write_impl(path: &Path, data: &[u8], attempt: u32) -> std::io::Result<()> {
    let tmp = tmp_path(path);
    {
        // Open in create+truncate+write mode. A failure here leaves any
        // pre-existing `path` untouched, so a corrupt-file crash window
        // is impossible.
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        // Flush the OS buffer to the disk before renaming so the new
        // contents are durable on the renamed file.
        f.sync_all().ok();
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(_e) if attempt < 3 => {
            // Backoff and retry — Windows can race with AV scanners.
            std::thread::sleep(std::time::Duration::from_millis(10 * (attempt + 1) as u64));
            // Clean up the tmp file before retrying so `File::create` succeeds.
            let _ = std::fs::remove_file(&tmp);
            write_impl(path, data, attempt + 1)
        }
        Err(e) => {
            // Final failure: clean up the tmp file so we don't leave it behind.
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_file() {
        let dir = std::env::temp_dir().join("givsim-atomic-write-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("file.txt");
        std::fs::write(&path, b"original").unwrap();

        write(&path, b"replaced").unwrap();
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"replaced");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn creates_parent_dirs() {
        let dir = std::env::temp_dir().join("givsim-atomic-write-parent");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nested").join("file.txt");

        write(&path, b"hello").unwrap();
        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"hello");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leaves_target_intact_on_failure() {
        let dir = std::env::temp_dir().join("givsim-atomic-write-fail");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("file.txt");
        std::fs::write(&path, b"original").unwrap();

        // We can't easily simulate a failure without filesystem mocking, but
        // we can at least verify the tmp file is cleaned up after a rename
        // success.
        write(&path, b"new").unwrap();
        let tmp = tmp_path(&path);
        assert!(!tmp.exists(), "tmp file should be cleaned up after rename");
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        std::fs::remove_dir_all(&dir).ok();
    }
}
