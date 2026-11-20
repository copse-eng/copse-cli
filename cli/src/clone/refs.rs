use super::{CloneProvider, Support};
use crate::volume;
use std::fs::File;
use std::io;
use std::path::Path;

/// Windows block cloning (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`) on ReFS, e.g. a Dev Drive.
/// `reflink-copy` handles cluster alignment, sparse files and chunking.
pub struct Refs;

impl CloneProvider for Refs {
    fn name(&self) -> &'static str {
        "refs-blockclone"
    }
    fn is_cow(&self) -> bool {
        true
    }
    fn supported(&self, src: &Path, dst: &Path) -> Support {
        let (s, d) = match (volume::info(src), volume::info(dst)) {
            (Ok(s), Ok(d)) => (s, d),
            (Err(e), _) | (_, Err(e)) => return Support::No(format!("can't read volume info: {e}")),
        };
        if s.id != d.id {
            return Support::No(format!(
                "source ({}) and destination ({}) are on different volumes",
                s.root.display(),
                d.root.display()
            ));
        }
        if !s.can_clone {
            return Support::No(format!("{} ({}) doesn't support block cloning", s.root.display(), s.fs));
        }
        Support::Yes
    }
    fn clone_file(&self, src: &File, dst: &File) -> io::Result<()> {
        reflink_copy::reflink_file(src, dst)
    }
}
