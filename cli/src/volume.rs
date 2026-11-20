//! Which volume a path lives on, its filesystem, and whether it can block-clone.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeInfo {
    /// Mount point / volume root (e.g. `E:\`).
    pub root: PathBuf,
    /// Filesystem name as the OS reports it (`ReFS`, `NTFS`, `btrfs`, ...).
    pub fs: String,
    /// Opaque id: equal ids mean the same volume.
    pub id: u64,
    /// The filesystem advertises copy-on-write file cloning.
    pub can_clone: bool,
}

/// Nearest ancestor of `path` (or `path` itself) that exists. Lets us inspect a
/// destination that hasn't been created yet.
pub fn existing_ancestor(path: &Path) -> io::Result<PathBuf> {
    let abs = std::path::absolute(path)?;
    let mut p = abs.as_path();
    loop {
        if p.exists() {
            return Ok(p.to_path_buf());
        }
        p = p.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("no existing ancestor of {}", abs.display()))
        })?;
    }
}

/// Volume info for `path`, which need not exist yet.
pub fn info(path: &Path) -> io::Result<VolumeInfo> {
    imp::info(&existing_ancestor(path)?)
}

pub fn same_volume(a: &Path, b: &Path) -> io::Result<bool> {
    Ok(info(a)?.id == info(b)?.id)
}

#[cfg(windows)]
mod imp {
    use super::VolumeInfo;
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};

    // Win32/System/SystemServices; defined here to avoid pulling in that feature.
    const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        OsString::from_wide(&buf[..end]).to_string_lossy().into_owned()
    }

    pub fn info(path: &Path) -> io::Result<VolumeInfo> {
        let mut root = vec![0u16; 1024];
        // SAFETY: valid NUL-terminated input; output buffer length passed.
        if unsafe { GetVolumePathNameW(wide(path).as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let root = PathBuf::from(from_wide(&root));

        let (mut serial, mut max_len, mut flags) = (0u32, 0u32, 0u32);
        let mut fs = vec![0u16; 64];
        // SAFETY: root is NUL-terminated; out-pointers are valid; name buffer omitted.
        let ok = unsafe {
            GetVolumeInformationW(
                wide(&root).as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                &mut max_len,
                &mut flags,
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(VolumeInfo {
            root,
            fs: from_wide(&fs),
            id: serial as u64,
            can_clone: flags & FILE_SUPPORTS_BLOCK_REFCOUNTING != 0,
        })
    }
}

#[cfg(unix)]
mod imp {
    use super::VolumeInfo;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::process::Command;

    // Copy-on-write providers for macOS/Linux arrive in M3; until then report
    // the filesystem but never claim clone support.
    pub fn info(path: &Path) -> io::Result<VolumeInfo> {
        let id = std::fs::metadata(path)?.dev();
        Ok(VolumeInfo { root: mount_point(path), fs: fs_name(path), id, can_clone: false })
    }

    fn mount_point(path: &Path) -> std::path::PathBuf {
        let dev = std::fs::metadata(path).map(|m| m.dev()).ok();
        let mut p = path.to_path_buf();
        while let Some(parent) = p.parent() {
            if std::fs::metadata(parent).map(|m| m.dev()).ok() != dev {
                break;
            }
            p = parent.to_path_buf();
        }
        p
    }

    fn fs_name(path: &Path) -> String {
        let out = if cfg!(target_os = "macos") {
            Command::new("stat").args(["-f", "%T"]).arg(path).output()
        } else {
            Command::new("stat").args(["-f", "-c", "%T"]).arg(path).output()
        };
        out.ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".into())
    }
}
