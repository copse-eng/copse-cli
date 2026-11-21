//! `copse rm`: remove a workspace and its worktree registration.

use crate::commands::list;
use crate::config::Project;
use crate::git;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Workspace name.
    pub name: String,
    /// Remove even if it has uncommitted or untracked changes (they are lost).
    #[arg(short, long)]
    pub force: bool,
    /// Also delete the workspace's branch.
    #[arg(long)]
    pub delete_branch: bool,
}

pub fn run(args: Args) -> Result<()> {
    let p = Project::discover(&std::env::current_dir()?)?;
    let ws = list::workspaces(&p)?
        .into_iter()
        .find(|w| w.path.file_name().map(|n| n.to_string_lossy() == args.name.as_str()).unwrap_or(false))
        .with_context(|| format!("no workspace named {:?} in {} (see `copse list`)", args.name, p.workspaces_dir.display()))?;

    let changes = git::run(&ws.path, &["status", "--porcelain"]).map(|s| s.lines().count()).unwrap_or(0);
    if changes > 0 && !args.force {
        bail!(
            "{} has {changes} uncommitted or untracked changes; commit them, or pass --force to discard them",
            ws.path.display()
        );
    }

    remove_worktree(&p.main, &ws.path)?;
    println!("✓ removed {}", ws.path.display());

    if args.delete_branch {
        match &ws.branch {
            Some(b) => {
                git::run(&p.main, &["branch", "-D", b])?;
                println!("✓ deleted branch {b}");
            }
            None => println!("  (detached, no branch to delete)"),
        }
    } else if let Some(b) = &ws.branch {
        println!("  branch {b} kept (--delete-branch to remove it)");
    }
    Ok(())
}

/// `git worktree remove --force`, falling back to deleting the folder ourselves when
/// Windows refuses (read-only files, e.g. under node_modules), then pruning.
pub fn remove_worktree(main: &Path, path: &Path) -> Result<()> {
    let removed = git::run(main, &["worktree".as_ref(), "remove".as_ref(), "--force".as_ref(), path.as_os_str()]);
    if removed.is_err() && path.exists() {
        clear_readonly(path);
        fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    }
    git::run(main, &["worktree", "prune"])?;
    Ok(())
}

fn clear_readonly(dir: &Path) {
    for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
        if let Ok(m) = e.metadata() {
            let mut perms = m.permissions();
            if perms.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                let _ = fs::set_permissions(e.path(), perms);
            }
        }
    }
}
