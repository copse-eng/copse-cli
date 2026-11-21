//! `.copse.toml` and where everything lives (docs/12-cli-v1.md §5).

use crate::git;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const FILE: &str = ".copse.toml";
/// The warm template, a detached worktree inside the workspaces dir.
pub const TEMPLATE: &str = ".template";
/// Written by a successful `warm`; `new` refuses to clone without it.
pub const READY: &str = ".template.ready";

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Relative to the main worktree. Must be on the same volume as the repo.
    pub workspaces_dir: Option<PathBuf>,
    /// Branch the template tracks.
    pub base: Option<String>,
    pub warm: Warm,
    pub new: New,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Warm {
    /// Run in the template by `copse warm`, e.g. `pnpm install --frozen-lockfile`.
    pub setup: Vec<String>,
    /// Globs (relative to the worktree root) never cloned into workspaces.
    pub exclude: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct New {
    /// Run in each new workspace after checkout.
    pub post_create: Vec<String>,
    /// Extra globs of files whose absolute template paths are rewritten to the
    /// workspace's (in addition to pnpm's .bin shims and state files).
    pub fix_paths: Vec<String>,
}

/// A repo, its config, and the resolved locations copse uses.
#[derive(Debug)]
pub struct Project {
    /// Root of the main worktree (where `.copse.toml` lives).
    pub main: PathBuf,
    pub config: Config,
    pub workspaces_dir: PathBuf,
    pub base: String,
}

impl Project {
    /// Find the repo containing `start` (any of its worktrees) and load its config.
    pub fn discover(start: &Path) -> Result<Project> {
        let common = git::run(start, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .with_context(|| format!("{} is not inside a git repository", start.display()))?;
        let common = git::native_path(&common);
        let main = common.parent().context("can't find the main worktree")?.to_path_buf();

        let path = main.join(FILE);
        let config: Config = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };

        let workspaces_dir = match &config.workspaces_dir {
            Some(d) => std::path::absolute(main.join(d))?,
            None => default_workspaces_dir(&main),
        };
        let base = config.base.clone().unwrap_or_else(|| "main".into());
        Ok(Project { main, config, workspaces_dir, base })
    }

    pub fn template(&self) -> PathBuf {
        self.workspaces_dir.join(TEMPLATE)
    }

    pub fn ready_marker(&self) -> PathBuf {
        self.workspaces_dir.join(READY)
    }

    /// The commit the template was warmed at, if it's warm.
    pub fn warm_commit(&self) -> Option<String> {
        let text = fs::read_to_string(self.ready_marker()).ok()?;
        text.lines().find_map(|l| l.strip_prefix("commit = ").map(|c| c.trim_matches('"').to_string()))
    }

    /// Skip predicate for `clone_tree`: the source's `.git` file plus `warm.exclude`.
    pub fn skip_matcher(&self) -> Result<impl Fn(&Path) -> bool + Sync> {
        let mut b = globset::GlobSetBuilder::new();
        for pat in &self.config.warm.exclude {
            b.add(globset::Glob::new(pat).with_context(|| format!("bad warm.exclude glob {pat:?}"))?);
        }
        let set = b.build()?;
        Ok(move |rel: &Path| {
            rel == Path::new(".git") || {
                // Globs are written with `/`; Windows paths use `\`.
                let s = rel.to_string_lossy().replace('\\', "/");
                set.is_match(s.as_str())
            }
        })
    }
}

/// `../<repo>.copse`, next to the main worktree.
pub fn default_workspaces_dir(main: &Path) -> PathBuf {
    let name = main.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    main.parent().unwrap_or(main).join(format!("{name}.copse"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let c: Config = toml::from_str(
            r#"
            workspaces_dir = "../x.copse"
            base = "trunk"
            [warm]
            setup = ["pnpm install --frozen-lockfile", "pnpm run build"]
            exclude = ["**/.cache/tmp", "*.log"]
            [new]
            post_create = ["pnpm run build"]
            "#,
        )
        .unwrap();
        assert_eq!(c.base.as_deref(), Some("trunk"));
        assert_eq!(c.warm.setup.len(), 2);
        assert_eq!(c.new.post_create, ["pnpm run build"]);
    }

    #[test]
    fn rejects_typos() {
        assert!(toml::from_str::<Config>("[warm]\nsetpu = []\n").is_err());
    }

    #[test]
    fn skip_matches_git_file_and_globs() {
        let p = Project {
            main: PathBuf::from("/r"),
            config: Config {
                warm: Warm { setup: vec![], exclude: vec!["*.log".into(), "**/.cache/tmp".into()] },
                ..Default::default()
            },
            workspaces_dir: PathBuf::from("/r.copse"),
            base: "main".into(),
        };
        let skip = p.skip_matcher().unwrap();
        assert!(skip(Path::new(".git")));
        assert!(skip(Path::new("a/b.log")));
        assert!(skip(&Path::new("pkg").join(".cache").join("tmp")));
        assert!(!skip(Path::new("src/main.rs")));
        assert!(!skip(Path::new("pkg/.git")), "only the top-level .git is the worktree link");
    }
}
