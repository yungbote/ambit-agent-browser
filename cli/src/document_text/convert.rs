//! Each format's way to text, through the image's converters: poppler's
//! `pdftotext` (in layout mode, which keeps a table's columns in line),
//! `pandoc` for the word processing formats it reads (into GitHub Markdown,
//! headings, lists and tables kept), and LibreOffice for the rest, which it
//! turns into what those read: a Word 97-2003 file into docx, a presentation
//! into PDF, a spreadsheet into one CSV per sheet.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use tokio::time::Instant;

use super::{code, DocumentError, Format, WorkDirectory};

/// A document's text, as `read` returns it: Markdown where the converter
/// keeps structure, each page or slide after a `<!-- page N -->` or
/// `<!-- slide N -->` line, each sheet under its name as CSV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Text {
    pub(crate) content: String,
    /// The text stopped at its bound; the document goes on.
    pub(crate) truncated: bool,
}

#[cfg(target_os = "linux")]
use super::confine::Program;

#[cfg(target_os = "linux")]
const PDFTOTEXT: Program = Program {
    command: "pdftotext",
    what: "pdftotext (poppler)",
};
#[cfg(target_os = "linux")]
const PANDOC: Program = Program {
    command: "pandoc",
    what: "pandoc",
};
#[cfg(target_os = "linux")]
const LIBREOFFICE: Program = Program {
    command: "soffice",
    what: "LibreOffice",
};

/// The text of `document`, a private copy in `work` known to be `format`.
#[cfg(target_os = "linux")]
pub(super) async fn run(
    document: &Path,
    format: Format,
    work: &WorkDirectory,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Text, DocumentError> {
    match format {
        Format::Pdf => paged(pdf_text(document, work, max_bytes, deadline).await?, "page"),
        Format::Docx | Format::Odt | Format::Rtf => {
            markdown(document, format, work, max_bytes, deadline).await
        }
        Format::Doc => {
            let docx = office(document, "docx", work, deadline).await?;
            markdown(&docx, Format::Docx, work, max_bytes, deadline).await
        }
        Format::Pptx | Format::Odp | Format::Ppt => {
            let pdf = office(document, "pdf", work, deadline).await?;
            paged(pdf_text(&pdf, work, max_bytes, deadline).await?, "slide")
        }
        Format::Xlsx | Format::Ods | Format::Xls => sheets(document, work, max_bytes, deadline).await,
    }
}

/// Converters run only where they can be confined.
#[cfg(not(target_os = "linux"))]
pub(super) async fn run(
    _document: &Path,
    _format: Format,
    _work: &WorkDirectory,
    _max_bytes: usize,
    _deadline: Instant,
) -> Result<Text, DocumentError> {
    Err(DocumentError::new(
        code::CONVERTER_UNAVAILABLE,
        "Reading PDF and Office documents needs Linux, where their converters run confined.",
    ))
}

#[cfg(target_os = "linux")]
async fn pdf_text(
    pdf: &Path,
    work: &WorkDirectory,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Text, DocumentError> {
    let ran = PDFTOTEXT
        .run(
            &[
                "-layout".into(),
                "-enc".into(),
                "UTF-8".into(),
                pdf.into(),
                "-".into(),
            ],
            work.path(),
            max_bytes,
            deadline,
        )
        .await?;
    Ok(Text {
        content: String::from_utf8_lossy(&ran.stdout).into_owned(),
        truncated: ran.truncated,
    })
}

#[cfg(target_os = "linux")]
async fn markdown(
    document: &Path,
    format: Format,
    work: &WorkDirectory,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Text, DocumentError> {
    // --sandbox confines pandoc's own reads to the file it is given, so a
    // document cannot include another file; the extracted media stay out.
    let ran = PANDOC
        .run(
            &[
                "--sandbox".into(),
                format!("--from={}", format.name()).into(),
                "--to=gfm".into(),
                "--wrap=none".into(),
                document.into(),
            ],
            work.path(),
            max_bytes,
            deadline,
        )
        .await?;
    Ok(Text {
        content: String::from_utf8_lossy(&ran.stdout).trim().to_string(),
        truncated: ran.truncated,
    })
}

/// LibreOffice's conversion of `document` into `target` (a format name, or
/// LibreOffice's `format:filter` form) in a fresh output directory, with a
/// fresh profile. Answers the converted file, or for several (one per
/// sheet) the directory and LibreOffice's report of what it wrote.
#[cfg(target_os = "linux")]
async fn office_run(
    document: &Path,
    target: &str,
    work: &WorkDirectory,
    deadline: Instant,
) -> Result<(PathBuf, String), DocumentError> {
    let output = work.path().join("converted");
    let profile = work.path().join("profile");
    std::fs::create_dir(&output).map_err(DocumentError::io)?;
    let mut installation = OsString::from("-env:UserInstallation=file://");
    installation.push(&profile);
    let ran = LIBREOFFICE
        .run(
            &[
                installation,
                "--headless".into(),
                "--norestore".into(),
                "--nologo".into(),
                "--nodefault".into(),
                "--nolockcheck".into(),
                "--convert-to".into(),
                target.into(),
                "--outdir".into(),
                output.clone().into(),
                document.into(),
            ],
            work.path(),
            64 * 1024,
            deadline,
        )
        .await?;
    Ok((output, String::from_utf8_lossy(&ran.stdout).into_owned()))
}

#[cfg(target_os = "linux")]
async fn office(
    document: &Path,
    extension: &str,
    work: &WorkDirectory,
    deadline: Instant,
) -> Result<PathBuf, DocumentError> {
    let (output, _) = office_run(document, extension, work, deadline).await?;
    let converted = output.join(
        document
            .with_extension(extension)
            .file_name()
            .unwrap_or_default(),
    );
    if converted.is_file() {
        Ok(converted)
    } else {
        Err(DocumentError::new(
            code::CONVERSION_FAILED,
            "LibreOffice could not convert the document. It may be damaged or of a kind it does not read.",
        ))
    }
}

/// One CSV per sheet, in the workbook's order, each under its sheet's name.
/// LibreOffice names each file after its sheet and reports each as
/// `Writing sheet <name> -> <file>`; a file its report does not name follows,
/// by name.
#[cfg(target_os = "linux")]
async fn sheets(
    document: &Path,
    work: &WorkDirectory,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Text, DocumentError> {
    // Comma-separated, double-quoted, UTF-8, formulas' results as shown,
    // every sheet (the last field, -1) to its own file.
    const TO_CSV: &str =
        "csv:Text - txt - csv (StarCalc):44,34,UTF8,1,,0,false,true,false,false,false,-1";
    let (output, report) = office_run(document, TO_CSV, work, deadline).await?;
    let stem = document
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("document");
    let mut files: Vec<PathBuf> = report
        .lines()
        .filter(|line| line.starts_with("Writing sheet "))
        .filter_map(|line| line.rsplit_once(" -> ").map(|(_, file)| PathBuf::from(file)))
        .filter(|file| file.parent() == Some(output.as_path()))
        .collect();
    let mut rest: Vec<PathBuf> = std::fs::read_dir(&output)
        .map_err(DocumentError::io)?
        .flatten()
        .map(|entry| entry.path())
        .filter(|file| !files.contains(file))
        .collect();
    rest.sort();
    files.extend(rest);
    let mut content = String::new();
    let mut truncated = false;
    for file in files {
        let name = file
            .file_stem()
            .and_then(|name| name.to_str())
            .map(|name| {
                name.strip_prefix(stem)
                    .and_then(|name| name.strip_prefix('-'))
                    .unwrap_or(name)
            })
            .unwrap_or("Sheet")
            .to_string();
        let room = max_bytes.saturating_sub(content.len());
        let (csv, cut) = read_prefix(&file, room)?;
        let csv = csv.trim_end();
        let fence = fence_for(csv);
        let section = format!("## {name}\n\n{fence}csv\n{csv}\n{fence}\n\n");
        if section.len() > room || cut {
            content.push_str(&section[..floor_char_boundary(&section, room)]);
            truncated = true;
            break;
        }
        content.push_str(&section);
    }
    if content.is_empty() {
        return Err(DocumentError::new(
            code::CONVERSION_FAILED,
            "LibreOffice could not convert the spreadsheet. It may be damaged or of a kind it does not read.",
        ));
    }
    Ok(Text {
        content: content.trim_end().to_string(),
        truncated,
    })
}

/// A file's first `limit` bytes as text, and whether it goes on.
#[cfg(target_os = "linux")]
fn read_prefix(file: &Path, limit: usize) -> Result<(String, bool), DocumentError> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .map_err(DocumentError::io)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(DocumentError::io)?;
    let cut = bytes.len() > limit;
    bytes.truncate(limit);
    Ok((String::from_utf8_lossy(&bytes).into_owned(), cut))
}

/// A code fence longer than any run of backticks in `text`.
fn fence_for(text: &str) -> String {
    let longest = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
    (0..=index.min(text.len()))
        .rev()
        .find(|&at| text.is_char_boundary(at))
        .unwrap_or(0)
}

/// `pdftotext`'s pages (each ended by a form feed) as `<!-- page N -->`
/// sections. A PDF with pages but no text on any is refused: it is scanned,
/// and its text is only in its pictures.
fn paged(text: Text, unit: &str) -> Result<Text, DocumentError> {
    let pages: Vec<&str> = text
        .content
        .strip_suffix('\u{c}')
        .unwrap_or(&text.content)
        .split('\u{c}')
        .collect();
    if !text.truncated && pages.iter().all(|page| page.trim().is_empty()) {
        return Err(DocumentError::new(
            code::NO_TEXT,
            format!(
                "The document has {} {unit}{} but no text in them: it is scanned or made of pictures. Open it in the tab and take a screenshot of each {unit} to read it.",
                pages.len(),
                if pages.len() == 1 { "" } else { "s" },
            ),
        ));
    }
    let content = pages
        .iter()
        .enumerate()
        .map(|(index, page)| {
            let page = page.trim_end();
            let page = page.trim_start_matches('\n');
            format!("<!-- {unit} {} -->\n\n{page}", index + 1)
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(Text {
        content,
        truncated: text.truncated,
    })
}
