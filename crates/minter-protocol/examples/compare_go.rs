#[path = "../tests/support/mod.rs"]
mod support;

use anyhow::{Context, Result, ensure};
use std::{fs::File, io::BufReader};

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: compare_go GO_TRANSACTIONS.jsonl")?;
    let counts = support::compare(BufReader::new(File::open(path)?))?;
    ensure!(counts.single + counts.multisig > 0, "empty Go reference");
    println!("{}", serde_json::to_string(&counts)?);
    Ok(())
}
