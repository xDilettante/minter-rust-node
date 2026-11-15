use super::*;

pub(super) const TEMP_PREFIX: &str = ".history-download-";

pub(super) fn cache_name(hash: &[u8; 32]) -> String {
    format!("history-{}.pb", hex::encode(hash))
}

/// Called only after acquiring this database's exclusive writer lock. Unknown
/// files, directories and symlinks are not owned download artifacts.
pub(super) fn cleanup(
    directory: &Path,
    required: Option<&HistoryContext>,
    ready: bool,
) -> Result<u64> {
    let completed = required
        .filter(|_| ready)
        .map(|context| cache_name(&context.sha256));
    let mut removed = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let temporary = name
            .strip_prefix(TEMP_PREFIX)
            .is_some_and(|suffix| !suffix.is_empty());
        if (temporary || completed.as_deref() == Some(name)) && entry.file_type()?.is_file() {
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) => eprintln!("Cannot remove a stale history download: {error}"),
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_interrupted_files_but_preserves_required_cache_and_unrelated_paths() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let db = directory.path().join("state");
        fs::create_dir(&db)?;
        let required = HistoryContext {
            from: 100,
            bytes: 10,
            sha256: [7; 32],
        };
        let cache = format!("history-{}.pb", hex::encode(required.sha256));
        let other = format!("history-{}.pb", hex::encode([8; 32]));
        for name in [
            ".history-download-interrupted",
            ".history-download-",
            "context.pb",
            &cache,
            &other,
        ] {
            fs::write(db.join(name), b"data")?;
        }
        fs::create_dir(db.join(".history-download-directory"))?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(db.join("context.pb"), db.join(".history-download-symlink"))?;
        assert_eq!(cleanup(&db, Some(&required), false)?, 1);
        assert!(db.join(&cache).is_file());
        assert_eq!(cleanup(&db, Some(&required), true)?, 1);
        assert!(!db.join(&cache).exists());
        for name in [".history-download-", "context.pb", &other] {
            assert!(db.join(name).is_file());
        }
        assert!(db.join(".history-download-directory").is_dir());
        #[cfg(unix)]
        assert!(db.join(".history-download-symlink").is_symlink());
        assert_eq!(cleanup(&db, None, true)?, 0);
        Ok(())
    }
}
