//! Temporary custody bytes, streamed without a whole-frame allocation. These
//! bytes are never a saved browser profile or a second durable custody store.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// One work unit fits below the existing 2 MiB control budget even after
/// base64. Total site capacity is independent of this allocation bound.
pub(crate) const CHUNK_BYTES: usize = 1 << 20;

pub(crate) struct Bytes {
    file: File,
    path: Option<PathBuf>,
    length: u64,
}

impl Bytes {
    pub(crate) fn new() -> std::io::Result<Self> {
        let path =
            std::env::temp_dir().join(format!(".ambit-site-custody-{}", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut bytes = Self {
            file: options.open(&path)?,
            path: Some(path),
            length: 0,
        };
        #[cfg(unix)]
        {
            std::fs::remove_file(bytes.path.as_ref().unwrap())?;
            bytes.path = None;
        }
        Ok(bytes)
    }

    pub(crate) fn reader(&mut self) -> std::io::Result<BufReader<&mut File>> {
        self.file.seek(SeekFrom::Start(0))?;
        Ok(BufReader::new(&mut self.file))
    }

    pub(crate) fn length(&self) -> u64 {
        self.length
    }

    pub(crate) fn chunk(&mut self, offset: u64, limit: usize) -> std::io::Result<Vec<u8>> {
        if offset > self.length {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        }
        self.file.seek(SeekFrom::Start(offset))?;
        let count = (self.length - offset).min(limit as u64) as usize;
        let mut chunk = vec![0; count];
        self.file.read_exact(&mut chunk)?;
        Ok(chunk)
    }
}

impl Write for Bytes {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        // Readback may have moved the cursor; every producer append retains
        // its exact byte offset, rather than overwriting a previous fragment.
        self.file.seek(SeekFrom::Start(self.length))?;
        let written = self.file.write(input)?;
        self.length = self
            .length
            .checked_add(written as u64)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::FileTooLarge))?;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl Drop for Bytes {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_bytes_keep_exact_offsets_across_readback() {
        let mut bytes = Bytes::new().unwrap();
        bytes.write_all(b"first").unwrap();
        assert_eq!(bytes.chunk(0, 2).unwrap(), b"fi");
        bytes.write_all("雪".as_bytes()).unwrap();
        assert_eq!(bytes.length(), 8);
        assert_eq!(bytes.chunk(5, 64).unwrap(), "雪".as_bytes());
        assert!(bytes.chunk(9, 64).is_err());
        assert!(bytes.chunk(8, 64).unwrap().is_empty());
        let mut restored = String::new();
        bytes
            .reader()
            .unwrap()
            .read_to_string(&mut restored)
            .unwrap();
        assert_eq!(restored, "first雪");
        #[cfg(unix)]
        assert!(
            bytes.path.is_none(),
            "temporary plaintext has no directory entry"
        );
    }
}
