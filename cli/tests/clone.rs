use copse::clone::{self, clone_tree, CloneProvider};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const OLD: Duration = Duration::from_secs(1_600_000_000);

/// Build a source tree. Returns whether links could be created (Windows needs
/// Developer Mode for symlinks), so link assertions can be skipped otherwise.
fn build_tree(src: &Path, outside: &Path) -> bool {
    fs::create_dir_all(src.join("a/b/c")).unwrap();
    fs::create_dir_all(src.join("node_modules/.pnpm/pkg@1/node_modules/pkg")).unwrap();
    fs::create_dir_all(outside).unwrap();
    fs::write(src.join("top.txt"), "top").unwrap();
    fs::write(src.join("a/b/c/deep.txt"), "deep").unwrap();
    fs::write(src.join("empty"), "").unwrap();
    fs::write(src.join("node_modules/.pnpm/pkg@1/node_modules/pkg/index.js"), "x").unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
    fs::write(src.join("a/big.bin"), &big).unwrap();
    fs::write(src.join(".git"), "gitdir: elsewhere").unwrap();

    // Old mtime: git's index matches on it.
    let f = fs::OpenOptions::new().write(true).open(src.join("top.txt")).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + OLD).unwrap();
    drop(f);

    let ro = src.join("readonly.txt");
    fs::write(&ro, "ro").unwrap();
    let mut p = fs::metadata(&ro).unwrap().permissions();
    p.set_readonly(true);
    fs::set_permissions(&ro, p).unwrap();

    let inside_target = src.join("node_modules/.pnpm/pkg@1/node_modules/pkg");
    let outside_target = outside.to_path_buf();
    #[cfg(windows)]
    {
        // pnpm on Windows uses junctions; they need no special privilege.
        junction::create(&inside_target, src.join("node_modules/pkg")).unwrap();
        let a = std::os::windows::fs::symlink_dir(&inside_target, src.join("abs-inside"));
        let b = std::os::windows::fs::symlink_dir(&outside_target, src.join("abs-outside"));
        let c = std::os::windows::fs::symlink_file("top.txt", src.join("rel-link"));
        a.is_ok() && b.is_ok() && c.is_ok()
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(&inside_target, src.join("node_modules/pkg")).unwrap();
        symlink(&inside_target, src.join("abs-inside")).unwrap();
        symlink(&outside_target, src.join("abs-outside")).unwrap();
        symlink("top.txt", src.join("rel-link")).unwrap();
        true
    }
}

fn check_tree(src: &Path, dst: &Path, symlinks: bool) {
    for f in ["top.txt", "a/b/c/deep.txt", "empty", "a/big.bin", "readonly.txt"] {
        assert_eq!(fs::read(src.join(f)).unwrap(), fs::read(dst.join(f)).unwrap(), "{f}");
        let (ms, md) = (fs::metadata(src.join(f)).unwrap(), fs::metadata(dst.join(f)).unwrap());
        assert_eq!(ms.modified().unwrap(), md.modified().unwrap(), "mtime of {f}");
        #[cfg(windows)]
        assert_eq!(ms.created().unwrap(), md.created().unwrap(), "creation time of {f}");
    }
    assert!(!dst.join(".git").exists(), ".git should be skipped");
    assert!(fs::metadata(dst.join("readonly.txt")).unwrap().permissions().readonly());
    assert!(!fs::metadata(dst.join("top.txt")).unwrap().permissions().readonly());

    // The junction / pnpm-style link must point into dst, not back at src.
    let pkg = dst.join("node_modules/pkg");
    let want = dst.join("node_modules/.pnpm/pkg@1/node_modules/pkg");
    assert_eq!(link_target(&pkg), want);
    assert_eq!(fs::read_to_string(pkg.join("index.js")).unwrap(), "x");

    if symlinks {
        assert_eq!(link_target(&dst.join("abs-inside")), want);
        assert_eq!(link_target(&dst.join("abs-outside")), link_target(&src.join("abs-outside")));
        assert_eq!(fs::read_link(dst.join("rel-link")).unwrap(), PathBuf::from("top.txt"));
    }
}

fn link_target(p: &Path) -> PathBuf {
    #[cfg(windows)]
    if junction::exists(p).unwrap() {
        return junction::get_target(p).unwrap();
    }
    let t = fs::read_link(p).unwrap();
    PathBuf::from(t.to_string_lossy().trim_start_matches(r"\\?\").to_string())
}

fn run(provider: &dyn CloneProvider, root: &Path) -> clone::Stats {
    let (src, dst, outside) = (root.join("src"), root.join("dst"), root.join("outside"));
    let symlinks = build_tree(&src, &outside);
    let stats = clone_tree(provider, &src, &dst, &|rel| rel == Path::new(".git"), None).unwrap();
    check_tree(&src, &dst, symlinks);
    assert_eq!(stats.files, 6);
    stats
}

#[test]
fn copy_provider_clones_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let s = run(&clone::Copy, tmp.path());
    assert_eq!((s.cloned, s.copied), (0, 6));
}

#[test]
fn refuses_non_empty_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let (src, dst) = (tmp.path().join("s"), tmp.path().join("d"));
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    fs::write(dst.join("x"), "").unwrap();
    assert!(clone_tree(&clone::Copy, &src, &dst, &|_| false, None).is_err());
}

/// Needs a ReFS / Dev Drive folder: `COPSE_TEST_REFS_DIR=E:\copse-test cargo test -p copse -- --ignored`
#[cfg(windows)]
#[test]
#[ignore]
fn refs_provider_block_clones_tree() {
    let base = PathBuf::from(std::env::var("COPSE_TEST_REFS_DIR").expect("set COPSE_TEST_REFS_DIR"));
    fs::create_dir_all(&base).unwrap();
    let tmp = tempfile::tempdir_in(&base).unwrap();
    let p = clone::Refs;
    assert_eq!(p.supported(tmp.path(), &tmp.path().join("dst")), clone::Support::Yes);
    let s = run(&p, tmp.path());
    // big.bin (3 MB) is always cloned. Small files are cloned too when the write
    // probe finds antivirus scanning, otherwise written; either way all 6 land.
    assert!(s.write_probe_us.is_some());
    assert_eq!(s.cloned + s.copied, 6);
    let expect_cloned = if s.clone_min_bytes == 0 { 6 } else { 1 };
    assert_eq!(s.cloned, expect_cloned, "probe {:?} µs/file", s.write_probe_us);
}
