//! Extended attributes and ACLs in AppleDouble form, for the `__MACOSX/`
//! entries of a ZIP archive.
//!
//! The system encodes them: `fcopyfile(COPYFILE_PACK)` writes the same
//! AppleDouble file that `ditto --sequesterRsrc` stores, and Archive Utility
//! and `ditto -x` restore it. Code signatures of files that are not Mach-O
//! live in such attributes (`com.apple.cs.*`), so an app bundle stays valid
//! only with them.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::ptr;

/// Packs the metadata of open objects, one after the other, through one
/// scratch file.
///
/// The scratch file is anonymous and made at the first object that has
/// extended attributes; a job uses one packer.
#[derive(Debug, Default)]
pub struct AppleDouble {
    scratch: Option<File>,
}

impl AppleDouble {
    /// A packer without a scratch file yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The extended attributes and ACL of `source` in AppleDouble form, or
    /// `None` when it has no extended attributes or its volume keeps none.
    ///
    /// Reads only through the descriptor, never by path.
    ///
    /// # Errors
    ///
    /// The error of the system when the attributes can't be listed or
    /// packed, or the scratch file can't be used.
    pub fn pack(&mut self, source: &File) -> io::Result<Option<Vec<u8>>> {
        if !has_extended_attributes(source)? {
            return Ok(None);
        }
        let scratch = match &mut self.scratch {
            Some(scratch) => scratch,
            None => self.scratch.insert(tempfile::tempfile()?),
        };
        scratch.set_len(0)?;
        scratch.seek(SeekFrom::Start(0))?;
        let flags = libc::COPYFILE_PACK | libc::COPYFILE_XATTR | libc::COPYFILE_ACL;
        // SAFETY: both descriptors stay open for the call, which only reads
        // the metadata of the first and writes to the second; a null state
        // is allowed.
        let code = unsafe {
            libc::fcopyfile(
                source.as_raw_fd(),
                scratch.as_raw_fd(),
                ptr::null_mut(),
                flags,
            )
        };
        if code < 0 {
            return Err(io::Error::last_os_error());
        }
        scratch.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        scratch.read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }
}

/// Whether `source` has extended attributes. A volume that keeps none has
/// nothing to store.
fn has_extended_attributes(source: &File) -> io::Result<bool> {
    // SAFETY: a null buffer of size 0 asks only for the size of the list.
    let size = unsafe { libc::flistxattr(source.as_raw_fd(), ptr::null_mut(), 0, 0) };
    if size >= 0 {
        return Ok(size > 0);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOTSUP | libc::EOPNOTSUPP) => Ok(false),
        _ => Err(error),
    }
}
