//! Shell-out wrapper around `git`.

use std::path::Path;
use std::process::Command;

pub const MIN_VERSION: (u32, u32, u32) = (2, 38, 0);

/// A `git` command with a clean environment (see [`clean_env`]).
pub fn git() -> Command {
    let mut c = Command::new("git");
    clean_env(&mut c);
    c
}

/// Strip cargo/rustup variables, so running copse via `cargo run` doesn't leak
/// toolchain settings into git hooks or a repo's own setup commands (e.g. override
/// its rust-toolchain pin).
pub fn clean_env(c: &mut Command) {
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if (k.starts_with("CARGO") && k != "CARGO_HOME") || k == "RUSTUP_TOOLCHAIN" {
            c.env_remove(k.as_ref());
        }
    }
}

/// Trimmed stdout of a successful git command in `dir`, or None.
pub fn capture(dir: Option<&Path>, args: &[&str]) -> Option<String> {
    let mut c = git();
    if let Some(d) = dir {
        c.arg("-C").arg(d);
    }
    let out = c.args(args).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    out.status.success().then_some(s)
}

/// Run git in `dir`; trimmed stdout on success, an error carrying stderr otherwise.
pub fn run<S: AsRef<std::ffi::OsStr>>(dir: &Path, args: &[S]) -> anyhow::Result<String> {
    let out = git().arg("-C").arg(dir).args(args).output()?;
    if !out.status.success() {
        let shown: Vec<_> = args.iter().map(|a| a.as_ref().to_string_lossy().into_owned()).collect();
        anyhow::bail!(
            "git {} failed in {}:\n{}",
            shown.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A path git printed (`C:/x/y` on Windows), in native form.
pub fn native_path(s: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(if cfg!(windows) { s.replace('/', "\\") } else { s.to_string() })
}

/// Raw `git --version` output, e.g. `git version 2.56.1.windows.1`.
pub fn version_string() -> Option<String> {
    capture(None, &["--version"])
}

/// Parse `git version 2.56.1.windows.1` → (2, 56, 1). Missing parts are 0.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let v = s.trim().strip_prefix("git version ")?.split_whitespace().next()?;
    let mut nums = v.split('.').map(|p| p.parse::<u32>().ok());
    let major = nums.next()??;
    let minor = nums.next().flatten().unwrap_or(0);
    let patch = nums.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("git version 2.56.1.windows.1"), Some((2, 56, 1)));
        assert_eq!(parse_version("git version 2.39.5 (Apple Git-154)"), Some((2, 39, 5)));
        assert_eq!(parse_version("git version 2.43.0\n"), Some((2, 43, 0)));
        assert_eq!(parse_version("git version 3"), Some((3, 0, 0)));
        assert_eq!(parse_version("nope"), None);
        assert!(parse_version("git version 2.37.1").unwrap() < MIN_VERSION);
    }
}
