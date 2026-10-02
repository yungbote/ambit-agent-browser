//! The document a tab shows, read with the browser's own session. Chrome
//! shows a PDF in its viewer, a page of Chrome's own around the file, so the
//! page's HTML holds none of the PDF's text. Its bytes are fetched again the
//! way the browser fetched them, with its cookies, proxy and certificate
//! trust: `Network.loadNetworkResource` for a web address; for a `blob:` or
//! `data:` address (a PDF a page made), which the browser's network does not
//! load, the page's own fetch of its address, read through its Blob. Either
//! way the bytes arrive in bounded chunks into a [`Document`].

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use super::browser::BrowserManager;
use crate::document_text::{code, Document, DocumentError, Format};

/// How much one read of the browser's stream asks for.
const CHUNK: u64 = 1 << 20;

/// The active page, when it shows a document rather than a web page.
pub(crate) struct Shown {
    frame_id: String,
    pub(crate) url: String,
    pub(crate) media_type: String,
}

/// The active page's document, or `None` for a web page: a document is a
/// page whose main frame Chrome loaded as a document's media type.
pub(crate) async fn shown(browser: &BrowserManager) -> Result<Option<Shown>, String> {
    let session = browser.active_session_id()?;
    let tree = browser
        .client
        .send_command_no_params("Page.getFrameTree", Some(session))
        .await?;
    let frame = &tree["frameTree"]["frame"];
    let media_type = frame["mimeType"].as_str().unwrap_or_default();
    if !Format::is_document_media_type(media_type) {
        return Ok(None);
    }
    Ok(Some(Shown {
        frame_id: frame["id"].as_str().unwrap_or_default().to_string(),
        url: frame["url"].as_str().unwrap_or_default().to_string(),
        media_type: media_type.to_string(),
    }))
}

/// The document `shown`, fetched with the browser's session into a
/// [`Document`], and the HTTP status its fetch answered when it was a web
/// address.
pub(crate) async fn fetch(
    browser: &BrowserManager,
    shown: &Shown,
) -> Result<(Document, Option<u16>), DocumentError> {
    let session = browser.active_session_id().map_err(unavailable)?;
    let web = shown.url.starts_with("http://") || shown.url.starts_with("https://");
    let (handle, status, length) = if web {
        load(browser, session, shown).await?
    } else {
        (page_blob(browser, session).await?, None, None)
    };
    let receiver = Document::receive(length);
    let received = match receiver {
        Ok(receiver) => read_stream(browser, session, &handle, receiver).await,
        Err(error) => Err(error),
    };
    let _ = browser
        .client
        .send_command("IO.close", Some(json!({ "handle": handle })), Some(session))
        .await;
    Ok((received?, status))
}

/// `Network.loadNetworkResource` of the page's own address, from its frame,
/// with credentials and the browser's cache: the stream handle, the status,
/// and the length the server declared.
async fn load(
    browser: &BrowserManager,
    session: &str,
    shown: &Shown,
) -> Result<(String, Option<u16>, Option<u64>), DocumentError> {
    let loaded = browser
        .client
        .send_command(
            "Network.loadNetworkResource",
            Some(json!({
                "frameId": shown.frame_id,
                "url": shown.url,
                "options": { "disableCache": false, "includeCredentials": true },
            })),
            Some(session),
        )
        .await
        .map_err(unavailable)?;
    let resource = &loaded["resource"];
    let status = resource["httpStatusCode"]
        .as_u64()
        .and_then(|status| u16::try_from(status).ok());
    let handle = resource["stream"].as_str().filter(|_| {
        resource["success"] == true && status.is_none_or(|status| (200..300).contains(&status))
    });
    let Some(handle) = handle else {
        let reason = resource["netErrorName"]
            .as_str()
            .map(str::to_string)
            .or_else(|| status.map(|status| format!("HTTP {status}")))
            .unwrap_or_else(|| "no answer".to_string());
        return Err(DocumentError::new(
            code::FETCH_FAILED,
            format!("The browser could not fetch the document this tab shows again ({reason}). Reload the tab, then read it again."),
        ));
    };
    let length = header(&resource["headers"], "content-length").and_then(|value| value.parse().ok());
    Ok((handle.to_string(), status, length))
}

/// The page's fetch of its own address, as a Blob the browser streams: how a
/// `blob:` or `data:` document is read.
async fn page_blob(browser: &BrowserManager, session: &str) -> Result<String, DocumentError> {
    let evaluated = browser
        .client
        .send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": "fetch(location.href).then((response) => response.blob())",
                "awaitPromise": true,
                "returnByValue": false,
            })),
            Some(session),
        )
        .await
        .map_err(unavailable)?;
    let Some(object) = evaluated["result"]["objectId"]
        .as_str()
        .filter(|_| evaluated["exceptionDetails"].is_null())
    else {
        return Err(DocumentError::new(
            code::FETCH_FAILED,
            "The page could not read the document it shows. Open the document's own link, then read it again.",
        ));
    };
    let resolved = browser
        .client
        .send_command("IO.resolveBlob", Some(json!({ "objectId": object })), Some(session))
        .await
        .map_err(unavailable)?;
    let _ = browser
        .client
        .send_command(
            "Runtime.releaseObject",
            Some(json!({ "objectId": object })),
            Some(session),
        )
        .await;
    resolved["uuid"]
        .as_str()
        .map(|uuid| format!("blob:{uuid}"))
        .ok_or_else(|| unavailable("the browser did not name the document's data".into()))
}

/// Reads the browser's stream `handle` into `receiver`, which refuses bytes
/// past the document bound, so a stream that never ends stops there.
async fn read_stream(
    browser: &BrowserManager,
    session: &str,
    handle: &str,
    mut receiver: crate::document_text::Receiver,
) -> Result<Document, DocumentError> {
    loop {
        let chunk = browser
            .client
            .send_command(
                "IO.read",
                Some(json!({ "handle": handle, "size": CHUNK })),
                Some(session),
            )
            .await
            .map_err(unavailable)?;
        let data = chunk["data"].as_str().unwrap_or_default();
        if chunk["base64Encoded"] == true {
            receiver.write(&STANDARD.decode(data).map_err(|error| unavailable(error.to_string()))?)?;
        } else {
            receiver.write(data.as_bytes())?;
        }
        if chunk["eof"] == true {
            return receiver.finish();
        }
    }
}

fn header<'a>(headers: &'a Value, name: &str) -> Option<&'a str> {
    headers
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.as_str())
}

fn unavailable(error: String) -> DocumentError {
    DocumentError::new(
        code::FETCH_FAILED,
        format!("The browser could not read the document this tab shows: {error}"),
    )
}
