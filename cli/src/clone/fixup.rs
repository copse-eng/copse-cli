//! Rewriting absolute paths in cloned files.
//!
//! Some tools write the absolute path of the tree they ran in. pnpm's
//! `node_modules/.bin` shims put the template's `.pnpm/node_modules` on NODE_PATH,
//! and its `.modules.yaml` / `.pnpm-workspace-state-v1.json` name the template's
//! directories, so the first `pnpm install` in a workspace re-links everything.
//! For files matching the fixup globs, the clone writes the content with every
//! form of the source root replaced by the destination root.

use anyhow::{Context, Result};
use std::path::Path;

/// pnpm (and npm/yarn cmd-shim) artifacts that embed absolute paths.
pub const DEFAULT_GLOBS: &[&str] = &[
    "**/node_modules/.bin/*",
    "**/node_modules/.modules.yaml",
    "**/node_modules/.pnpm-workspace-state*.json",
];

pub struct Fixup {
    files: globset::GlobSet,
    /// (from, to) byte strings, longest `from` first.
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Fixup {
    /// Rewrite `from_root` to `to_root` in files matching `globs` (relative paths,
    /// `/`-separated).
    pub fn new<S: AsRef<str>>(globs: &[S], from_root: &Path, to_root: &Path) -> Result<Fixup> {
        let mut b = globset::GlobSetBuilder::new();
        for g in globs {
            let g = g.as_ref();
            b.add(globset::Glob::new(g).with_context(|| format!("bad fixup glob {g:?}"))?);
        }
        let from = path_forms(from_root);
        let to = path_forms(to_root);
        let mut pairs: Vec<_> = from.into_iter().zip(to).map(|(f, t)| (f.into_bytes(), t.into_bytes())).collect();
        pairs.sort_by_key(|(f, _)| std::cmp::Reverse(f.len()));
        pairs.dedup_by(|a, b| a.0 == b.0);
        Ok(Fixup { files: b.build()?, pairs })
    }

    pub fn matches(&self, rel: &Path) -> bool {
        let s = rel.to_string_lossy().replace('\\', "/");
        self.files.is_match(s.as_str())
    }

    /// `data` with every occurrence of the source root replaced, or None if it
    /// contains none.
    pub fn apply(&self, data: &[u8]) -> Option<Vec<u8>> {
        let mut out: Option<Vec<u8>> = None;
        for (from, to) in &self.pairs {
            let cur = out.as_deref().unwrap_or(data);
            if let Some(next) = replace_bounded(cur, from, to) {
                out = Some(next);
            }
        }
        out
    }
}

/// The spellings of an absolute path that tools write: native, forward slashes,
/// JSON-escaped, WSL (`/mnt/e/...`), each with upper and lower drive letters.
/// Index `i` of `path_forms(a)` corresponds to index `i` of `path_forms(b)`.
pub fn path_forms(p: &Path) -> Vec<String> {
    let native = p.to_string_lossy().trim_end_matches(['/', '\\']).to_string();
    let native = native.strip_prefix(r"\\?\").unwrap_or(&native).to_string();
    if !cfg!(windows) {
        return vec![native];
    }
    let mut v = Vec::new();
    for drive_case in [false, true] {
        let n = if drive_case { lower_drive(&native) } else { native.clone() };
        v.push(n.clone());
        v.push(n.replace('\\', "/"));
        v.push(n.replace('\\', r"\\"));
    }
    // WSL form, drive letter always lowercase.
    let fwd = native.replace('\\', "/");
    if let Some((drive, rest)) = fwd.split_once(":/") {
        v.push(format!("/mnt/{}/{}", drive.to_lowercase(), rest));
    }
    v
}

fn lower_drive(s: &str) -> String {
    let mut c = s.chars();
    match (c.next(), c.next()) {
        (Some(d), Some(':')) => format!("{}:{}", d.to_ascii_lowercase(), &s[2..]),
        _ => s.to_string(),
    }
}

/// Replace `from` with `to` where the match ends at a path boundary (so
/// `C:\x\tmpl` doesn't rewrite `C:\x\tmpl2`). None if nothing matched.
fn replace_bounded(hay: &[u8], from: &[u8], to: &[u8]) -> Option<Vec<u8>> {
    if from.is_empty() || hay.len() < from.len() {
        return None;
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    let mut hit = false;
    while i + from.len() <= hay.len() {
        if &hay[i..i + from.len()] == from {
            let next = hay.get(i + from.len()).copied();
            let boundary = !matches!(next, Some(b) if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.');
            if boundary {
                out.extend_from_slice(to);
                i += from.len();
                hit = true;
                continue;
            }
        }
        out.push(hay[i]);
        i += 1;
    }
    out.extend_from_slice(&hay[i..]);
    hit.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn rewrites_every_form_in_a_pnpm_shim() {
        let f = Fixup::new(DEFAULT_GLOBS, Path::new(r"E:\ws\vite\.template"), Path::new(r"E:\ws\vite\b1")).unwrap();
        let shim = "@SET \"NODE_PATH=E:\\ws\\vite\\.template\\node_modules\\.pnpm\\node_modules\"\n\
                    new_node_path='/mnt/e/ws/vite/.template/node_modules/.pnpm/node_modules'\n\
                    # cmd-shim-target=E:/ws/vite/.template/node_modules/vite/bin/vite.js\n\
                    \"virtualStoreDir\": \"E:\\\\ws\\\\vite\\\\.template\\\\node_modules\\\\.pnpm\"\n\
                    lower: e:\\ws\\vite\\.template\\x\n";
        let out = String::from_utf8(f.apply(shim.as_bytes()).unwrap()).unwrap();
        assert!(!out.contains(".template"), "{out}");
        assert!(out.contains(r"E:\ws\vite\b1\node_modules\.pnpm"));
        assert!(out.contains("/mnt/e/ws/vite/b1/node_modules"));
        assert!(out.contains("E:/ws/vite/b1/node_modules/vite"));
        assert!(out.contains(r"E:\\ws\\vite\\b1\\node_modules"));
        assert!(out.contains(r"e:\ws\vite\b1\x"));
    }

    #[test]
    fn respects_path_boundaries() {
        let (from, to) = if cfg!(windows) { (r"C:\r\tmpl", r"C:\r\ws") } else { ("/r/tmpl", "/r/ws") };
        let f = Fixup::new(DEFAULT_GLOBS, Path::new(from), Path::new(to)).unwrap();
        let other = format!("{from}2/x and {from}.bak");
        assert_eq!(f.apply(other.as_bytes()), None);
        let hit = format!("\"{from}\"");
        assert_eq!(String::from_utf8(f.apply(hit.as_bytes()).unwrap()).unwrap(), format!("\"{to}\""));
    }

    #[test]
    fn matches_default_globs() {
        let f = Fixup::new(DEFAULT_GLOBS, Path::new("/a"), Path::new("/b")).unwrap();
        assert!(f.matches(Path::new("node_modules/.bin/vite")));
        assert!(f.matches(&Path::new("packages").join("vite").join("node_modules").join(".bin").join("tsc.cmd")));
        assert!(f.matches(Path::new("node_modules/.modules.yaml")));
        assert!(f.matches(Path::new("node_modules/.pnpm-workspace-state-v1.json")));
        assert!(!f.matches(Path::new("node_modules/vite/index.js")));
        assert!(!f.matches(Path::new("src/.bin/x")));
    }
}
