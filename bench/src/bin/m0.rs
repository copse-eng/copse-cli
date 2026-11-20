//! M0 baseline: time plain `git worktree add`, dependency install, build and `git status`
//! separately, so we know where the minutes go before optimizing anything.
//!
//! Usage: `cargo m0 --repo ripgrep --runs 1` (see bench/README.md).

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::Deserialize;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Parser)]
#[command(about = "M0 baseline: time worktree add / install / build / status with plain Git")]
struct Args {
    /// Where source clones and worktrees go. Put it on the volume you want to measure.
    #[arg(long, default_value_os_t = default_workdir())]
    workdir: PathBuf,
    /// Only run these repos from repos.toml (repeatable). Default: all.
    #[arg(long = "repo")]
    repos: Vec<String>,
    /// Runs per repo.
    #[arg(long, default_value_t = 3)]
    runs: u32,
    /// Leave the last run's worktree in place for inspection.
    #[arg(long)]
    keep: bool,
    /// Stream command output instead of capturing it.
    #[arg(short, long)]
    verbose: bool,
}

fn default_workdir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join("copse-bench")
}

#[derive(Deserialize)]
struct Config {
    repo: Vec<Repo>,
}

#[derive(Deserialize)]
struct Repo {
    name: String,
    url: String,
    commit: String,
    /// Omit for checkout-only repos.
    install: Option<String>,
    build: Option<String>,
    /// Fetch only the pinned commit (for huge histories like linux).
    #[serde(default)]
    shallow: bool,
}

struct Row {
    repo: String,
    commit: String,
    run: u32,
    phase: &'static str,
    seconds: f64,
    exit_code: i32,
    files: u64,
    bytes: u64,
}

fn main() -> Result<()> {
    let mut args = Args::parse();
    // Child commands run in different directories (and `git -C` resolves paths after
    // switching), so every path must be absolute. `path::absolute` avoids the `\\?\`
    // prefix `canonicalize` adds on Windows, which git and cmd don't handle well.
    args.workdir = std::path::absolute(&args.workdir)?;
    let bench_dir = Path::new(env!("CARGO_MANIFEST_DIR"));

    let cfg: Config = toml::from_str(&fs::read_to_string(bench_dir.join("repos.toml"))?)
        .context("parsing repos.toml")?;
    let repos: Vec<&Repo> = cfg
        .repo
        .iter()
        .filter(|r| args.repos.is_empty() || args.repos.contains(&r.name))
        .collect();
    if repos.is_empty() {
        bail!("no matching repos in repos.toml");
    }

    fs::create_dir_all(args.workdir.join("src"))?;
    fs::create_dir_all(args.workdir.join("wt"))?;

    let fs_type = filesystem_type(&args.workdir);
    let host = capture("hostname", &[]).unwrap_or_else(|| "unknown".into());
    let git_version = capture("git", &["--version"])
        .map(|v| v.trim_start_matches("git version ").to_string())
        .unwrap_or_else(|| "unknown".into());
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H%M").to_string();
    println!(
        "M0 baseline  host={host}  fs={fs_type}  git={git_version}  workdir={}",
        args.workdir.display()
    );

    let mut rows = Vec::new();
    for r in repos {
        let src = args.workdir.join("src").join(&r.name);

        // One-time clone (not timed).
        if !src.exists() {
            println!("[{}] cloning {} (not timed)...", r.name, r.url);
            if r.shallow {
                run_ok(git().args(["init", "--quiet"]).arg(&src))?;
                run_ok(git().arg("-C").arg(&src).args(["remote", "add", "origin", &r.url]))?;
            } else {
                run_ok(git().args(["clone", "--quiet", "--no-checkout", &r.url]).arg(&src))?;
            }
        }
        let have_commit = git()
            .arg("-C")
            .arg(&src)
            .args(["cat-file", "-e", &format!("{}^{{commit}}", r.commit)])
            .stderr(Stdio::null())
            .status()?
            .success();
        if !have_commit {
            let mut fetch = git();
            fetch.arg("-C").arg(&src).args(["fetch", "--quiet"]);
            if r.shallow {
                fetch.args(["--depth", "1"]);
            }
            run_ok(fetch.args(["origin", &r.commit]))?;
        }

        for run in 1..=args.runs {
            let dest = args.workdir.join("wt").join(format!("{}-{run}", r.name));
            remove_worktree(&src, &dest)?;

            // Parallel checkout (`checkout.workers=0` = one worker per CPU): we compare
            // against the fastest plain Git, not its single-threaded default.
            let mut phases: Vec<(&'static str, Command)> = vec![("worktree_add", {
                let mut c = git();
                c.args(["-c", "checkout.workers=0", "-C"]).arg(&src);
                c.args(["worktree", "add", "--quiet", "--detach"]);
                c.arg(&dest).arg(&r.commit).current_dir(&args.workdir);
                c
            })];
            if let Some(cmd) = &r.install {
                phases.push(("install", shell(cmd, &dest)));
            }
            if let Some(cmd) = &r.build {
                phases.push(("build", shell(cmd, &dest)));
            }
            phases.push(("git_status", {
                let mut c = git();
                c.args(["status", "--porcelain"]).current_dir(&dest);
                c
            }));

            let first = rows.len();
            for (phase, cmd) in phases {
                print!("[{} run {run}/{}] {phase:<12} ", r.name, args.runs);
                std::io::stdout().flush()?;
                let (seconds, exit_code) = timed(cmd, args.verbose)?;
                println!("{seconds:>9.2}s  exit={exit_code}");
                rows.push(Row {
                    repo: r.name.clone(),
                    commit: r.commit[..12].to_string(),
                    run,
                    phase,
                    seconds,
                    exit_code,
                    files: 0,
                    bytes: 0,
                });
                if exit_code != 0 && phase == "worktree_add" {
                    break;
                }
            }

            // A fresh checkout should be clean. If not (e.g. case-colliding or reserved
            // file names on Windows), the timings are suspect: say so loudly.
            if dest.exists() {
                if let Ok(out) = git().args(["status", "--porcelain"]).current_dir(&dest).output() {
                    let dirty = String::from_utf8_lossy(&out.stdout).lines().count();
                    if dirty > 0 {
                        eprintln!(
                            "[{} run {run}/{}] WARNING: {dirty} dirty paths after checkout (run with --keep to inspect)",
                            r.name, args.runs
                        );
                    }
                }
            }

            // Size of the finished workspace (not timed): what copse would have to clone.
            let (files, bytes) = dir_size(&dest);
            for row in &mut rows[first..] {
                row.files = files;
                row.bytes = bytes;
            }
            println!(
                "[{} run {run}/{}] size         {files} files, {}",
                r.name,
                args.runs,
                human_bytes(bytes)
            );

            if !(args.keep && run == args.runs) {
                remove_worktree(&src, &dest)?;
            }
        }
    }

    // Write results.
    let results = bench_dir.join("results");
    fs::create_dir_all(&results)?;
    let base = results.join(format!("{stamp}-{host}"));

    let mut csv = String::from("date,host,fs,git,repo,commit,run,phase,seconds,exit_code,files,bytes\n");
    for r in &rows {
        writeln!(
            csv,
            "{stamp},{host},{fs_type},{git_version},{},{},{},{},{:.2},{},{},{}",
            r.repo, r.commit, r.run, r.phase, r.seconds, r.exit_code, r.files, r.bytes
        )?;
    }
    fs::write(base.with_extension("csv"), csv)?;
    fs::write(
        base.with_extension("md"),
        summary(&rows, &stamp, &host, &fs_type, &git_version, args.runs),
    )?;
    println!("\nWrote {}.csv and .md", base.display());
    Ok(())
}

fn summary(rows: &[Row], stamp: &str, host: &str, fs_type: &str, git: &str, runs: u32) -> String {
    let mut md = format!(
        "# M0 baseline: {stamp}\n\nhost: {host} · fs: {fs_type} · git: {git} · runs: {runs}\n\n\
         | repo | phase | median s | min s | max s | failed runs |\n|---|---|---:|---:|---:|---:|\n"
    );
    let mut keys: Vec<(&str, &str)> = Vec::new();
    for r in rows {
        if !keys.contains(&(r.repo.as_str(), r.phase)) {
            keys.push((r.repo.as_str(), r.phase));
        }
    }
    for (repo, phase) in keys {
        let group: Vec<&Row> = rows.iter().filter(|r| r.repo == repo && r.phase == phase).collect();
        let mut s: Vec<f64> = group.iter().map(|r| r.seconds).collect();
        s.sort_by(f64::total_cmp);
        let failed = group.iter().filter(|r| r.exit_code != 0).count();
        let _ = writeln!(
            md,
            "| {repo} | {phase} | {:.2} | {:.2} | {:.2} | {failed} |",
            s[(s.len() - 1) / 2],
            s[0],
            s[s.len() - 1]
        );
    }
    md.push('\n');
    let mut seen: Vec<&str> = Vec::new();
    for r in rows.iter().rev() {
        if !seen.contains(&r.repo.as_str()) {
            seen.push(&r.repo);
            let _ = writeln!(
                md,
                "- **{}**: {} files, {} after build (last run)",
                r.repo,
                r.files,
                human_bytes(r.bytes)
            );
        }
    }
    if rows.iter().any(|r| r.exit_code != 0) {
        md.push_str("\n> **Warning:** some phases failed (see `exit_code` in the CSV). Their timings aren't comparable.\n");
    }
    md
}

fn human_bytes(b: u64) -> String {
    match b {
        b if b >= 1_000_000_000 => format!("{:.2} GB", b as f64 / 1e9),
        b if b >= 1_000_000 => format!("{:.1} MB", b as f64 / 1e6),
        b => format!("{:.1} KB", b as f64 / 1e3),
    }
}

/// A `git` command with cargo/rustup env stripped.
fn git() -> Command {
    clean(Command::new("git"))
}

/// Run a repo's install/build command through the platform shell.
fn shell(cmd: &str, dir: &Path) -> Command {
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
    clean(c)
}

/// `cargo run` leaks CARGO_* and RUSTUP_TOOLCHAIN into children, which can change how a
/// nested `cargo build` behaves (e.g. override the repo's rust-toolchain pin). Strip them.
fn clean(mut c: Command) -> Command {
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if (k.starts_with("CARGO") && k != "CARGO_HOME") || k == "RUSTUP_TOOLCHAIN" {
            c.env_remove(k.as_ref());
        }
    }
    c
}

fn timed(mut cmd: Command, verbose: bool) -> Result<(f64, i32)> {
    let start = Instant::now();
    if verbose {
        let status = cmd.status()?;
        return Ok((start.elapsed().as_secs_f64(), status.code().unwrap_or(-1)));
    }
    let out = cmd.output()?;
    let secs = start.elapsed().as_secs_f64();
    let code = out.status.code().unwrap_or(-1);
    if code != 0 {
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let lines: Vec<&str> = text.lines().collect();
        eprintln!();
        for l in &lines[lines.len().saturating_sub(20)..] {
            eprintln!("    | {l}");
        }
    }
    Ok((secs, code))
}

fn run_ok(cmd: &mut Command) -> Result<()> {
    let status = cmd.status()?;
    if !status.success() {
        bail!("command failed: {cmd:?}");
    }
    Ok(())
}

fn capture(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog).args(args).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn remove_worktree(src: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        let _ = git()
            .arg("-C")
            .arg(src)
            .args(["worktree", "remove", "--force"])
            .arg(dest)
            .stderr(Stdio::null())
            .status();
        if dest.exists() && fs::remove_dir_all(dest).is_err() {
            // Windows: read-only files (e.g. under node_modules) block deletion.
            for e in walkdir::WalkDir::new(dest).into_iter().flatten() {
                if let Ok(m) = e.metadata() {
                    let mut p = m.permissions();
                    if p.readonly() {
                        #[allow(clippy::permissions_set_readonly_false)]
                        p.set_readonly(false);
                        let _ = fs::set_permissions(e.path(), p);
                    }
                }
            }
            fs::remove_dir_all(dest).with_context(|| format!("removing {}", dest.display()))?;
        }
    }
    let _ = git().arg("-C").arg(src).args(["worktree", "prune"]).status();
    Ok(())
}

/// File count and apparent size. Symlinks/junctions are not followed.
fn dir_size(dir: &Path) -> (u64, u64) {
    let (mut files, mut bytes) = (0, 0);
    for e in walkdir::WalkDir::new(dir).into_iter().flatten() {
        if e.file_type().is_file() {
            files += 1;
            bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    (files, bytes)
}

/// Best-effort filesystem type of the volume holding `path`.
fn filesystem_type(path: &Path) -> String {
    let p = path.to_string_lossy().to_string();
    let found = if cfg!(windows) {
        let script = format!("(Get-Volume -FilePath '{}').FileSystem", p.replace('\'', "''"));
        capture("powershell", &["-NoProfile", "-Command", &script])
    } else if cfg!(target_os = "macos") {
        // `mount` lines look like: /dev/disk3s1 on / (apfs, local, journaled)
        capture("mount", &[]).and_then(|out| {
            let canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            out.lines()
                .filter_map(|l| {
                    let (_, rest) = l.split_once(" on ")?;
                    let (mnt, opts) = rest.split_once(" (")?;
                    let fs = opts.split([',', ')']).next()?.to_string();
                    canon.starts_with(mnt).then_some((mnt.len(), fs))
                })
                .max_by_key(|(len, _)| *len)
                .map(|(_, fs)| fs)
        })
    } else {
        capture("stat", &["-f", "-c", "%T", &p])
    };
    found.unwrap_or_else(|| "unknown".into())
}
