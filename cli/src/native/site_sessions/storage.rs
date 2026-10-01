//! Chrome adaptation. A hidden synthetic document imports/exports one origin
//! without running its scripts, loading remote content or altering a real tab.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use super::bytes::Bytes;
use super::state::{self, Cookie, SiteState};
use crate::native::cdp::client::CdpClient;

pub(crate) const UNCLEARED: &str = "The browser site cleanup is unsettled.";

const FAILED: &str = "The browser could not complete site state custody.";
const SCRIPT: &str = include_str!("storage.js");

async fn command(
    client: &CdpClient,
    method: &str,
    params: Value,
    session: Option<&str>,
) -> Result<Value, &'static str> {
    client
        .send_command(method, Some(params), session)
        .await
        .map_err(|_| FAILED)
}

fn title(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
        .unwrap_or_default()
}

fn cookie_to_chrome(cookie: &Cookie) -> Value {
    let mut value = json!({ "name":cookie.name, "value":cookie.value, "domain":cookie.domain,
        "path":cookie.path, "httpOnly":cookie.http_only, "secure":cookie.secure,
        "priority":title(&cookie.priority), "sourceScheme":match cookie.source_scheme.as_str() {
            "secure" => "Secure", "non_secure" => "NonSecure", _ => "Unset" } });
    if let Some(expires) = cookie.expires {
        value["expires"] = json!(expires);
    }
    if let Some(same_site) = &cookie.same_site {
        value["sameSite"] = json!(title(same_site));
    }
    if let Some(partition) = &cookie.partition_key {
        value["partitionKey"] = json!({
        "topLevelSite":partition.top_level_site, "hasCrossSiteAncestor":partition.has_cross_site_ancestor });
    }
    value
}

#[cfg(test)]
pub(crate) async fn import(client: &Arc<CdpClient>, state: &SiteState) -> Result<(), &'static str> {
    import_current(client, state, None).await
}

/// The owner awaits this to completion on cancellation: the private document
/// closes first, then partial state clears, with no applying command left alive.
pub(crate) async fn import_current(
    client: &Arc<CdpClient>,
    state: &SiteState,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<(), &'static str> {
    let values = client.site_context();
    state.register(&values.values);
    let applying = async {
        for origin in &state.origins {
            synthetic(client, &origin.origin, "import", origin, cancel.clone()).await?;
        }
        if cancel.as_ref().is_some_and(|cancel| *cancel.borrow()) {
            return Err(FAILED);
        }
        command(
            client,
            "Storage.setCookies",
            json!({ "cookies":state.cookies.iter().map(cookie_to_chrome).collect::<Vec<_>>() }),
            None,
        )
        .await?;
        if cancel.as_ref().is_some_and(|cancel| *cancel.borrow()) {
            return Err(FAILED);
        }
        Ok(())
    }
    .await;
    if applying.is_err() {
        // A refused close cannot prove the importer stopped. Keep the profile
        // and pause unsettled instead of clearing beneath a live transaction.
        if applying == Err(UNCLEARED) || clear(client, state).await.is_err() {
            client.site_profile().mark_uncleared(&state.site);
            return Err(UNCLEARED);
        }
    }
    applying
}

async fn cookies(client: &CdpClient, site: &str) -> Result<Vec<Value>, &'static str> {
    let boundary = state::site_url(site)?;
    let raw = command(client, "Storage.getCookies", json!({}), None).await?;
    let mut cookies = Vec::new();
    for cookie in raw["cookies"].as_array().ok_or(FAILED)? {
        let domain = cookie["domain"].as_str().ok_or(FAILED)?;
        let partition = cookie.get("partitionKey").filter(|key| !key.is_null());
        let partition_site =
            partition.and_then(|key| key.as_str().or_else(|| key["topLevelSite"].as_str()));
        let belongs = match partition_site {
            Some(partition) => partition == site,
            None => state::host_in_site(domain.strip_prefix('.').unwrap_or(domain), &boundary),
        };
        if !belongs {
            continue;
        }
        cookies.push(json!({ "name":cookie["name"], "value":cookie["value"], "domain":domain,
            "path":cookie["path"], "expires":if cookie["session"] == true || cookie["expires"].as_f64().is_some_and(|value| value < 0.0) { Value::Null } else { cookie["expires"].clone() },
            "httpOnly":cookie["httpOnly"], "secure":cookie["secure"], "sameSite":cookie["sameSite"].as_str().map(str::to_ascii_lowercase),
            "priority":cookie["priority"].as_str().unwrap_or("Medium").to_ascii_lowercase(),
            "sourceScheme":match cookie["sourceScheme"].as_str() { Some("Secure") => "secure", Some("NonSecure") => "non_secure", _ => "unset" },
            "partitionKey":partition_site.map(|site| json!({"topLevelSite":site,"hasCrossSiteAncestor":partition.and_then(|key| key["hasCrossSiteAncestor"].as_bool()).unwrap_or(false)})) }));
    }
    Ok(cookies)
}

pub(crate) async fn capture(
    client: &Arc<CdpClient>,
    site: &str,
    origins: &[String],
) -> Result<SiteState, &'static str> {
    capture_current(client, site, origins, None).await
}

pub(crate) async fn capture_current(
    client: &Arc<CdpClient>,
    site: &str,
    origins: &[String],
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<SiteState, &'static str> {
    let boundary = state::site_url(site)?;
    let cookies = cookies(client, site).await?;
    let mut stored = Vec::new();
    let mut omitted = Vec::new();
    for origin in origins {
        if !state::origin_in_site(origin, &boundary) {
            return Err(state::INVALID);
        }
        let mut result = synthetic(
            client,
            origin,
            "export",
            &json!({"chunkCharacters":super::bytes::CHUNK_BYTES / 4}),
            cancel.clone(),
        )
        .await?;
        for omission in result["omitted"].as_array().ok_or(FAILED)? {
            omitted
                .push(json!({"origin":origin,"what":omission["what"],"bytes":omission["bytes"]}));
        }
        stored.push(Value::Object(serde_json::Map::from_iter([
            ("origin".into(), Value::String(origin.clone())),
            ("localStorage".into(), result["localStorage"].take()),
            ("indexedDB".into(), result["indexedDB"].take()),
        ])));
    }
    let version = command(client, "Browser.getVersion", json!({}), None).await?;
    let chrome_major = version["product"]
        .as_str()
        .and_then(|product| product.split('/').nth(1))
        .and_then(|version| version.split('.').next())
        .and_then(|major| major.parse::<u64>().ok())
        .ok_or("The browser could not identify its Chrome version for site capture.")?;
    let mut value = json!({"format":state::FORMAT,"site":site,
        "capturedAt":chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "chromeMajor":chrome_major,"cookies":[],"origins":[],"omitted":[]});
    value["cookies"] = Value::Array(cookies);
    value["origins"] = Value::Array(stored);
    value["omitted"] = Value::Array(omitted);
    let state = SiteState::read_streamed(value, site)?;
    state.register(&client.site_context().values);
    Ok(state)
}

pub(crate) async fn clear(client: &Arc<CdpClient>, state: &SiteState) -> Result<(), &'static str> {
    clear_site(
        client,
        &state.site,
        &state
            .origins
            .iter()
            .map(|origin| origin.origin.clone())
            .collect::<Vec<_>>(),
    )
    .await
}

/// Deletion needs current cookie keys and origin names, not a copy of every
/// IndexedDB record. It remains bounded even when the site's cache is huge.
pub(crate) async fn clear_site(
    client: &CdpClient,
    site: &str,
    origins: &[String],
) -> Result<(), &'static str> {
    let boundary = state::site_url(site)?;
    if origins
        .iter()
        .any(|origin| !state::origin_in_site(origin, &boundary))
    {
        return Err(state::INVALID);
    }
    let current: Vec<Cookie> =
        serde_json::from_value(Value::Array(cookies(client, site).await?)).map_err(|_| FAILED)?;
    let targets = command(client, "Target.getTargets", json!({}), None).await?;
    // As BrowserManager's download context adapter already establishes,
    // TargetInfo carries a synthetic default-context id. Only contexts in
    // getBrowserContexts are isolated, so absence is not the default test.
    let contexts = command(client, "Target.getBrowserContexts", json!({}), None).await?;
    let isolated = contexts["browserContextIds"].as_array().ok_or(FAILED)?;
    let session = targets["targetInfos"]
        .as_array()
        .ok_or(FAILED)?
        .iter()
        .filter(|target| {
            target["type"] == "page"
                && !isolated
                    .iter()
                    .any(|context| context == &target["browserContextId"])
        })
        .find_map(|target| {
            target["targetId"]
                .as_str()
                .and_then(|target| client.session_for_target(target))
        })
        .ok_or("The browser has no native default-context page session for site clearing.")?;
    for cookie in &current {
        let mut deletion = json!({"name":cookie.name,"domain":cookie.domain,"path":cookie.path});
        if let Some(partition) = &cookie.partition_key {
            deletion["partitionKey"] = json!({
            "topLevelSite":partition.top_level_site,"hasCrossSiteAncestor":partition.has_cross_site_ancestor });
        }
        command(client, "Network.deleteCookies", deletion, Some(&session))
            .await
            .map_err(|_| "The browser could not clear the site's cookies.")?;
    }
    for origin in origins {
        command(
            client,
            "Storage.clearDataForOrigin",
            json!({"origin":origin,"storageTypes":"all"}),
            Some(&session),
        )
        .await
        .map_err(|_| "The browser could not clear the site's origin storage.")?;
    }
    for key in client.site_profile().documents.keys(site)? {
        command(
            client,
            "Storage.clearDataForStorageKey",
            json!({"storageKey":key,"storageTypes":"all"}),
            Some(&session),
        )
        .await
        .map_err(|_| UNCLEARED)?;
    }
    Ok(())
}

async fn synthetic<T: serde::Serialize + Sync>(
    client: &Arc<CdpClient>,
    origin: &str,
    mode: &str,
    input: &T,
    mut cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<Value, &'static str> {
    let target = command(
        client,
        "Target.createTarget",
        json!({"url":"about:blank","background":true,"hidden":true}),
        None,
    )
    .await?;
    let target = target["targetId"].as_str().ok_or(FAILED)?.to_owned();
    let mut held = SyntheticTarget::new(client.clone(), target.clone());
    let outcome = {
        let applying = async {
            let session = tokio::time::timeout(Duration::from_secs(8), async {
        let attached = command(client, "Target.attachToTarget", json!({"targetId":target,"flatten":true}), None).await?;
        let session = attached["sessionId"].as_str().ok_or(FAILED)?;
        let mut events = client.subscribe_session(session);
        command(client, "Page.enable", json!({}), Some(session)).await?;
        command(client, "Fetch.enable", json!({"patterns":[{"urlPattern":"*","resourceType":"Document","requestStage":"Request"}]}), Some(session)).await?;
        let url = format!("{origin}/.ambit-site-custody-{}", uuid::Uuid::new_v4().simple());
        let navigation = command(client, "Page.navigate", json!({"url":url}), Some(session));
        tokio::pin!(navigation);
        let mut navigated = false;
        let mut loaded = false;
        loop {
            if navigated && loaded { break; }
            tokio::select! {
                result = &mut navigation, if !navigated => { result?; navigated = true; }
                event = events.recv() => {
                    let event = event.ok_or(FAILED)?;
                    if event.method == "Fetch.requestPaused" {
                        command(client, "Fetch.fulfillRequest", json!({"requestId":event.params["requestId"],
                            "responseCode":200,"responseHeaders":[{"name":"Content-Type","value":"text/html; charset=utf-8"},
                                {"name":"Cache-Control","value":"no-store"}],
                            "body":STANDARD.encode("<!doctype html><title>Browser site custody</title>")}), Some(session)).await?;
                    }
                    if event.method == "Page.loadEventFired" { loaded = true; }
                }
            }
        }
        command(client, "Fetch.disable", json!({}), Some(session)).await?;
        Ok::<_, &'static str>(session.to_owned())
        }).await.map_err(|_| FAILED)??;
            client
                .site_profile()
                .documents
                .seed(client, &session)
                .await?;
            let expression = if mode == "export" {
                format!(
                    "({SCRIPT})(\"export\",{})",
                    serde_json::to_string(input).map_err(|_| FAILED)?
                )
            } else {
                "({decoder:new TextDecoder('utf-8',{fatal:true}),parts:[]})".into()
            };
            let result = tokio::time::timeout(
                Duration::from_secs(8),
                command(
                    client,
                    "Runtime.evaluate",
                    json!({"expression":expression,"awaitPromise":true,"returnByValue":false}),
                    Some(&session),
                ),
            )
            .await
            .map_err(|_| FAILED)??;
            client.unsubscribe_session(&session);
            if result.get("exceptionDetails").is_some() {
                return Err(FAILED);
            }
            let iterator = result["result"]["objectId"].as_str().ok_or(FAILED)?;
            #[cfg(test)]
            eprintln!("private_document_stage=iterator-ready mode={mode}");
            let mut bytes = Bytes::new().map_err(|_| FAILED)?;
            if mode == "import" {
                bytes.write_json(input).map_err(|_| FAILED)?;
                let mut offset = 0;
                while offset < bytes.length() {
                    let chunk = bytes
                        .chunk(offset, super::bytes::CHUNK_BYTES)
                        .map_err(|_| FAILED)?;
                    let appended = tokio::time::timeout(Duration::from_secs(8), command(client,"Runtime.callFunctionOn",json!({
                    "objectId":iterator,"functionDeclaration":"function(data){const bytes=Uint8Array.from(atob(data),character=>character.charCodeAt(0));this.parts.push(this.decoder.decode(bytes,{stream:true}));return bytes.length}",
                    "arguments":[{"value":STANDARD.encode(&chunk)}],"returnByValue":true}),Some(&session))).await.map_err(|_| FAILED)??;
                    if appended.get("exceptionDetails").is_some()
                        || appended["result"]["value"] != chunk.len()
                    {
                        return Err(FAILED);
                    }
                    offset += chunk.len() as u64;
                }
                #[cfg(test)]
                eprintln!(
                    "private_document_stage=input-ready bytes={}",
                    bytes.length()
                );
                let applying = format!("async function(){{this.parts.push(this.decoder.decode());this.decoder=null;const input=JSON.parse(this.parts.join(''));this.parts=null;return await ({SCRIPT})(\"import\",input)}}");
                let applied = tokio::time::timeout(Duration::from_secs(8),command(client,"Runtime.callFunctionOn",json!({
                "objectId":iterator,"functionDeclaration":applying,"awaitPromise":true,"returnByValue":true}),Some(&session))).await.map_err(|_| FAILED)??;
                if applied.get("exceptionDetails").is_some() {
                    return Err(FAILED);
                }
                return applied["result"].get("value").cloned().ok_or(FAILED);
            }
            loop {
                let next = tokio::time::timeout(Duration::from_secs(8), command(client, "Runtime.callFunctionOn",
                json!({"objectId":iterator,"functionDeclaration":"async function(){return await this.next()}","awaitPromise":true,"returnByValue":true}), Some(&session)))
                .await.map_err(|_| FAILED)??;
                if next.get("exceptionDetails").is_some() {
                    return Err(FAILED);
                }
                let next = &next["result"]["value"];
                if next["done"] == true {
                    break;
                }
                let chunk = next["value"].as_str().ok_or(FAILED)?;
                if chunk.len() > super::bytes::CHUNK_BYTES {
                    return Err(FAILED);
                }
                bytes.write_all(chunk.as_bytes()).map_err(|_| FAILED)?;
            }
            serde_json::from_reader(bytes.reader().map_err(|_| FAILED)?).map_err(|_| FAILED)
        };
        tokio::pin!(applying);
        let outcome = tokio::select! {
            biased;
            _ = async { match &mut cancel {
                Some(cancel) => { while !*cancel.borrow_and_update() { if cancel.changed().await.is_err() { break; } } },
                None => std::future::pending::<()>().await,
            } } => Err(FAILED),
            result = &mut applying => result,
        };
        // Drop the applying future before close. Chrome's target closure settles
        // its script/transactions; only then can the caller clear partial writes.
        outcome
    };
    #[cfg(test)]
    eprintln!(
        "private_document_outcome={}",
        json!({"mode":mode,"complete":outcome.is_ok()})
    );
    held.close().await?;
    outcome
}

/// Cancellation closes the private document and aborts its readonly storage
/// transaction. Keep masking the target until Chrome confirms its closure.
struct SyntheticTarget {
    client: Arc<CdpClient>,
    target: Option<String>,
}

impl SyntheticTarget {
    fn new(client: Arc<CdpClient>, target: String) -> Self {
        client.site_context().hold_target(&target);
        Self {
            client,
            target: Some(target),
        }
    }

    async fn close(&mut self) -> Result<(), &'static str> {
        let target = self.target.as_ref().ok_or(UNCLEARED)?;
        close_target(&self.client, target).await?;
        self.client.site_context().release_target(target);
        self.target = None;
        Ok(())
    }
}

impl Drop for SyntheticTarget {
    fn drop(&mut self) {
        if let Some(target) = self.target.take() {
            let client = self.client.clone();
            tokio::spawn(async move {
                if close_target(&client, &target).await.is_ok() {
                    client.site_context().release_target(&target);
                }
            });
        }
    }
}

/// A close reply is not a retirement receipt. Confirm absence through the
/// same browser before allowing a partial import to clear or a pause to resume.
async fn close_target(client: &CdpClient, target: &str) -> Result<(), &'static str> {
    let mut events = client.subscribe();
    let closed = command(
        client,
        "Target.closeTarget",
        json!({"targetId":target}),
        None,
    )
    .await;
    if closed.as_ref().is_ok_and(|reply| reply["success"] == false) {
        return Err(UNCLEARED);
    }
    let absent = |targets: &Value| -> Result<bool, &'static str> {
        Ok(!targets["targetInfos"]
            .as_array()
            .ok_or(UNCLEARED)?
            .iter()
            .any(|entry| entry["targetId"] == target))
    };
    let targets = command(client, "Target.getTargets", json!({}), None)
        .await
        .map_err(|_| UNCLEARED)?;
    #[cfg(test)]
    eprintln!(
        "private_close_receipt={}",
        json!({"replySuccess":closed.as_ref().ok().map(|reply|reply["success"].clone()),"targetAbsent":absent(&targets)?})
    );
    if !absent(&targets)? {
        tokio::time::timeout(Duration::from_secs(8),async {
            loop {tokio::select! {
                event=events.recv()=>{let event=event.map_err(|_|UNCLEARED)?;if event.method=="Target.targetDestroyed" && event.params["targetId"]==target {return Ok::<_,&'static str>(());}},
                _=client.closed()=>return Err(UNCLEARED),
            }}
        }).await.map_err(|_|UNCLEARED)??;
        let targets = command(client, "Target.getTargets", json!({}), None)
            .await
            .map_err(|_| UNCLEARED)?;
        if !absent(&targets)? {
            return Err(UNCLEARED);
        }
    }
    Ok(())
}

pub(crate) async fn settle_private_targets(
    client: &CdpClient,
    context: &super::Context,
) -> Result<(), &'static str> {
    for target in context.owned_targets() {
        close_target(client, &target).await?;
        context.release_target(&target);
    }
    Ok(())
}
