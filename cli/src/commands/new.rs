//! `copse new`: a warm workspace in seconds (docs/12-cli-v1.md §2).
//!
//! 1. `git worktree add --no-checkout` at the template's commit (registers, writes nothing).
//! 2. Clone the template's files in, including ignored ones (node_modules, target/...).
//! 3. Copy the template's index. Cloned files keep their mtime and creation time,
//!    so the index's stat data matches and git trusts it without rehashing.
//! 4. `git checkout -b <branch> [<from>]` rewrites only files that differ.
//! 5. `new.post_create` hooks.

use crate::commands::rm::remove_worktree;
use crate::config::Project;
use crate::{clone, git, shell};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Changing one of these between the template and the target means the template's
/// installed dependencies may not match.
const LOCKFILES: &[&str] = &[
    "pnpm-lock.yaml",
    "package-lock.json",
    "yarn.lock",
    "bun.lockb",
    "bun.lock",
    "Cargo.lock",
    "uv.lock",
    "poetry.lock",
    "Gemfile.lock",
    "go.sum",
];

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
    /// Clone this worktree instead of the warm template.
    #[arg(long)]
    pub template: Option<PathBuf>,
    /// Where to put the workspace (default: `workspaces_dir` from .copse.toml).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Skip `new.post_create` hooks.
    #[arg(long)]
    pub no_hooks: bool,
}

pub fn run(args: Args) -> Result<()> {
    let start = Instant::now();
    // An explicit template decides which repo this is; otherwise the current directory.
    let here = match &args.template {
        Some(t) => std::path::absolute(t)?,
        None => std::env::current_dir()?,
    };
    let p = Project::discover(&here)?;

    let template = match &args.template {
        Some(t) => git::native_path(
            &git::run(&std::path::absolute(t)?, &["rev-parse", "--show-toplevel"])
                .context("--template is not a git worktree")?,
        ),
        None => {
            let t = p.template();
            if !t.exists() {
                bail!("no template yet; run `copse init` (or `copse warm`)");
            }
            if p.warm_commit().is_none() {
                bail!("the template isn't warm (a `copse warm` failed or is running); run `copse warm`");
            }
            t
        }
    };
    let tmpl_gitdir = git::native_path(&git::run(&template, &["rev-parse", "--absolute-git-dir"])?);
    let commit = git::run(&template, &["rev-parse", "HEAD"])?;
    if args.template.is_none() && p.warm_commit().as_deref() != Some(commit.as_str()) {
        eprintln!("! the template moved since it was warmed; run `copse warm`");
    }

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

    let dir = match &args.dir {
        Some(d) => std::path::absolute(d)?,
        None => p.workspaces_dir.clone(),
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
    let skip = p.skip_matcher()?;

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

    let report = match populate(&args, &target, &template, &tmpl_gitdir, &dest, &commit, provider.as_ref(), &skip, &p.config.new.fix_paths) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("✗ failed, removing {}", dest.display());
            let _ = remove_worktree(&p.main, &dest);
            return Err(e);
        }
    };

    println!(
        "✓ cloned template ({} files, {:.0} MB) in {:.1}s  [{}]{}",
        report.stats.files,
        report.stats.bytes as f64 / 1e6,
        report.stats.elapsed.as_secs_f64(),
        report.stats.provider,
        match report.stats.rewritten {
            0 => String::new(),
            n => format!(", {n} files repointed from the template"),
        }
    );
    println!("✓ {}", report.checkout);
    if !report.lockfiles.is_empty() {
        eprintln!(
            "! {} changed vs the template; installed dependencies may be stale. \
             Re-run setup in the workspace, or `copse warm` if the base moved.",
            report.lockfiles.join(", ")
        );
    }

    // 5. Hooks. A failure keeps the workspace (it's valid git-wise) but fails the command.
    if !args.no_hooks {
        for cmd in &p.config.new.post_create {
            shell::run(cmd, &dest).with_context(|| format!("post_create hook failed; the workspace is at {}", dest.display()))?;
        }
    }

    match git::run(&dest, &["status", "--porcelain"])?.lines().count() {
        0 => println!("✓ git status clean"),
        n => println!("! git status: {n} changed paths"),
    }
    println!("→ {}  ({:.1}s total)", dest.display(), start.elapsed().as_secs_f64());
    Ok(())
}

struct Report {
    stats: clone::Stats,
    checkout: String,
    lockfiles: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn populate(
    args: &Args,
    target: &str,
    template: &Path,
    tmpl_gitdir: &Path,
    dest: &Path,
    commit: &str,
    provider: &dyn clone::CloneProvider,
    skip: &(dyn Fn(&Path) -> bool + Sync),
    fix_paths: &[String],
) -> Result<Report> {
    // 2. Files. `skip` always covers the template's `.git` file (it points at the
    // template's own admin dir; the new worktree already has its own).
    let mut globs: Vec<String> = clone::fixup::DEFAULT_GLOBS.iter().map(|g| g.to_string()).collect();
    globs.extend(fix_paths.iter().cloned());
    let fixup = clone::Fixup::new(&globs, template, dest)?;
    let stats = clone::clone_tree(provider, template, dest, skip, Some(&fixup))?;

    // 3. Index.
    let dest_gitdir = git::native_path(&git::run(dest, &["rev-parse", "--absolute-git-dir"])?);
    fs::copy(tmpl_gitdir.join("index"), dest_gitdir.join("index")).context("copying the template's index")?;

    // 4. Target.
    let changed: Vec<String> = if target == commit {
        Vec::new()
    } else {
        git::run(dest, &["diff", "--name-only", commit, target])?.lines().map(String::from).collect()
    };
    let lockfiles = changed
        .iter()
        .filter(|f| LOCKFILES.iter().any(|l| f.rsplit('/').next() == Some(*l)))
        .cloned()
        .collect();
    let n = changed.len();
    let checkout = if args.detach {
        git::run(dest, &["checkout", "--quiet", "--detach", target])?;
        format!("detached at {} ({n} files changed vs template)", &target[..12])
    } else {
        let b = args.branch.as_deref().unwrap_or(&args.name);
        git::run(dest, &["checkout", "--quiet", "-b", b, target])?;
        format!("checked out new branch {b} at {} ({n} files changed vs template)", &target[..12])
    };
    Ok(Report { stats, checkout, lockfiles })
}
