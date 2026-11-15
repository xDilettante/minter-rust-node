use anyhow::{Context, Result};
use minter_state::{BlockStore, IavlWriter};
use std::path::Path;

fn rejects_alias(alias: &Path) -> Result<()> {
    let error = BlockStore::open(alias)
        .err()
        .context("alias opened a second node writer")?;
    assert!(format!("{error:#}").contains("LOCK"), "{error:#}");
    let error = IavlWriter::open(alias)
        .err()
        .context("alias opened a second trace writer")?;
    assert!(format!("{error:#}").contains("LOCK"), "{error:#}");
    Ok(())
}

#[test]
fn rejects_directory_aliases_while_a_node_writer_is_open() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("state");
    let mut store = BlockStore::open(&path)?;
    store.initialize(99, [], b"execution")?;
    let child = path.join("alias-base");
    std::fs::create_dir(&child)?;
    rejects_alias(&child.join(".."))?;
    #[cfg(unix)]
    {
        let link = directory.path().join("state-link");
        std::os::unix::fs::symlink(&path, &link)?;
        rejects_alias(&link)?;
    }
    Ok(())
}
