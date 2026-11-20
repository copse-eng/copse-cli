//! M1 file-creation floor: how fast can we materialize a tree's files on this volume?
//!
//! Replays the files of an existing tree (e.g. the M0 linux worktree) into a fresh
//! directory under different strategies, to see where per-file time goes on Windows:
//!
//! - mode `empty`:   create + close (the floor: no data at all)
//! - mode `append`:  create, write bytes, set mtime, close (a naive copy)
//! - mode `presize`: create, set_len, write bytes, set mtime, close (in-place write)
//! - mode `synth`:   create, write zeros of the same length (no source read), set mtime, close
//! - mode `hybrid`:  block-clone files >= 64 KiB, write smaller ones
//! - mode `copyfile`: std::fs::copy (CopyFileExW), then reopen to set mtime
//! - mode `clone`:   create, block-clone from source, set mtime, close (copse's path)
//!
//! Each with `inline` closes (worker drops the handle) or `bg` closes (workers hand
//! handles to dedicated closer threads; see Mercurial's worker.backgroundclose).
//! Every mode uses one destination handle per file.
//!
//! Usage: cargo run -p copse-bench --release --bin m1_files -- --src E:/copse-bench/wt/linux-3 --dst E:/copse-bench/m1

use anyhow::{Context, Result};
use clap::Parser;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

#[derive(Parser)]
struct Args {
    /// Tree whose files to replay.
    #[arg(long)]
    src: PathBuf,
    /// Scratch dir; each run writes and then deletes a subfolder here.
    #[arg(long)]
    dst: PathBuf,
    /// Modes to run (comma-separated): empty, append, presize, clone.
    #[arg(long, value_delimiter = ',', default_value = "empty,append,presize,clone")]
    modes: Vec<String>,
    /// Close strategies (comma-separated): inline, bg.
    #[arg(long, value_delimiter = ',', default_value = "inline,bg")]
    close: Vec<String>,
    /// Worker thread counts (comma-separated).
    #[arg(long, value_delimiter = ',', default_value = "16,64")]
    threads: Vec<usize>,
    /// Skip setting the mtime (to measure what SetFileTime costs).
    #[arg(long)]
    no_mtime: bool,
    /// Run only shard `k/n` of the files (for multi-process tests).
    #[arg(long)]
    shard: Option<String>,
    /// Closer threads for `bg`.
    #[arg(long, default_value_t = 16)]
    closers: usize,
}

struct Entry {
    rel: PathBuf,
    mtime: SystemTime,
    len: u64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let src = std::path::absolute(&args.src)?;
    let (dirs, files) = scan(&src)?;
    let bytes: u64 = files.iter().map(|f| f.len).sum();
    println!(
        "source {}: {} files, {} dirs, {:.1} MB",
        src.display(),
        files.len(),
        dirs.len(),
        bytes as f64 / 1e6
    );
    println!("mtime: {}", if args.no_mtime { "not set" } else { "set" });
    println!("{:<8} {:<7} {:>7}  {:>8}  {:>8}  {:>9}", "mode", "close", "threads", "dirs", "files", "µs/file");

    fs::create_dir_all(&args.dst)?;

    // Multi-process mode: this process does files where index % n == k, into a
    // shared `sharded` folder that the caller times and deletes.
    if let Some(spec) = &args.shard {
        let (k, n) = spec.split_once('/').context("--shard is k/n")?;
        let (k, n): (usize, usize) = (k.parse()?, n.parse()?);
        let root = std::path::absolute(args.dst.join("sharded"))?;
        fs::create_dir_all(&root)?;
        for d in &dirs {
            fs::create_dir_all(root.join(d))?;
        }
        let mine: Vec<Entry> = files.into_iter().enumerate().filter(|(i, _)| i % n == k).map(|(_, e)| e).collect();
        let t = Instant::now();
        run_files(&src, &root, &mine, &args.modes[0], args.close[0] == "bg", args.threads[0], args.closers, !args.no_mtime)?;
        println!("shard {k}/{n}: {} files in {:.2}s", mine.len(), t.elapsed().as_secs_f64());
        return Ok(());
    }

    let mut n = 0;
    for mode in &args.modes {
        for close in &args.close {
            for &threads in &args.threads {
                n += 1;
                let root = std::path::absolute(args.dst.join(format!("run-{n}")))?;
                let _ = fs::remove_dir_all(&root);

                let t0 = Instant::now();
                fs::create_dir(&root)?;
                for d in &dirs {
                    fs::create_dir(root.join(d))?;
                }
                let t_dirs = t0.elapsed().as_secs_f64();

                let t1 = Instant::now();
                run_files(&src, &root, &files, mode, close == "bg", threads, args.closers, !args.no_mtime)
                    .with_context(|| format!("{mode}/{close}/{threads}"))?;
                let t_files = t1.elapsed().as_secs_f64();

                println!(
                    "{mode:<8} {close:<7} {threads:>7}  {t_dirs:>7.2}s  {t_files:>7.2}s  {:>9.1}",
                    t_files * 1e6 / files.len() as f64
                );
                fs::remove_dir_all(&root).with_context(|| format!("removing {}", root.display()))?;
            }
        }
    }
    Ok(())
}

fn scan(src: &Path) -> Result<(Vec<PathBuf>, Vec<Entry>)> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let walk = walkdir::WalkDir::new(src).min_depth(1).into_iter();
    for e in walk.filter_entry(|e| !(e.depth() == 1 && e.file_name() == ".git")) {
        let e = e?;
        let rel = e.path().strip_prefix(src)?.to_path_buf();
        if e.file_type().is_dir() {
            dirs.push(rel);
        } else if e.file_type().is_file() {
            let m = e.metadata()?;
            files.push(Entry { rel, mtime: m.modified()?, len: m.len() });
        }
    }
    Ok((dirs, files))
}

fn run_files(
    src: &Path,
    dst: &Path,
    files: &[Entry],
    mode: &str,
    bg: bool,
    threads: usize,
    closers: usize,
    mtime: bool,
) -> Result<()> {
    use rayon::prelude::*;
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build()?;
    let (tx, rx) = crossbeam_channel::unbounded::<File>();
    let closer_threads: Vec<_> = (0..if bg { closers } else { 0 })
        .map(|_| {
            let rx = rx.clone();
            std::thread::spawn(move || {
                for f in rx {
                    drop(f);
                }
            })
        })
        .collect();
    drop(rx);

    pool.install(|| {
        files.par_iter().try_for_each_with(tx, |tx, e| -> Result<()> {
            let to = dst.join(&e.rel);
            let f = materialize(&src.join(&e.rel), &to, e, mode, mtime)
                .with_context(|| format!("{}", to.display()))?;
            if bg {
                tx.send(f).ok();
            }
            Ok(())
        })
    })?;
    // All senders are dropped now; wait for the closers to drain.
    for t in closer_threads {
        t.join().expect("closer thread panicked");
    }
    Ok(())
}

/// Create one file under `mode`, returning its still-open handle.
fn materialize(from: &Path, to: &Path, e: &Entry, mode: &str, mtime: bool) -> Result<File> {
    let f = File::create(to)?;
    match mode {
        "empty" => return Ok(f),
        "append" => {
            (&f).write_all(&fs::read(from)?)?;
        }
        "presize" => {
            let data = fs::read(from)?;
            f.set_len(data.len() as u64)?;
            (&f).write_all(&data)?;
        }
        "hybrid" => {
            if e.len >= 64 * 1024 {
                reflink_copy::reflink_file(&File::open(from)?, &f)?;
            } else {
                (&f).write_all(&fs::read(from)?)?;
            }
        }
        "copyfile" => {
            // CopyFileExW (auto block-clones on ReFS in recent Windows); needs its own handles.
            drop(f);
            fs::copy(from, to)?;
            let f = fs::OpenOptions::new().write(true).open(to)?;
            if mtime {
                f.set_modified(e.mtime)?;
            }
            return Ok(f);
        }
        "synth" => {
            (&f).write_all(&vec![0u8; e.len as usize])?;
        }
        "clone" => {
            let s = File::open(from)?;
            reflink_copy::reflink_file(&s, &f)?;
        }
        other => anyhow::bail!("unknown mode {other}"),
    }
    if mtime {
        f.set_modified(e.mtime)?;
    }
    Ok(f)
}
