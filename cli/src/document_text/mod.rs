//! The text of PDF and Office documents, for `read`: a PDF the tab shows, a
//! document at a URL, a document the browser downloaded. A document is
//! untrusted input from the web, so its size is bounded before anything reads
//! it, and it is converted by a program run confined (`confine`): without a
//! network, with bounded memory, time, files and output, and killed with its
//! process group when it overruns. A failure is an error with a code, never a
//! hang and never partial text presented as whole: text cut at the output
//! bound says `truncated`, as a long page's does.
//!
//! Converters are the image's own: poppler's `pdftotext` for PDF, `pandoc`
//! for the word processing formats it reads, and LibreOffice for the rest,
//! which it first turns into something those two read (a spreadsheet into
//! one CSV per sheet).

mod convert;
#[cfg(target_os = "linux")]
mod confine;
mod format;

use std::fmt;
use std::path::{Path, PathBuf};

pub(crate) use convert::Text;
pub(crate) use format::Format;

/// The largest document `read` converts, whether a server declares it, a
/// stream delivers it or a download holds it.
pub(crate) const MAX_DOCUMENT_BYTES: u64 = 64 << 20;

/// The longest a document's conversion may take, however long its caller
/// waits; the caller's own deadline may end it sooner.
pub(crate) const CONVERSION_TIME: std::time::Duration = std::time::Duration::from_secs(60);

/// A document that could not be read, as `read` reports it: a code the host
/// and scripts match on, and a message that says what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocumentError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

/// The codes, each named once. The daemon keeps a coded message as it is
/// (`native::browser::error_code`): every one starts with `document_`.
pub(crate) mod code {
    pub(crate) const TOO_LARGE: &str = "document_too_large";
    pub(crate) const UNSUPPORTED: &str = "document_unsupported";
    pub(crate) const ENCRYPTED: &str = "document_encrypted";
    pub(crate) const NO_TEXT: &str = "document_has_no_text";
    pub(crate) const CONVERTER_UNAVAILABLE: &str = "document_converter_unavailable";
    pub(crate) const CONVERSION_FAILED: &str = "document_conversion_failed";
    pub(crate) const CONVERSION_TIMEOUT: &str = "document_conversion_timeout";
    pub(crate) const FETCH_FAILED: &str = "document_fetch_failed";
    pub(crate) const NOT_FOUND: &str = "document_not_found";
}

impl DocumentError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(crate) fn too_large(bytes: Option<u64>) -> Self {
        let size = bytes.map_or_else(
            || "more than 64 MB".to_string(),
            |bytes| format!("{:.1} MB", bytes as f64 / 1_000_000.0),
        );
        Self::new(
            code::TOO_LARGE,
            format!("The document is {size}; read converts documents of at most 64 MB. Open it in the tab and take screenshots of the pages you need."),
        )
    }

    pub(crate) fn encrypted() -> Self {
        Self::new(
            code::ENCRYPTED,
            "The document is protected by a password, so its text cannot be read. Ask the person to open it, or for an unprotected copy.",
        )
    }

    /// A failure of the daemon's own files, not of the document.
    pub(crate) fn io(error: std::io::Error) -> Self {
        Self::new(
            code::CONVERSION_FAILED,
            format!("The document could not be prepared for conversion: {error}"),
        )
    }
}

impl fmt::Display for DocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl From<DocumentError> for String {
    fn from(error: DocumentError) -> Self {
        error.to_string()
    }
}

/// A document held for conversion: its bytes in a file of its own, in a
/// private working directory that goes when the document does. It arrives
/// as a stream ([`Document::receive`]) or as a copy of a file the browser
/// downloaded ([`Document::copy_of`]), bounded either way; the original is
/// never handed to a converter, which may write beside what it reads (as
/// LibreOffice writes its lock files).
pub(crate) struct Document {
    work: WorkDirectory,
    path: PathBuf,
}

impl Document {
    /// A document about to arrive as a stream, refused at once when its
    /// declared length is over the bound.
    pub(crate) fn receive(declared: Option<u64>) -> Result<Receiver, DocumentError> {
        if let Some(declared) = declared.filter(|&length| length > MAX_DOCUMENT_BYTES) {
            return Err(DocumentError::too_large(Some(declared)));
        }
        let work = WorkDirectory::create()?;
        let path = work.path().join("document");
        let file = std::fs::File::create(&path).map_err(DocumentError::io)?;
        Ok(Receiver {
            document: Self { work, path },
            file,
            received: 0,
        })
    }

    /// A copy of the regular file at `source`, refused over the bound.
    pub(crate) fn copy_of(source: &Path) -> Result<Self, DocumentError> {
        use std::io::Read;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(source).map_err(DocumentError::io)?;
        let metadata = file.metadata().map_err(DocumentError::io)?;
        if !metadata.is_file() {
            return Err(DocumentError::new(
                code::UNSUPPORTED,
                "The document is not a regular file.",
            ));
        }
        let mut receiver = Self::receive(Some(metadata.len()))?;
        let mut file = file.take(MAX_DOCUMENT_BYTES + 1);
        let mut chunk = vec![0u8; 256 * 1024];
        loop {
            let read = file.read(&mut chunk).map_err(DocumentError::io)?;
            if read == 0 {
                return receiver.finish();
            }
            receiver.write(&chunk[..read])?;
        }
    }

    pub(crate) fn format(&self) -> Result<Option<Format>, DocumentError> {
        Format::of(&self.path)
    }

    /// The bytes as text, when they are text: UTF-8 with no NUL in its first
    /// block. At most `max_bytes` of it, and whether it goes on.
    pub(crate) fn plain_text(&self, max_bytes: usize) -> Result<Option<(String, bool)>, DocumentError> {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(&self.path)
            .map_err(DocumentError::io)?
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(DocumentError::io)?;
        let truncated = bytes.len() > max_bytes;
        bytes.truncate(max_bytes);
        if bytes[..bytes.len().min(8192)].contains(&0) {
            return Ok(None);
        }
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            // A cut can split the last character; anything else is not text.
            Err(error) if truncated && error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or_default()
            }
            Err(_) => return Ok(None),
        };
        Ok(Some((text.to_string(), truncated)))
    }

    /// The document's text, converted within `deadline` (and never longer
    /// than [`CONVERSION_TIME`]), at most `max_bytes` of it.
    pub(crate) async fn convert(
        self,
        format: Format,
        max_bytes: usize,
        deadline: tokio::time::Instant,
    ) -> Result<Text, DocumentError> {
        // LibreOffice reads the kind of a document from its name too.
        let named = self.work.path().join(format!("document.{}", format.name()));
        std::fs::rename(&self.path, &named).map_err(DocumentError::io)?;
        let deadline = deadline.min(tokio::time::Instant::now() + CONVERSION_TIME);
        convert::run(&named, format, &self.work, max_bytes, deadline).await
    }
}

/// A document arriving: its bytes written as they come, refused the moment
/// they pass the bound, so a stream that never ends costs at most the bound.
pub(crate) struct Receiver {
    document: Document,
    file: std::fs::File,
    received: u64,
}

impl Receiver {
    pub(crate) fn write(&mut self, chunk: &[u8]) -> Result<(), DocumentError> {
        use std::io::Write;
        self.received += chunk.len() as u64;
        if self.received > MAX_DOCUMENT_BYTES {
            return Err(DocumentError::too_large(None));
        }
        self.file.write_all(chunk).map_err(DocumentError::io)
    }

    pub(crate) fn finish(self) -> Result<Document, DocumentError> {
        self.file.sync_data().map_err(DocumentError::io)?;
        Ok(self.document)
    }
}

/// A private directory a conversion works in and its converters write to,
/// removed with everything in it when the conversion ends.
pub(crate) struct WorkDirectory {
    path: PathBuf,
}

impl WorkDirectory {
    pub(crate) fn create() -> Result<Self, DocumentError> {
        let path = std::env::temp_dir().join(format!(
            "agent-browser-document-{}",
            uuid::Uuid::new_v4()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(DocumentError::io)?;
        Ok(Self { path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
