//! Writing a file so that a failed or interrupted write never leaves it half
//! written.
//!
//! The bytes go to a temporary file in the same directory first, are flushed
//! to the disk, and are then renamed over the file. The rename replaces the
//! old file in one step (on Linux and on Windows), so a reader sees either the
//! old file or the new one. If writing fails, the temporary file is removed
//! and the old file is not touched.
//!
//! This is what [`Config::save`](crate::Config::save) does, and what the
//! index of the sound library does (ADR-0010).
//!
//! The directory of the file is not synced after the rename, so a power loss
//! right after a save can still bring the old file back. It never leaves a
//! half written one.
//!
//! This does file I/O: never call it from the realtime path.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Writes `bytes` to `path`, replacing the file if it exists.
///
/// The directory of `path` must exist. The temporary file is `path` with
/// `.tmp` added (see [`temp_path`]); one that an earlier crash left behind is
/// overwritten.
///
/// # Errors
///
/// Returns the error of the step that failed (creating or writing the
/// temporary file, flushing it, or the rename). The old file is kept, and the
/// temporary file is removed.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temp_path(path);
    let result = write_synced(&tmp, bytes).and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        // The error of the write is the one to report; a temporary file that
        // cannot be removed (or is not there) changes nothing.
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Returns the temporary file for `path`: the same directory and name, with
/// `.tmp` added.
pub(crate) fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Creates `path`, writes `bytes`, and flushes them to the disk.
fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory under the system temp dir, unique per test.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ink-core-atomic-file-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_temporary_file_is_the_name_with_tmp_added() {
        assert_eq!(
            temp_path(Path::new("/data/library.toml")),
            Path::new("/data/library.toml.tmp")
        );
        assert_eq!(
            temp_path(Path::new("config")),
            Path::new("config.tmp"),
            "a name without an extension"
        );
        assert_eq!(
            temp_path(Path::new("dir/効果音 索引.toml")),
            Path::new("dir/効果音 索引.toml.tmp"),
            "Japanese and a space"
        );
    }

    #[test]
    fn a_new_file_is_written_and_no_temporary_file_is_left() {
        let path = test_dir("new").join("file.toml");
        write_atomically(&path, b"hello").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        assert!(!temp_path(&path).exists());
    }

    #[test]
    fn an_existing_file_is_replaced_as_a_whole() {
        let path = test_dir("replace").join("file.toml");
        write_atomically(&path, b"a much longer first version of the file").unwrap();
        write_atomically(&path, b"short").unwrap();
        // Nothing of the longer old file is left at the end.
        assert_eq!(fs::read(&path).unwrap(), b"short");
        assert!(!temp_path(&path).exists());
    }

    #[test]
    fn nothing_is_a_file_of_no_bytes() {
        let path = test_dir("empty").join("file.toml");
        write_atomically(&path, b"not empty").unwrap();
        write_atomically(&path, b"").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"");
    }

    #[test]
    fn every_byte_value_and_a_large_file_come_back_exactly() {
        let path = test_dir("bytes").join("file.bin");
        let bytes: Vec<u8> = (0..1_000_000_u32).map(|n| (n % 256) as u8).collect();
        write_atomically(&path, &bytes).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn a_temporary_file_left_by_an_earlier_crash_is_overwritten() {
        let path = test_dir("stale").join("file.toml");
        fs::write(temp_path(&path), b"half of an old write").unwrap();
        write_atomically(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(!temp_path(&path).exists());
    }

    #[test]
    fn a_failure_to_write_keeps_the_existing_file() {
        let path = test_dir("write-fails").join("file.toml");
        write_atomically(&path, b"the old file").unwrap();
        // A directory where the temporary file should be: it cannot be
        // created.
        let tmp = temp_path(&path);
        fs::create_dir_all(tmp.join("inner")).unwrap();

        assert!(write_atomically(&path, b"the new file").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"the old file");
        // The directory in the way is not touched.
        assert!(tmp.join("inner").is_dir());
    }

    #[test]
    fn a_failure_to_rename_removes_the_temporary_file_and_keeps_what_was_there() {
        let path = test_dir("rename-fails").join("file.toml");
        // A directory with something in it where the file should be: the
        // rename cannot replace it.
        fs::create_dir_all(path.join("inner")).unwrap();
        fs::write(path.join("inner").join("keep.txt"), b"keep").unwrap();

        assert!(write_atomically(&path, b"data").is_err());
        assert!(!temp_path(&path).exists(), "the temporary file was left");
        assert_eq!(
            fs::read(path.join("inner").join("keep.txt")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn a_missing_directory_is_an_error_and_makes_nothing() {
        let path = test_dir("no-dir").join("missing").join("file.toml");
        let error = write_atomically(&path, b"data").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn japanese_names_and_spaces_work() {
        let dir = test_dir("names").join("音源 フォルダ");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("効果音 索引.toml");
        write_atomically(&path, "拍手".as_bytes()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "拍手");
        assert!(!temp_path(&path).exists());
    }

    #[test]
    fn a_path_of_more_than_260_characters_works() {
        // Longer than the old limit of Windows (MAX_PATH).
        let mut dir = test_dir("long");
        for _ in 0..8 {
            dir.push("a".repeat(40));
        }
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("file.toml");
        assert!(path.to_string_lossy().chars().count() > 260);

        write_atomically(&path, b"long path").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"long path");
        assert!(!temp_path(&path).exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_written_is_a_permission_error() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = test_dir("read-only");
        let path = dir.join("file.toml");
        write_atomically(&path, b"old").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

        let result = write_atomically(&path, b"new");

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        // A user who can write anywhere (root) is not stopped by the mode.
        if let Err(error) = result {
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(fs::read(&path).unwrap(), b"old");
            assert!(!temp_path(&path).exists());
        }
    }
}
