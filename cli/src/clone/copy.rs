use super::{CloneProvider, Support};
use std::fs::File;
use std::io;
use std::path::Path;

/// Plain byte copy. Always works, just slow and uses full disk space.
pub struct Copy;

impl CloneProvider for Copy {
    fn name(&self) -> &'static str {
        "copy"
    }
    fn is_cow(&self) -> bool {
        false
    }
    fn supported(&self, _src: &Path, _dst: &Path) -> Support {
        Support::Yes
    }
    fn clone_file(&self, src: &File, dst: &File) -> io::Result<()> {
        io::copy(&mut &*src, &mut &*dst).map(|_| ())
    }
}
