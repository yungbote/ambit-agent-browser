//! A document's format, read from its bytes. A server's Content-Type and a
//! file's name are hints that are often wrong (`application/octet-stream`,
//! a download named by its id), so neither decides.
//!
//! The containers are read only as far as naming the format needs: a ZIP
//! archive's central directory (its part names, and an OpenDocument's
//! `mimetype`), a compound file's directory (its stream names). Every read
//! is bounded by the file, which the caller already bounded.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::DocumentError;

/// A document format `read` converts to text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Pdf,
    /// Word's Office Open XML: docx, docm, dotx, dotm.
    Docx,
    Odt,
    Rtf,
    /// Word 97-2003, a compound file.
    Doc,
    /// Excel's Office Open XML, the binary xlsb included.
    Xlsx,
    Ods,
    /// Excel 97-2003, a compound file.
    Xls,
    /// PowerPoint's Office Open XML: pptx, pptm, ppsx, potx.
    Pptx,
    Odp,
    /// PowerPoint 97-2003, a compound file.
    Ppt,
}

impl Format {
    /// The name `read` reports the format by.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Odt => "odt",
            Self::Rtf => "rtf",
            Self::Doc => "doc",
            Self::Xlsx => "xlsx",
            Self::Ods => "ods",
            Self::Xls => "xls",
            Self::Pptx => "pptx",
            Self::Odp => "odp",
            Self::Ppt => "ppt",
        }
    }

    /// The format's registered media type, for a document whose server named
    /// none (a download, a file).
    pub(crate) const fn media_type(self) -> &'static str {
        match self {
            Self::Pdf => "application/pdf",
            Self::Docx => {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            }
            Self::Odt => "application/vnd.oasis.opendocument.text",
            Self::Rtf => "application/rtf",
            Self::Doc => "application/msword",
            Self::Xlsx => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            Self::Ods => "application/vnd.oasis.opendocument.spreadsheet",
            Self::Xls => "application/vnd.ms-excel",
            Self::Pptx => {
                "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            }
            Self::Odp => "application/vnd.oasis.opendocument.presentation",
            Self::Ppt => "application/vnd.ms-powerpoint",
        }
    }

    /// The format of the document at `path`, or `None` when its bytes are
    /// not a document `read` converts. An encrypted Office document is
    /// refused: nothing can read it without its password.
    pub(crate) fn of(path: &Path) -> Result<Option<Self>, DocumentError> {
        let mut file = File::open(path).map_err(DocumentError::io)?;
        let mut head = Vec::with_capacity(HEAD);
        (&mut file)
            .take(HEAD as u64)
            .read_to_end(&mut head)
            .map_err(DocumentError::io)?;
        if head.windows(5).any(|window| window == b"%PDF-") {
            Ok(Some(Self::Pdf))
        } else if head.starts_with(b"{\\rtf") {
            Ok(Some(Self::Rtf))
        } else if head.starts_with(ZIP_LOCAL_HEADER) {
            zip_format(&mut file)
        } else if head.starts_with(&COMPOUND_SIGNATURE) {
            compound_format(&mut file)
        } else {
            Ok(None)
        }
    }

    /// Whether a media type names a document format, so that its body is a
    /// document to convert rather than text. Parameters and case are ignored.
    pub(crate) fn is_document_media_type(media_type: &str) -> bool {
        let base = media_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        matches!(
            base.as_str(),
            "application/pdf"
                | "application/x-pdf"
                | "application/rtf"
                | "text/rtf"
                | "application/msword"
                | "application/vnd.ms-excel"
                | "application/vnd.ms-powerpoint"
        ) || base.starts_with("application/vnd.openxmlformats-officedocument.")
            || base.starts_with("application/vnd.oasis.opendocument.")
            || base.starts_with("application/vnd.ms-word.")
            || base.starts_with("application/vnd.ms-excel.")
            || base.starts_with("application/vnd.ms-powerpoint.")
    }

    /// Whether the first bytes of a body are a document's. `read` sniffs a
    /// body whose server named a generic type, since downloads are commonly
    /// served as `application/octet-stream`.
    pub(crate) fn sniffs_as_document(head: &[u8]) -> bool {
        head.windows(5).any(|window| window == b"%PDF-")
            || head.starts_with(b"{\\rtf")
            || head.starts_with(ZIP_LOCAL_HEADER)
            || head.starts_with(&COMPOUND_SIGNATURE)
    }
}

/// A PDF's header may follow up to this much leading junk, which readers
/// accept; the other signatures are at the start.
const HEAD: usize = 1024;

const ZIP_LOCAL_HEADER: &[u8] = b"PK\x03\x04";
const ZIP_END: &[u8; 4] = b"PK\x05\x06";
const ZIP_ENTRY: &[u8; 4] = b"PK\x01\x02";
/// The end record plus its longest comment.
const ZIP_END_SEARCH: u64 = 22 + 0xFFFF;
/// More entries than any Office document has; a directory that declares more
/// is not read.
const ZIP_MAX_ENTRIES: usize = 16_384;
/// A central directory of at most that many entries with ordinary names.
const ZIP_MAX_DIRECTORY: u64 = 4 << 20;

/// The ZIP archive's format, from its central directory: the part that makes
/// it Word, Excel or PowerPoint, or an OpenDocument's `mimetype`.
fn zip_format(file: &mut File) -> Result<Option<Format>, DocumentError> {
    let names = match zip_entries(file)? {
        Some(entries) => entries,
        None => return Ok(None),
    };
    let named = |part: &str| names.iter().any(|(name, _)| name.eq_ignore_ascii_case(part));
    if named("word/document.xml") {
        return Ok(Some(Format::Docx));
    }
    if named("xl/workbook.xml") || named("xl/workbook.bin") {
        return Ok(Some(Format::Xlsx));
    }
    if named("ppt/presentation.xml") {
        return Ok(Some(Format::Pptx));
    }
    let Some(&(_, offset)) = names.iter().find(|(name, _)| name == "mimetype") else {
        return Ok(None);
    };
    let mimetype = stored_entry(file, offset, 128)?;
    Ok(match mimetype.as_deref() {
        Some(b"application/vnd.oasis.opendocument.text") => Some(Format::Odt),
        Some(b"application/vnd.oasis.opendocument.spreadsheet") => Some(Format::Ods),
        Some(b"application/vnd.oasis.opendocument.presentation") => Some(Format::Odp),
        _ => None,
    })
}

/// The central directory's entries as (name, local header offset), or `None`
/// for an archive whose directory cannot be read within its bounds.
fn zip_entries(file: &mut File) -> Result<Option<Vec<(String, u64)>>, DocumentError> {
    let length = file.metadata().map_err(DocumentError::io)?.len();
    let tail_start = length.saturating_sub(ZIP_END_SEARCH);
    let mut tail = Vec::new();
    file.seek(SeekFrom::Start(tail_start))
        .map_err(DocumentError::io)?;
    file.read_to_end(&mut tail).map_err(DocumentError::io)?;
    let Some(end) = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&at| &tail[at..at + 4] == ZIP_END)
    else {
        return Ok(None);
    };
    let record = &tail[end..];
    let entries = u16_at(record, 10) as usize;
    let directory_size = u32_at(record, 12) as u64;
    let directory_offset = u32_at(record, 16) as u64;
    // A ZIP64 archive marks these fields full; no Office document needs one.
    if entries == 0xFFFF
        || entries > ZIP_MAX_ENTRIES
        || directory_offset == 0xFFFF_FFFF
        || directory_size > ZIP_MAX_DIRECTORY
        || directory_offset + directory_size > length
    {
        return Ok(None);
    }
    let mut directory = vec![0u8; directory_size as usize];
    file.seek(SeekFrom::Start(directory_offset))
        .map_err(DocumentError::io)?;
    file.read_exact(&mut directory).map_err(DocumentError::io)?;
    let mut names = Vec::with_capacity(entries);
    let mut at = 0usize;
    for _ in 0..entries {
        let Some(header) = directory.get(at..at + 46) else {
            return Ok(None);
        };
        if &header[..4] != ZIP_ENTRY {
            return Ok(None);
        }
        let name_length = u16_at(header, 28) as usize;
        let extra_length = u16_at(header, 30) as usize;
        let comment_length = u16_at(header, 32) as usize;
        let local_offset = u32_at(header, 42) as u64;
        let Some(name) = directory.get(at + 46..at + 46 + name_length) else {
            return Ok(None);
        };
        names.push((String::from_utf8_lossy(name).into_owned(), local_offset));
        at += 46 + name_length + extra_length + comment_length;
    }
    Ok(Some(names))
}

/// Up to `limit` bytes of a stored (uncompressed) entry, from its local
/// header; `None` for a compressed or larger entry.
fn stored_entry(
    file: &mut File,
    local_offset: u64,
    limit: usize,
) -> Result<Option<Vec<u8>>, DocumentError> {
    let mut header = [0u8; 30];
    file.seek(SeekFrom::Start(local_offset))
        .map_err(DocumentError::io)?;
    if file.read_exact(&mut header).is_err() || &header[..4] != ZIP_LOCAL_HEADER {
        return Ok(None);
    }
    let method = u16_at(&header, 8);
    let size = u32_at(&header, 18) as usize;
    let skip = u16_at(&header, 26) as i64 + u16_at(&header, 28) as i64;
    if method != 0 || size > limit {
        return Ok(None);
    }
    file.seek(SeekFrom::Current(skip))
        .map_err(DocumentError::io)?;
    let mut data = vec![0u8; size];
    if file.read_exact(&mut data).is_err() {
        return Ok(None);
    }
    Ok(Some(data))
}

/// The compound file (OLE2) signature Word, Excel and PowerPoint 97-2003
/// share.
const COMPOUND_SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
/// The highest regular sector number; larger values mark chain ends and
/// special sectors.
const MAX_REGULAR_SECTOR: u32 = 0xFFFF_FFFA;
const DIRECTORY_ENTRY: usize = 128;

/// The compound file's format, from the streams in its directory: Word
/// keeps `WordDocument`, Excel `Workbook` (`Book` before Excel 97),
/// PowerPoint `PowerPoint Document`. An encrypted Office Open XML document
/// is a compound file holding `EncryptedPackage`.
fn compound_format(file: &mut File) -> Result<Option<Format>, DocumentError> {
    let Some(streams) = compound_streams(file)? else {
        return Ok(None);
    };
    let has = |stream: &str| streams.iter().any(|name| name == stream);
    if has("EncryptedPackage") {
        return Err(DocumentError::encrypted());
    }
    Ok(if has("WordDocument") {
        Some(Format::Doc)
    } else if has("Workbook") || has("Book") {
        Some(Format::Xls)
    } else if has("PowerPoint Document") {
        Some(Format::Ppt)
    } else {
        None
    })
}

/// The names of the streams in a compound file's directory (MS-CFB), or
/// `None` for one whose structure is not within its own bounds. The
/// directory's sector chain is followed through the allocation table that
/// the header and its DIFAT sectors locate, one table sector at a time.
/// Every sector number is checked against the file, and no chain may be
/// longer than the file has sectors, so a cycle ends.
fn compound_streams(file: &mut File) -> Result<Option<Vec<String>>, DocumentError> {
    let length = file.metadata().map_err(DocumentError::io)?.len();
    let mut header = [0u8; 512];
    file.seek(SeekFrom::Start(0)).map_err(DocumentError::io)?;
    if file.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    let shift = u16_at(&header, 30);
    if shift != 9 && shift != 12 {
        return Ok(None);
    }
    let compound = Compound {
        shift,
        sectors: (length >> shift).saturating_sub(1),
    };
    let links = compound.size() / 4;
    // The allocation table's own sectors: 109 named in the header, the rest
    // in a chain of DIFAT sectors whose last entry links the next. No more
    // are read than the file's sectors need.
    let needed = (compound.sectors as usize).div_ceil(links);
    let mut table: Vec<u32> = (0..109).map(|index| u32_at(&header, 76 + index * 4)).collect();
    let mut difat = u32_at(&header, 68);
    let mut hops = 0u64;
    while difat <= MAX_REGULAR_SECTOR && table.len() < needed {
        hops += 1;
        let Some(data) = compound.read(file, difat).filter(|_| hops <= compound.sectors) else {
            return Ok(None);
        };
        table.extend((0..links - 1).map(|index| u32_at(&data, index * 4)));
        difat = u32_at(&data, (links - 1) * 4);
    }
    let mut streams = Vec::new();
    let mut sector = u32_at(&header, 48);
    let mut hops = 0u64;
    while sector != END_OF_CHAIN {
        hops += 1;
        let Some(data) = compound.read(file, sector).filter(|_| hops <= compound.sectors) else {
            return Ok(None);
        };
        for entry in data.chunks_exact(DIRECTORY_ENTRY) {
            // Object type 2 is a stream; the name's length counts its
            // terminating null, in bytes of UTF-16.
            let name_bytes = (u16_at(entry, 64) as usize).min(64);
            if entry[66] == 2 && name_bytes >= 2 {
                let units: Vec<u16> = entry[..name_bytes - 2]
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                streams.push(String::from_utf16_lossy(&units));
            }
        }
        // The next sector is this one's entry in the allocation table.
        let Some(next) = table
            .get(sector as usize / links)
            .and_then(|&table_sector| compound.read(file, table_sector))
            .map(|data| u32_at(&data, (sector as usize % links) * 4))
        else {
            return Ok(None);
        };
        sector = next;
    }
    Ok(Some(streams))
}

/// A compound file's geometry: its sector size and how many sectors follow
/// its header.
struct Compound {
    shift: u16,
    sectors: u64,
}

impl Compound {
    fn size(&self) -> usize {
        1 << self.shift
    }

    /// Sector `sector`, or `None` for a special value or one past the end.
    fn read(&self, file: &mut File, sector: u32) -> Option<Vec<u8>> {
        if sector > MAX_REGULAR_SECTOR || u64::from(sector) >= self.sectors {
            return None;
        }
        let mut data = vec![0u8; self.size()];
        file.seek(SeekFrom::Start((u64::from(sector) + 1) << self.shift))
            .ok()?;
        file.read_exact(&mut data).ok()?;
        Some(data)
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
