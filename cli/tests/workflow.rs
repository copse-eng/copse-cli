//! End-to-end `init` → `warm` → `new` → `list` → `rm` against a throwaway repo.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn copse(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_copse")).args(args).current_dir(dir).output().unwrap()
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

fn ok(o: Output) -> String {
    assert!(o.status.success(), "{}", text(&o));
    text(&o)
}

/// `git status` in a fresh workspace must trust the copied index: if any entry's
/// stat data mismatched, git would re-check (rehash) it and rewrite the index.
fn assert_index_trusted(ws: &Path) {
    let index = PathBuf::from(git(ws, &["rev-parse", "--path-format=absolute", "--git-path", "index"]));
    let before = fs::read(&index).unwrap();
    assert_eq!(git(ws, &["status", "--porcelain"]), "");
    assert!(fs::read(&index).unwrap() == before, "git status rewrote the index: the copied stat data didn't match");
}

/// Repo with two commits (the second changes Cargo.lock) and a config whose setup
/// "installs" ignored files. Setup commands work in both cmd and sh, and can run
/// twice like real ones (pnpm install, cargo fetch).
fn repo(root: &Path) -> (PathBuf, String, String) {
    let repo = root.join("app");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    fs::write(repo.join(".gitignore"), "deps.txt\nstamp.txt\n").unwrap();
    fs::write(repo.join("Cargo.lock"), "v1\n").unwrap();
    fs::write(repo.join("a.txt"), "one\n").unwrap();
    fs::write(
        repo.join(".copse.toml"),
        "base = \"main\"\n[warm]\nsetup = [\"git --version > deps.txt\", \"git rev-parse HEAD > stamp.txt\"]\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "one"]);
    let c1 = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("Cargo.lock"), "v2\n").unwrap();
    git(&repo, &["commit", "--quiet", "-am", "bump lock"]);
    let c2 = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "--quiet", &c1]);
    git(&repo, &["branch", "--quiet", "-f", "main", &c1]);
    git(&repo, &["checkout", "--quiet", "main"]);
    (repo, c1, c2)
}

#[test]
fn full_workflow() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, c1, c2) = repo(tmp.path());
    let ws_dir = tmp.path().join("app.copse");

    // Before warming, `new` refuses.
    let o = copse(&repo, &["new", "early"]);
    assert!(!o.status.success());
    assert!(text(&o).contains("copse init"), "{}", text(&o));

    // init keeps the existing config and warms.
    let out = ok(copse(&repo, &["init"]));
    assert!(out.contains("already exists"), "{out}");
    let tmpl = ws_dir.join(".template");
    assert!(tmpl.join("deps.txt").exists(), "setup ran in the template");
    assert_eq!(fs::read_to_string(tmpl.join("stamp.txt")).unwrap().trim(), c1);
    assert!(ws_dir.join(".template.ready").exists());

    // new from the template: ignored setup output comes along, status clean.
    let out = ok(copse(&repo, &["new", "feat"]));
    let ws = ws_dir.join("feat");
    if cfg!(windows) {
        // Creation-time preservation alone keeps the index valid on Windows.
        assert_index_trusted(&ws);
    }
    assert!(ws.join("deps.txt").exists(), "{out}");
    assert!(ws.join("stamp.txt").exists());
    assert_eq!(git(&ws, &["branch", "--show-current"]), "feat");
    assert_eq!(git(&ws, &["status", "--porcelain"]), "");

    // new at a commit whose lockfile differs: warns, still works.
    let out = ok(copse(&repo, &["new", "bumped", "--from", &c2]));
    assert!(out.contains("Cargo.lock changed"), "{out}");
    assert_eq!(git(&ws_dir.join("bumped"), &["rev-parse", "HEAD"]), c2);

    // list shows both workspaces, not the template.
    let out = ok(copse(&repo, &["list"]));
    assert!(out.contains("feat") && out.contains("bumped"), "{out}");
    assert!(out.contains("warm"), "{out}");
    assert!(!out.lines().any(|l| l.starts_with(".template")), "{out}");

    // rm refuses a dirty workspace, then --force removes it and its branch.
    fs::write(ws.join("a.txt"), "edited\n").unwrap();
    let o = copse(&repo, &["rm", "feat"]);
    assert!(!o.status.success());
    assert!(ws.exists());
    ok(copse(&repo, &["rm", "feat", "--force", "--delete-branch"]));
    assert!(!ws.exists());
    assert!(!git(&repo, &["worktree", "list"]).contains("feat"));
    assert!(git(&repo, &["branch", "--list", "feat"]).is_empty());

    // A clean workspace goes without --force; its branch is kept.
    ok(copse(&repo, &["rm", "bumped"]));
    assert!(!git(&repo, &["branch", "--list", "bumped"]).is_empty());

    // warm again: template stays, marker rewritten.
    ok(copse(&repo, &["warm", "--no-fetch"]));
    assert!(ws_dir.join(".template.ready").exists());
}

/// The Unix stat config (forced here so it runs on every platform): only the
/// workspace is relaxed; the main worktree and template keep git's full checks.
#[test]
fn relaxed_stat_is_per_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _, _) = repo(tmp.path());
    ok(copse(&repo, &["init"]));
    ok(copse(&repo, &["new", "relaxed", "--relaxed-stat"]));
    let ws = tmp.path().join("app.copse").join("relaxed");
    let tmpl = tmp.path().join("app.copse").join(".template");

    assert_eq!(git(&repo, &["config", "--local", "--get", "extensions.worktreeConfig"]), "true");
    assert_eq!(git(&ws, &["config", "--get", "core.checkStat"]), "minimal");
    assert_eq!(git(&ws, &["config", "--get", "core.trustctime"]), "false");
    for other in [&repo, &tmpl] {
        let out = Command::new("git").arg("-C").arg(other).args(["config", "--get", "core.checkStat"]).output().unwrap();
        assert!(!out.status.success(), "core.checkStat leaked into {}", other.display());
    }
    assert_index_trusted(&ws);
}

#[cfg(windows)]
#[test]
fn windows_needs_no_stat_config() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _, _) = repo(tmp.path());
    ok(copse(&repo, &["init"]));
    ok(copse(&repo, &["new", "plain"]));
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["config", "--local", "--get", "extensions.worktreeConfig"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "creation-time preservation should make the extension unnecessary");
}

#[test]
fn init_writes_config_from_lockfiles() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("web");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "t"]);
    fs::write(repo.join("pnpm-lock.yaml"), "lockfileVersion: 9\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "--quiet", "-m", "one"]);

    let out = ok(copse(&repo, &["init", "--no-warm"]));
    let cfg = fs::read_to_string(repo.join(".copse.toml")).unwrap();
    assert!(cfg.contains("pnpm install --frozen-lockfile"), "{cfg}");
    assert!(cfg.contains("base = \"main\""), "{cfg}");
    assert!(out.contains("copse warm"), "{out}");
}
