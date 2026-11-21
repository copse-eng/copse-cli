//! Running a repo's own commands (`warm.setup`, `new.post_create`).

use anyhow::{bail, Result};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

/// Run `cmd` through the platform shell in `dir`, streaming its output.
/// Prints the command and how long it took.
pub fn run(cmd: &str, dir: &Path) -> Result<()> {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    c.current_dir(dir);
    crate::git::clean_env(&mut c);

    println!("$ {cmd}");
    let start = Instant::now();
    let status = c.status()?;
    let secs = start.elapsed().as_secs_f64();
    if !status.success() {
        bail!("`{cmd}` failed ({status}) after {secs:.1}s in {}", dir.display());
    }
    println!("  ({secs:.1}s)");
    Ok(())
}
