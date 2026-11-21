//! `copse list`: the template and every workspace, with branch and changes.

use crate::config::Project;
use crate::git;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(clap::Args, Debug)]
pub struct Args {}

pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    /// Short branch name, or None when detached.
    pub branch: Option<String>,
}

pub fn run(_args: Args) -> Result<()> {
    let p = Project::discover(&std::env::current_dir()?)?;

    match (p.warm_commit(), warmed_ago(&p)) {
        (Some(c), Some(ago)) => println!("template  {}  warm, {ago} ago", &c[..12.min(c.len())]),
        (Some(c), None) => println!("template  {}  warm", &c[..12.min(c.len())]),
        _ if p.template().exists() => println!("template  not warm (run `copse warm`)"),
        _ => println!("template  none (run `copse init` or `copse warm`)"),
    }

    let ws = workspaces(&p)?;
    if ws.is_empty() {
        println!("no workspaces in {}", p.workspaces_dir.display());
        return Ok(());
    }
    let rows: Vec<[String; 4]> = ws
        .iter()
        .map(|w| {
            let name = w.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let changes = match git::run(&w.path, &["status", "--porcelain"]) {
                Ok(s) => s.lines().count().to_string(),
                Err(_) => "?".into(),
            };
            let branch = w.branch.clone().unwrap_or_else(|| "(detached)".into());
            [name, branch, w.head[..12.min(w.head.len())].to_string(), changes]
        })
        .collect();
    let header = ["NAME", "BRANCH", "COMMIT", "CHANGES"].map(String::from);
    let width = |i: usize| rows.iter().chain([&header]).map(|r| r[i].len()).max().unwrap_or(0);
    let (w0, w1, w2) = (width(0), width(1), width(2));
    for r in [&header].into_iter().chain(rows.iter()) {
        println!("{:<w0$}  {:<w1$}  {:<w2$}  {}", r[0], r[1], r[2], r[3]);
    }
    Ok(())
}

/// Registered worktrees inside the workspaces dir, excluding the template.
pub fn workspaces(p: &Project) -> Result<Vec<Worktree>> {
    let out = git::run(&p.main, &["worktree", "list", "--porcelain"])?;
    let dir = key(&p.workspaces_dir);
    let tmpl = key(&p.template());
    Ok(parse_porcelain(&out)
        .into_iter()
        .filter(|w| {
            let k = key(&w.path);
            k != tmpl && Path::new(&k).parent().map(|par| par == Path::new(&dir)).unwrap_or(false)
        })
        .collect())
}

pub fn parse_porcelain(out: &str) -> Vec<Worktree> {
    let mut v = Vec::new();
    for block in out.split("\n\n") {
        let mut path = None;
        let mut head = String::new();
        let mut branch = None;
        for line in block.lines() {
            if let Some(x) = line.strip_prefix("worktree ") {
                path = Some(git::native_path(x));
            } else if let Some(x) = line.strip_prefix("HEAD ") {
                head = x.to_string();
            } else if let Some(x) = line.strip_prefix("branch ") {
                branch = Some(x.strip_prefix("refs/heads/").unwrap_or(x).to_string());
            }
        }
        if let Some(path) = path {
            v.push(Worktree { path, head, branch });
        }
    }
    v
}

/// Comparable form of a path: absolute, native separators, case-folded on Windows.
fn key(p: &Path) -> String {
    let s = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().into_owned();
    let s = s.trim_end_matches(['/', '\\']).to_string();
    if cfg!(windows) {
        s.replace('/', "\\").to_lowercase()
    } else {
        s
    }
}

fn warmed_ago(p: &Project) -> Option<String> {
    let text = std::fs::read_to_string(p.ready_marker()).ok()?;
    let at: u64 = text.lines().find_map(|l| l.strip_prefix("warmed_at = "))?.trim().parse().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    let s = now.saturating_sub(at);
    Some(match s {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worktree_list() {
        let out = "worktree C:/r\nHEAD aaaa\nbranch refs/heads/main\n\n\
                   worktree C:/r.copse/.template\nHEAD bbbb\ndetached\n\n\
                   worktree C:/r.copse/feat\nHEAD cccc\nbranch refs/heads/feat\nlocked\n";
        let w = parse_porcelain(out);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0].branch.as_deref(), Some("main"));
        assert_eq!(w[1].branch, None);
        assert_eq!(w[2].head, "cccc");
        assert_eq!(w[2].branch.as_deref(), Some("feat"));
    }
}
