use anyhow::Result;
use clap::{Parser, Subcommand};
use copse::clone;
use copse::commands::{doctor, new};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "copse", version, about = "Warm Git worktrees in seconds, via copy-on-write clones")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a warm workspace by cloning a template worktree.
    New(new::Args),
    /// Check the filesystem, clone support and Git for this repo.
    Doctor(doctor::Args),
    /// Developer tools.
    #[command(hide = true, subcommand)]
    Debug(Debug),
}

#[derive(Subcommand)]
enum Debug {
    /// Clone a folder with the best available provider.
    Clone {
        src: PathBuf,
        dst: PathBuf,
        /// Force the plain-copy provider.
        #[arg(long)]
        copy: bool,
    },
    /// Time writing small files in a folder (detects antivirus scanning).
    WriteProbe {
        dir: PathBuf,
        /// Number of 16 KiB files.
        #[arg(long, default_value_t = 2000)]
        n: usize,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::New(args) => new::run(args)?,
        Cmd::Doctor(args) => {
            if !doctor::run(args)? {
                std::process::exit(1);
            }
        }
        Cmd::Debug(Debug::Clone { src, dst, copy }) => {
            let provider: Box<dyn clone::CloneProvider> = if copy {
                Box::new(clone::Copy)
            } else {
                let (p, rejected) = clone::detect(&src, &dst);
                for (name, why) in rejected {
                    eprintln!("! {name} unavailable: {why}");
                }
                p
            };
            // Never clone the source's top-level `.git`: it belongs to the source worktree.
            let s = clone::clone_tree(provider.as_ref(), &src, &dst, &|rel| rel == std::path::Path::new(".git"))?;
            if let Some(us) = s.write_probe_us {
                let how = if s.clone_min_bytes == 0 {
                    "clone all files".to_string()
                } else {
                    format!("write files < {} KiB, clone the rest", s.clone_min_bytes / 1024)
                };
                println!("write probe: {us:.0} µs/file -> {how}");
            }
            println!(
                "{}: {} files, {} dirs, {} links, {:.1} MB in {:.2}s (cloned {}, copied {})",
                s.provider,
                s.files,
                s.dirs,
                s.links,
                s.bytes as f64 / 1e6,
                s.elapsed.as_secs_f64(),
                s.cloned,
                s.copied
            );
        }
        Cmd::Debug(Debug::WriteProbe { dir, n }) => {
            let us = clone::write_probe(&dir, n)?;
            println!("{n} files: {us:.1} µs/file");
        }
    }
    Ok(())
}
