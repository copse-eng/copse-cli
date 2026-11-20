//! Copy-on-write cloning of a directory tree, with a plain-copy fallback.
//!
//! Providers only know how to clone one file's contents. [`clone_tree`] does the
//! walk, directory creation, parallelism, metadata and link handling for all of them.
//!
//! Per-file cost dominates on Windows (see `bench/src/bin/m1_files.rs`), so each
//! destination file is opened exactly once, and small files are written instead of
//! cloned: setting up a block clone costs more than writing a few KB.

mod copy;
#[cfg(windows)]
mod refs;

pub use copy::Copy;
#[cfg(windows)]
pub use refs::Refs;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Files smaller than this are written, not cloned, when writes are fast. Measured
/// on a ReFS Dev Drive with the linux tree (97k files) and no antivirus scanning:
/// cloning everything 5.6s, writing everything 4.3s, this split 2.3s.
pub const CLONE_MIN_BYTES: u64 = 64 * 1024;

/// Above this (wall µs per small file written), something is scanning new files,
/// usually real-time antivirus. Then writing data is far slower than block cloning
/// (linux tree with Defender scanning: write small files 57s, clone everything 14s),
/// so clone every file. Unscanned ReFS: ~25 µs/file; Defender: ~300 µs/file.
pub const SLOW_WRITE_US: f64 = 100.0;

/// Probe size. Smaller bursts finish before the scanner's queue backs up and read
/// as unscanned (Dev Drive, Defender scanning: 300 files 80 µs, 2000 files 240 µs;
/// unscanned: ~47 µs at every size).
pub const PROBE_FILES: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    Yes,
    No(String),
}

pub trait CloneProvider: Sync {
    /// Short id shown to users, e.g. `refs-blockclone`.
    fn name(&self) -> &'static str;
    /// Copy-on-write (true) or a full byte copy (false).
    fn is_cow(&self) -> bool;
    /// Can this provider clone from `src` into `dst`? Neither needs to exist yet.
    fn supported(&self, src: &Path, dst: &Path) -> Support;
    /// Fill `dst` (new, empty, open for writing) with the contents of `src`.
    fn clone_file(&self, src: &File, dst: &File) -> io::Result<()>;
}

/// All providers, best first. [`Copy`] is last and always supported.
pub fn providers() -> Vec<Box<dyn CloneProvider>> {
    let mut v: Vec<Box<dyn CloneProvider>> = Vec::new();
    #[cfg(windows)]
    v.push(Box::new(Refs));
    v.push(Box::new(Copy));
    v
}

/// The best supported provider, plus why each better one was rejected.
pub fn detect(src: &Path, dst: &Path) -> (Box<dyn CloneProvider>, Vec<(&'static str, String)>) {
    let mut rejected = Vec::new();
    for p in providers() {
        match p.supported(src, dst) {
            Support::Yes => return (p, rejected),
            Support::No(why) => rejected.push((p.name(), why)),
        }
    }
    unreachable!("Copy is always supported")
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub provider: &'static str,
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
    pub bytes: u64,
    /// Files the provider cloned copy-on-write.
    pub cloned: u64,
    /// Files that were byte-copied (Copy provider, small files, or per-file fallback).
    pub copied: u64,
    /// Files at least this big were cloned; smaller ones written.
    pub clone_min_bytes: u64,
    /// Result of the write probe that chose `clone_min_bytes` (µs per small file).
    pub write_probe_us: Option<f64>,
    pub elapsed: Duration,
}

/// Clone the tree at `src` into `dst` (which must not exist, or be empty).
///
/// `skip` gets paths relative to `src`; a skipped directory is not descended.
/// Symlinks and junctions are recreated, not followed; absolute targets that
/// point inside `src` are rebased onto `dst` so the clone is self-contained.
pub fn clone_tree(
    provider: &dyn CloneProvider,
    src: &Path,
    dst: &Path,
    skip: &(dyn Fn(&Path) -> bool + Sync),
) -> Result<Stats> {
    let start = Instant::now();
    let src = std::path::absolute(src)?;
    let dst = std::path::absolute(dst)?;
    if !src.is_dir() {
        bail!("{} is not a directory", src.display());
    }
    // Skipped paths may already exist in dst (e.g. the `.git` file written by
    // `git worktree add --no-checkout`); anything else means we'd clobber work.
    if dst.exists() {
        for e in fs::read_dir(&dst)? {
            let name = PathBuf::from(e?.file_name());
            if !skip(&name) {
                bail!("{} already exists and is not empty", dst.display());
            }
        }
    }
    if dst.starts_with(&src) {
        bail!("destination {} is inside the source", dst.display());
    }
    fs::create_dir_all(&dst).with_context(|| format!("creating {}", dst.display()))?;

    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut links = Vec::new();
    let walker = walkdir::WalkDir::new(&src).follow_links(false).min_depth(1).into_iter();
    for entry in walker.filter_entry(|e| !skip(e.path().strip_prefix(&src).unwrap_or(e.path()))) {
        let entry = entry.context("walking source tree")?;
        let rel = entry.path().strip_prefix(&src).unwrap().to_path_buf();
        let ft = entry.file_type();
        if ft.is_symlink() {
            links.push(rel);
        } else if ft.is_dir() {
            dirs.push(rel);
        } else {
            // On Windows this comes from the directory listing: no extra open per file.
            let m = entry.metadata().with_context(|| format!("reading {}", entry.path().display()))?;
            files.push(FileEntry { rel, len: m.len(), mtime: m.modified()?, readonly: m.permissions().readonly() });
        }
    }

    // Walk order is parent-before-child, so plain create_dir is enough.
    for d in &dirs {
        let p = dst.join(d);
        fs::create_dir(&p).with_context(|| format!("creating {}", p.display()))?;
    }

    // Pick the small-file strategy for this machine: writing small files is only
    // a win if nothing is scanning every write.
    let (clone_min_bytes, write_probe_us) = if provider.is_cow() {
        match write_probe(&dst, PROBE_FILES) {
            Ok(us) if us > SLOW_WRITE_US => (0, Some(us)),
            Ok(us) => (CLONE_MIN_BYTES, Some(us)),
            Err(_) => (0, None),
        }
    } else {
        (u64::MAX, None)
    };

    let bytes = AtomicU64::new(0);
    let cloned = AtomicU64::new(0);
    let copied = AtomicU64::new(0);
    files.par_iter().try_for_each(|e| -> Result<()> {
        let (from, to) = (src.join(&e.rel), dst.join(&e.rel));
        let was_cloned = materialize(provider, &from, &to, e, clone_min_bytes).with_context(|| format!("cloning {}", from.display()))?;
        if was_cloned { &cloned } else { &copied }.fetch_add(1, Ordering::Relaxed);
        bytes.fetch_add(e.len, Ordering::Relaxed);
        Ok(())
    })?;

    for rel in &links {
        recreate_link(&src, &dst, rel)
            .with_context(|| format!("recreating link {}", src.join(rel).display()))?;
    }

    Ok(Stats {
        provider: provider.name(),
        files: files.len() as u64,
        dirs: dirs.len() as u64,
        links: links.len() as u64,
        bytes: bytes.into_inner(),
        cloned: cloned.into_inner(),
        copied: copied.into_inner(),
        clone_min_bytes,
        write_probe_us,
        elapsed: start.elapsed(),
    })
}

struct FileEntry {
    rel: PathBuf,
    len: u64,
    mtime: SystemTime,
    readonly: bool,
}

/// Create `to` with `from`'s contents, mtime and read-only flag, using one handle.
/// Returns whether the contents were cloned (vs written).
///
/// The git index compares mtime + size (with `core.checkStat=minimal`), so the
/// mtime must survive the clone.
fn materialize(provider: &dyn CloneProvider, from: &Path, to: &Path, e: &FileEntry, clone_min: u64) -> io::Result<bool> {
    let dst = fs::OpenOptions::new().write(true).create_new(true).open(to)?;
    let mut cloned = false;
    if provider.is_cow() && e.len >= clone_min {
        let src = File::open(from)?;
        if provider.clone_file(&src, &dst).is_ok() {
            cloned = true;
        } else {
            // A file the filesystem refuses to clone shouldn't fail the whole
            // tree: start over on the same handle and copy it.
            dst.set_len(0)?;
            io::copy(&mut &src, &mut &dst)?;
        }
    } else {
        write_contents(from, &dst, e.len)?;
    }
    dst.set_modified(e.mtime)?;
    if e.readonly {
        let mut perms = dst.metadata()?.permissions();
        perms.set_readonly(true);
        dst.set_permissions(perms)?;
    }
    Ok(cloned)
}

/// Wall time per file (µs) to create, write, timestamp and close `n` 16 KiB files
/// in parallel inside `dir`, the way small files are written. Cleans up after itself.
pub fn write_probe(dir: &Path, n: usize) -> io::Result<f64> {
    let probe = dir.join(format!(".copse-write-probe-{}", std::process::id()));
    fs::create_dir(&probe)?;
    let data: Vec<u8> = (0..16 * 1024).map(|i| (i % 251) as u8).collect();
    let now = SystemTime::now();
    let start = Instant::now();
    let result = (0..n).into_par_iter().try_for_each(|i| -> io::Result<()> {
        let f = fs::OpenOptions::new().write(true).create_new(true).open(probe.join(format!("f{i}")))?;
        (&f).write_all(&data)?;
        f.set_modified(now)
    });
    let us = start.elapsed().as_secs_f64() * 1e6 / n as f64;
    let _ = fs::remove_dir_all(&probe);
    result.map(|_| us)
}

/// Byte-copy `from` into `dst`. Small files in one read + write; big ones streamed.
pub(crate) fn write_contents(from: &Path, dst: &File, len: u64) -> io::Result<()> {
    if len <= 8 * 1024 * 1024 {
        (&*dst).write_all(&fs::read(from)?)
    } else {
        io::copy(&mut File::open(from)?, &mut &*dst).map(|_| ())
    }
}

fn recreate_link(src: &Path, dst: &Path, rel: &Path) -> io::Result<()> {
    let (from, to) = (src.join(rel), dst.join(rel));
    #[cfg(windows)]
    if junction::exists(&from)? {
        let target = junction::get_target(&from)?;
        return junction::create(rebase(&target, src, dst), &to);
    }
    let target = fs::read_link(&from)?;
    let target = if target.is_absolute() { rebase(&target, src, dst) } else { target };
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if fs::symlink_metadata(&from)?.file_type().is_symlink_dir() {
            std::os::windows::fs::symlink_dir(&target, &to)
        } else {
            std::os::windows::fs::symlink_file(&target, &to)
        }
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &to)
}

/// If `target` is inside `src`, the same path inside `dst`; otherwise unchanged.
/// Tolerates `\\?\` prefixes and (on Windows) case differences.
pub fn rebase(target: &Path, src: &Path, dst: &Path) -> PathBuf {
    let t = normalize(target);
    let s = normalize(src);
    if t.len() >= s.len() && t[..s.len()] == s[..] {
        let mut out = dst.to_path_buf();
        for c in &components(target)[s.len()..] {
            out.push(c);
        }
        out
    } else {
        target.to_path_buf()
    }
}

fn components(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Prefix(pre) => Some(strip_verbatim(&pre.as_os_str().to_string_lossy())),
            Component::RootDir | Component::CurDir => None,
            other => Some(other.as_os_str().to_string_lossy().into_owned()),
        })
        .collect()
}

fn normalize(p: &Path) -> Vec<String> {
    let c = components(p);
    if cfg!(windows) {
        c.into_iter().map(|s| s.to_lowercase()).collect()
    } else {
        c
    }
}

fn strip_verbatim(s: &str) -> String {
    s.strip_prefix(r"\\?\").or_else(|| s.strip_prefix(r"\??\")).unwrap_or(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn rebase_inside_windows() {
        let got = rebase(
            Path::new(r"\\?\E:\Repo\tmpl\node_modules\.pnpm\a"),
            Path::new(r"e:\repo\tmpl"),
            Path::new(r"E:\repo.copse\ws1"),
        );
        assert_eq!(got, PathBuf::from(r"E:\repo.copse\ws1\node_modules\.pnpm\a"));
    }

    #[cfg(windows)]
    #[test]
    fn rebase_outside_windows() {
        let t = Path::new(r"C:\Users\x\AppData\pnpm-store\v3");
        assert_eq!(rebase(t, Path::new(r"E:\repo\tmpl"), Path::new(r"E:\ws")), t);
    }

    #[test]
    fn rebase_sibling_prefix_is_not_inside() {
        // `/a/tmpl2` must not match source `/a/tmpl`.
        let (t, s, d) = if cfg!(windows) {
            (r"E:\a\tmpl2\x", r"E:\a\tmpl", r"E:\ws")
        } else {
            ("/a/tmpl2/x", "/a/tmpl", "/ws")
        };
        assert_eq!(rebase(Path::new(t), Path::new(s), Path::new(d)), PathBuf::from(t));
    }

    #[cfg(unix)]
    #[test]
    fn rebase_inside_unix() {
        let got = rebase(Path::new("/r/tmpl/a/b"), Path::new("/r/tmpl"), Path::new("/r/ws"));
        assert_eq!(got, PathBuf::from("/r/ws/a/b"));
    }
}
