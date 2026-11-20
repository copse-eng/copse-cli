//! Shell-out wrapper around `git`.

use std::path::Path;
use std::process::Command;

pub const MIN_VERSION: (u32, u32, u32) = (2, 38, 0);

/// A `git` command with cargo/rustup env stripped, so `cargo run` doesn't leak
/// toolchain settings into hooks or nested builds.
pub fn git() -> Command {
    let mut c = Command::new("git");
    for (k, _) in std::env::vars_os() {
        let k = k.to_string_lossy();
        if (k.starts_with("CARGO") && k != "CARGO_HOME") || k == "RUSTUP_TOOLCHAIN" {
            c.env_remove(k.as_ref());
        }
    }
    c
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
