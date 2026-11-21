//! `copse warm`: bring the template up to date with `base` and run setup in it.
//!
//! The template is a detached worktree at `<workspaces_dir>/.template`. Ignored files
//! (node_modules, target/, ...) survive between warms, so setup is incremental.

use crate::config::Project;
use crate::{git, shell};
use anyhow::{Context, Result};
use std::fs;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Don't `git fetch` first; warm at the local `base`.
    #[arg(long)]
    pub no_fetch: bool,
}

pub fn run(args: Args) -> Result<()> {
    let p = Project::discover(&std::env::current_dir()?)?;
    warm(&p, args.no_fetch)
}

pub fn warm(p: &Project, no_fetch: bool) -> Result<()> {
    let start = Instant::now();
    fs::create_dir_all(&p.workspaces_dir).with_context(|| format!("creating {}", p.workspaces_dir.display()))?;

    // Not ready until this run finishes: a half-warmed template must never be cloned.
    let marker = p.ready_marker();
    if marker.exists() {
        fs::remove_file(&marker)?;
    }

    let has_origin = git::run(&p.main, &["remote", "get-url", "origin"]).is_ok();
    if has_origin && !no_fetch {
        println!("fetching origin...");
        git::run(&p.main, &["fetch", "--quiet", "origin"])?;
    }
    let commit = resolve_base(p, has_origin)?;

    let tmpl = p.template();
    if tmpl.exists() {
        git::run(&tmpl, &["checkout", "--quiet", "--detach", "--force", &commit])?;
    } else {
        git::run(&p.main, &[
            "worktree".as_ref(),
            "add".as_ref(),
            "--quiet".as_ref(),
            "--detach".as_ref(),
            tmpl.as_os_str(),
            commit.as_ref(),
        ])?;
    }
    println!("✓ template at {} ({})", &commit[..12], p.base);

    for cmd in &p.config.warm.setup {
        shell::run(cmd, &tmpl)?;
    }

    let dirty = git::run(&tmpl, &["status", "--porcelain"])?.lines().count();
    if dirty > 0 {
        eprintln!(
            "! template has {dirty} changed paths after setup; every workspace will inherit them. \
             Setup should only write ignored files."
        );
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    fs::write(&marker, format!("commit = \"{commit}\"\nwarmed_at = {now}\n"))?;
    println!("✓ warm in {:.1}s → {}", start.elapsed().as_secs_f64(), tmpl.display());
    Ok(())
}

/// `origin/<base>` after a fetch, else the local branch.
fn resolve_base(p: &Project, has_origin: bool) -> Result<String> {
    let mut tried = Vec::new();
    if has_origin {
        tried.push(format!("origin/{}", p.base));
    }
    tried.push(p.base.clone());
    for r in &tried {
        if let Ok(c) = git::run(&p.main, &["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")]) {
            return Ok(c);
        }
    }
    anyhow::bail!("can't find base branch (tried {}); set `base` in .copse.toml", tried.join(", "))
}
