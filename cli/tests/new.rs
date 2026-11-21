//! End-to-end `copse new` against a throwaway repo.

use std::fs;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn copse(args: &[&str]) -> std::process::Output {
    // Run from a neutral directory: with --template, the template decides the repo.
    Command::new(env!("CARGO_BIN_EXE_copse")).args(args).current_dir(std::env::temp_dir()).output().unwrap()
}

/// Repo with two commits, and a template worktree at the first one that has an
/// ignored, "installed" dependency folder.
fn setup(root: &Path) -> (std::path::PathBuf, String, String) {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    fs::write(repo.join(".gitignore"), "node_modules/\n").unwrap();
    fs::write(repo.join("a.txt"), "one\n").unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/lib.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "one"]);
    let c1 = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("a.txt"), "two\n").unwrap();
    fs::write(repo.join("b.txt"), "new\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "two"]);
    let c2 = git(&repo, &["rev-parse", "HEAD"]);

    let tmpl = root.join("tmpl");
    git(&repo, &["worktree", "add", "--quiet", "--detach", tmpl.to_str().unwrap(), &c1]);
    fs::create_dir_all(tmpl.join("node_modules/pkg")).unwrap();
    fs::write(tmpl.join("node_modules/pkg/index.js"), "module.exports = 1\n").unwrap();
    (tmpl, c1, c2)
}

#[test]
fn new_clones_template_and_checks_out_target() {
    let tmp = tempfile::tempdir().unwrap();
    let (tmpl, _c1, c2) = setup(tmp.path());
    let ws_dir = tmp.path().join("ws");
    let out = copse(&["new", "feat", "--template", tmpl.to_str().unwrap(), "--dir", ws_dir.to_str().unwrap(), "--from", &c2]);
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));

    let ws = ws_dir.join("feat");
    assert_eq!(git(&ws, &["rev-parse", "HEAD"]), c2);
    assert_eq!(git(&ws, &["branch", "--show-current"]), "feat");
    assert_eq!(git(&ws, &["status", "--porcelain"]), "", "workspace should be clean");
    assert_eq!(fs::read_to_string(ws.join("a.txt")).unwrap().trim(), "two");
    assert!(ws.join("b.txt").exists());
    // The point of copse: ignored, installed files come along.
    assert!(ws.join("node_modules/pkg/index.js").exists());
    // A real worktree of the same repo, with its own .git file.
    assert!(fs::read_to_string(ws.join(".git")).unwrap().contains("worktrees"));
    assert!(git(&tmpl, &["worktree", "list"]).contains("feat"));
}

#[test]
fn bad_from_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (tmpl, _, _) = setup(tmp.path());
    let ws_dir = tmp.path().join("ws");
    let out = copse(&["new", "x", "--template", tmpl.to_str().unwrap(), "--dir", ws_dir.to_str().unwrap(), "--from", "nope"]);
    assert!(!out.status.success());
    assert!(!ws_dir.join("x").exists());
    assert!(!git(&tmpl, &["worktree", "list"]).contains("ws"));
}
