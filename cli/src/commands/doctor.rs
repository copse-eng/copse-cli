//! `copse doctor`: can this machine make fast workspaces for this repo?

use crate::clone::{self, CloneProvider};
use crate::{git, volume};
use anyhow::Result;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Repo to check (default: the current directory's repo).
    #[arg(long)]
    pub path: Option<PathBuf>,
    /// Where workspaces will live (default: `../<repo>.copse`).
    #[arg(long)]
    pub target: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Default)]
struct Report {
    fails: usize,
    warns: usize,
}

impl Report {
    fn line(&mut self, level: Level, label: &str, detail: impl AsRef<str>) {
        let mark = match level {
            Level::Ok => "✓",
            Level::Warn => {
                self.warns += 1;
                "!"
            }
            Level::Fail => {
                self.fails += 1;
                "✗"
            }
        };
        println!("{mark} {label:<14} {}", detail.as_ref());
    }
}

/// Returns true if every check passed (warnings allowed).
pub fn run(args: Args) -> Result<bool> {
    let mut r = Report::default();

    // Git
    match git::version_string() {
        None => r.line(Level::Fail, "git", "not found on PATH"),
        Some(s) => match git::parse_version(&s) {
            Some(v) if v >= git::MIN_VERSION => r.line(Level::Ok, "git", format!("{}.{}.{}", v.0, v.1, v.2)),
            Some(v) => r.line(
                Level::Fail,
                "git",
                format!("{}.{}.{} is too old, need 2.38+ (merge-tree --write-tree)", v.0, v.1, v.2),
            ),
            None => r.line(Level::Warn, "git", format!("can't parse version: {s}")),
        },
    }

    // Repo and target
    let start = match args.path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let repo = match git::capture(Some(&start), &["rev-parse", "--show-toplevel"]) {
        Some(top) => {
            let top = git::native_path(&top);
            r.line(Level::Ok, "repo", top.display().to_string());
            top
        }
        None => {
            r.line(Level::Warn, "repo", format!("{} is not a git repo; checking it as a plain folder", start.display()));
            std::path::absolute(&start)?
        }
    };
    let target = match args.target {
        Some(t) => std::path::absolute(t)?,
        None => default_target(&repo),
    };
    r.line(Level::Ok, "workspaces", target.display().to_string());

    // Filesystems
    let repo_vol = volume_line(&mut r, "repo fs", &repo);
    let target_vol = volume_line(&mut r, "workspaces fs", &target);

    if let (Some(a), Some(b)) = (&repo_vol, &target_vol) {
        if a.id == b.id {
            r.line(Level::Ok, "same volume", a.root.display().to_string());
        } else {
            r.line(
                Level::Fail,
                "same volume",
                format!(
                    "repo is on {}, workspaces on {}; copy-on-write only works within one volume",
                    a.root.display(),
                    b.root.display()
                ),
            );
        }
    }

    // Provider + live probe
    let (provider, rejected) = clone::detect(&repo, &target);
    if provider.is_cow() {
        match probe(provider.as_ref(), &target) {
            Ok(()) => r.line(Level::Ok, "clone", format!("{} (probe cloned a file)", provider.name())),
            Err(e) => r.line(Level::Fail, "clone", format!("{} is advertised but the probe failed: {e}", provider.name())),
        }
    } else {
        let why = rejected.iter().map(|(n, w)| format!("{n}: {w}")).collect::<Vec<_>>().join("; ");
        r.line(Level::Warn, "clone", format!("falling back to plain copy (slow). {why}"));
    }

    let probe_dir = volume::existing_ancestor(&target)?;
    match clone::write_probe(&probe_dir, 2000) {
        Ok(us) if us <= clone::SLOW_WRITE_US => r.line(Level::Ok, "file writes", format!("{us:.0} µs/file")),
        Ok(us) => r.line(
            Level::Warn,
            "file writes",
            format!(
                "{us:.0} µs/file, likely antivirus scanning every new file (copse clones every file to compensate). {}",
                if cfg!(windows) {
                    format!(
                        "Exclude the workspaces folder (or the whole Dev Drive) from Defender                          (admin: Add-MpPreference -ExclusionPath '{}'), or enable Dev Drive performance mode",
                        target.display()
                    )
                } else {
                    "Check for antivirus or file watchers on this volume".to_string()
                }
            ),
        ),
        Err(e) => r.line(Level::Warn, "file writes", format!("probe failed: {e}")),
    }

    #[cfg(windows)]
    windows_checks(&mut r, &repo);

    println!();
    match (r.fails, r.warns) {
        (0, 0) => println!("All checks passed."),
        (0, w) => println!("{w} warning(s)."),
        (f, w) => println!("{f} problem(s), {w} warning(s)."),
    }
    Ok(r.fails == 0)
}

/// `workspaces_dir` from .copse.toml when this is a repo, else `../<name>.copse`.
fn default_target(repo: &Path) -> PathBuf {
    match crate::config::Project::discover(repo) {
        Ok(p) => p.workspaces_dir,
        Err(_) => crate::config::default_workspaces_dir(repo),
    }
}

fn volume_line(r: &mut Report, label: &str, path: &Path) -> Option<volume::VolumeInfo> {
    match volume::info(path) {
        Ok(v) => {
            let (level, cap) = if v.can_clone {
                (Level::Ok, "copy-on-write cloning")
            } else {
                (Level::Warn, "no copy-on-write cloning")
            };
            r.line(level, label, format!("{} on {} ({cap})", v.fs, v.root.display()));
            Some(v)
        }
        Err(e) => {
            r.line(Level::Fail, label, format!("can't inspect {}: {e}", path.display()));
            None
        }
    }
}

/// Clone a small file next to the target and check the contents survive.
fn probe(p: &dyn CloneProvider, target: &Path) -> Result<()> {
    let dir = volume::existing_ancestor(target)?;
    let id = std::process::id();
    let a = dir.join(format!(".copse-probe-{id}-a"));
    let b = dir.join(format!(".copse-probe-{id}-b"));
    let data: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
    let result = (|| -> Result<()> {
        fs::File::create(&a)?.write_all(&data)?;
        let dst = fs::OpenOptions::new().write(true).create_new(true).open(&b)?;
        p.clone_file(&fs::File::open(&a)?, &dst)?;
        drop(dst);
        anyhow::ensure!(fs::read(&b)? == data, "cloned contents differ");
        Ok(())
    })();
    let _ = fs::remove_file(&a);
    let _ = fs::remove_file(&b);
    result
}

#[cfg(windows)]
fn windows_checks(r: &mut Report, repo: &Path) {
    match git::capture(Some(repo), &["config", "--get", "core.longpaths"]).as_deref() {
        Some("true") => r.line(Level::Ok, "long paths", "core.longpaths=true"),
        _ => r.line(Level::Warn, "long paths", "core.longpaths is off; deep node_modules may fail (git config --global core.longpaths true)"),
    }

    let dir = std::env::temp_dir();
    let link = dir.join(format!(".copse-symlink-probe-{}", std::process::id()));
    match std::os::windows::fs::symlink_file(&dir, &link) {
        Ok(()) => {
            let _ = fs::remove_file(&link);
            r.line(Level::Ok, "symlinks", "can create symlinks");
        }
        Err(e) => r.line(
            Level::Warn,
            "symlinks",
            format!("can't create symlinks ({e}); enable Developer Mode if the repo uses them"),
        ),
    }
}
