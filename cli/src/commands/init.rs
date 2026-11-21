//! `copse init`: write `.copse.toml`, check the volume, build the first template.

use crate::config::{self, Project};
use crate::{clone, git, volume};
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Setup command for the template (repeatable). Default: guessed from lockfiles.
    #[arg(long = "setup")]
    pub setup: Vec<String>,
    /// Write the config only; don't warm the template yet.
    #[arg(long)]
    pub no_warm: bool,
}

pub fn run(args: Args) -> Result<()> {
    let p = Project::discover(&std::env::current_dir()?)?;
    let path = p.main.join(config::FILE);
    if path.exists() {
        println!("✓ {} already exists, keeping it", path.display());
    } else {
        let setup = if args.setup.is_empty() { guess_setup(&p.main) } else { args.setup.clone() };
        let base = guess_base(&p.main);
        fs::write(&path, render(&base, &setup)).with_context(|| format!("writing {}", path.display()))?;
        println!("✓ wrote {} (base = {base})", path.display());
        if setup.is_empty() {
            println!("  no setup commands; add them under [warm] so workspaces come with dependencies installed");
        }
        for cmd in &setup {
            println!("  setup: {cmd}");
        }
    }

    // Re-read so the rest uses exactly what's on disk.
    let p = Project::discover(&p.main)?;
    fs::create_dir_all(&p.workspaces_dir)?;
    println!("✓ workspaces in {}", p.workspaces_dir.display());

    if !volume::same_volume(&p.main, &p.workspaces_dir)? {
        eprintln!("! the repo and workspaces dir are on different volumes: no copy-on-write, every workspace is a full copy");
    }
    let (provider, _) = clone::detect(&p.main, &p.workspaces_dir);
    println!("✓ clone provider: {}{}", provider.name(), if provider.is_cow() { "" } else { " (slow; see `copse doctor`)" });

    if args.no_warm {
        println!("next: `copse warm`, then `copse new <name>`");
        return Ok(());
    }
    crate::commands::warm::warm(&p, false)?;
    println!("next: `copse new <name>`");
    Ok(())
}

/// Install commands implied by the lockfiles at the repo root.
fn guess_setup(main: &Path) -> Vec<String> {
    let has = |f: &str| main.join(f).exists();
    let mut v = Vec::new();
    if has("pnpm-lock.yaml") {
        v.push("pnpm install --frozen-lockfile".into());
    } else if has("yarn.lock") {
        v.push("yarn install --frozen-lockfile".into());
    } else if has("package-lock.json") {
        v.push("npm ci".into());
    } else if has("bun.lockb") || has("bun.lock") {
        v.push("bun install --frozen-lockfile".into());
    }
    if has("Cargo.lock") {
        v.push("cargo fetch".into());
    }
    if has("uv.lock") {
        v.push("uv sync --frozen".into());
    }
    v
}

/// The remote's default branch if known, else the current branch, else `main`.
fn guess_base(main: &Path) -> String {
    if let Ok(r) = git::run(main, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]) {
        if let Some(b) = r.strip_prefix("origin/") {
            return b.to_string();
        }
    }
    git::run(main, &["branch", "--show-current"]).ok().filter(|b| !b.is_empty()).unwrap_or_else(|| "main".into())
}

fn render(base: &str, setup: &[String]) -> String {
    let list = |v: &[String]| {
        if v.is_empty() {
            "[]".to_string()
        } else {
            let items: Vec<String> = v.iter().map(|s| format!("  {},", toml_str(s))).collect();
            format!("[\n{}\n]", items.join("\n"))
        }
    };
    format!(
        "# copse config (see `copse --help`).\n\
         \n\
         # Where workspaces and the warm template live. Must be on the same volume as\n\
         # this repo for copy-on-write cloning. Default: ../<repo>.copse\n\
         # workspaces_dir = \"../my-repo.copse\"\n\
         \n\
         # Branch the template tracks.\n\
         base = {}\n\
         \n\
         [warm]\n\
         # Run in the template by `copse warm`. Should only write ignored files\n\
         # (node_modules, target/, ...), which every workspace then gets for free.\n\
         setup = {}\n\
         # Globs never cloned into workspaces, e.g. [\"**/.cache/tmp\", \"*.log\"].\n\
         exclude = []\n\
         \n\
         [new]\n\
         # Run in each new workspace after checkout (path fix-ups, a quick incremental build).\n\
         post_create = []\n",
        toml_str(base),
        list(setup)
    )
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_config_parses() {
        let text = render("main", &["pnpm install --frozen-lockfile".into(), "echo \"hi\"".into()]);
        let c: config::Config = toml::from_str(&text).unwrap();
        assert_eq!(c.base.as_deref(), Some("main"));
        assert_eq!(c.warm.setup, ["pnpm install --frozen-lockfile", "echo \"hi\""]);
        assert!(c.workspaces_dir.is_none());
    }
}
