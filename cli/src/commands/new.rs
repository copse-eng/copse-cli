//! `copse new`: a warm workspace in seconds (docs/12-cli-v1.md §2).
//!
//! 1. `git worktree add --no-checkout` at the template's commit (registers, writes nothing).
//! 2. Clone the template's files in, including ignored ones (node_modules, target/...).
//! 3. Copy the template's index. Cloned files keep their mtime and creation time,
//!    so the index's stat data matches and git trusts it without rehashing.
//! 4. `git checkout -b <branch> [<from>]` rewrites only files that differ.

use crate::clone;
use crate::git;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Workspace name (also the default branch name).
    pub name: String,
    /// Ref to start from (default: the template's commit).
    #[arg(long)]
    pub from: Option<String>,
    /// Branch to create (default: <name>).
    #[arg(short, long, conflicts_with = "detach")]
    pub branch: Option<String>,
    /// Detached HEAD instead of a new branch.
    #[arg(long)]
    pub detach: bool,
    /// Warm worktree to clone (until `copse warm` exists, any clean worktree of the repo).
    #[arg(long)]
    pub template: PathBuf,
    /// Where workspaces live (default: `../<repo>.copse`, next to the main worktree).
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

pub fn run(args: Args) -> Result<()> {
    let start = Instant::now();
    let template = std::path::absolute(&args.template)?;
    let tmpl_top = git::native_path(
        &git::run(&template, &["rev-parse", "--show-toplevel"]).context("--template is not a git worktree")?,
    );
    let template = tmpl_top;
    let common_dir = git::native_path(&git::run(&template, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
    let tmpl_gitdir = git::native_path(&git::run(&template, &["rev-parse", "--absolute-git-dir"])?);
    let commit = git::run(&template, &["rev-parse", "HEAD"])?;

    // Resolve the target before touching anything, so a typo costs nothing.
    let target = match &args.from {
        Some(r) => git::run(&template, &["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")])
            .with_context(|| format!("--from {r} is not a commit"))?,
        None => commit.clone(),
    };

    let dirty = git::run(&template, &["status", "--porcelain"])?.lines().count();
    if dirty > 0 {
        eprintln!("! template has {dirty} uncommitted changes; they will be copied into the workspace");
    }

    let dir = match args.dir {
        Some(d) => std::path::absolute(d)?,
        None => default_dir(&common_dir)?,
    };
    let dest = dir.join(&args.name);
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let (provider, rejected) = clone::detect(&template, &dest);
    if !provider.is_cow() {
        for (name, why) in &rejected {
            eprintln!("! {name} unavailable: {why}");
        }
        eprintln!("! falling back to a full copy (slow, uses full disk space); see `copse doctor`");
    }

    // 1. Register. From here on, failures must undo the registration.
    git::run(&template, &[
        "worktree".as_ref(),
        "add".as_ref(),
        "--quiet".as_ref(),
        "--no-checkout".as_ref(),
        "--detach".as_ref(),
        dest.as_os_str(),
        commit.as_ref(),
    ])?;

    let result = populate(&args.name, &target, args.branch.as_deref(), args.detach, &template, &tmpl_gitdir, &dest, &commit, provider.as_ref());
    match result {
        Ok(report) => {
            println!(
                "✓ cloned template ({} files, {:.0} MB) in {:.1}s  [{}]",
                report.stats.files,
                report.stats.bytes as f64 / 1e6,
                report.stats.elapsed.as_secs_f64(),
                report.stats.provider
            );
            println!("✓ {}", report.checkout);
            match report.dirty {
                0 => println!("✓ git status clean"),
                n => println!("! git status: {n} changed paths (same as the template's)"),
            }
            println!("→ {}  ({:.1}s total)", dest.display(), start.elapsed().as_secs_f64());
            Ok(())
        }
        Err(e) => {
            eprintln!("✗ failed, removing {}", dest.display());
            let _ = git::run(&template, &["worktree".as_ref(), "remove".as_ref(), "--force".as_ref(), dest.as_os_str()]);
            if dest.exists() {
                let _ = fs::remove_dir_all(&dest);
            }
            let _ = git::run(&template, &["worktree", "prune"]);
            Err(e)
        }
    }
}

struct Report {
    stats: clone::Stats,
    checkout: String,
    dirty: usize,
}

#[allow(clippy::too_many_arguments)]
fn populate(
    name: &str,
    target: &str,
    branch: Option<&str>,
    detach: bool,
    template: &Path,
    tmpl_gitdir: &Path,
    dest: &Path,
    commit: &str,
    provider: &dyn clone::CloneProvider,
) -> Result<Report> {
    // 2. Files. The template's `.git` is a file pointing at its own admin dir; skip it
    // (the new worktree already has its own).
    let stats = clone::clone_tree(provider, template, dest, &|rel| rel == Path::new(".git"))?;

    // 3. Index.
    let dest_gitdir = git::native_path(&git::run(dest, &["rev-parse", "--absolute-git-dir"])?);
    fs::copy(tmpl_gitdir.join("index"), dest_gitdir.join("index")).context("copying the template's index")?;

    // 4. Target.
    let changed = if target == commit {
        0
    } else {
        git::run(dest, &["diff", "--name-only", commit, target])?.lines().count()
    };
    let checkout = if detach {
        git::run(dest, &["checkout", "--quiet", "--detach", target])?;
        format!("detached at {} ({changed} files changed vs template)", &target[..12])
    } else {
        let b = branch.unwrap_or(name);
        git::run(dest, &["checkout", "--quiet", "-b", b, target])?;
        format!("checked out new branch {b} at {} ({changed} files changed vs template)", &target[..12])
    };

    let dirty = git::run(dest, &["status", "--porcelain"])?.lines().count();
    Ok(Report { stats, checkout, dirty })
}

/// `../<repo>.copse`, next to the main worktree (the parent of the common `.git`).
fn default_dir(common_dir: &Path) -> Result<PathBuf> {
    let main = common_dir.parent().context("can't find the main worktree")?;
    let name = main.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    Ok(main.parent().unwrap_or(main).join(format!("{name}.copse")))
}
