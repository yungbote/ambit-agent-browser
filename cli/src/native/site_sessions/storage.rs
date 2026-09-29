//! Chrome adaptation. A hidden synthetic document imports/exports one origin
//! without running its scripts, loading remote content or altering a real tab.

use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

use super::state::{self, Cookie, SiteState};
use crate::native::cdp::client::CdpClient;

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

pub(crate) async fn import(client: &CdpClient, state: &SiteState) -> Result<(), &'static str> {
    let values = client.site_context();
    state.register(&values.values);
    let applying = async {
        for origin in &state.origins {
            let input = serde_json::to_value(origin).map_err(|_| FAILED)?;
            synthetic(client, &origin.origin, "import", &input).await?;
        }
        command(
            client,
            "Storage.setCookies",
            json!({ "cookies":state.cookies.iter().map(cookie_to_chrome).collect::<Vec<_>>() }),
            None,
        )
        .await?;
        Ok(())
    }
    .await;
    if applying.is_err() {
        // An incomplete attach is signed out, never a partial grant release.
        let _ = clear(client, state).await;
    }
    applying
}

pub(crate) async fn capture(
    client: &CdpClient,
    site: &str,
    origins: &[String],
) -> Result<SiteState, &'static str> {
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
    let mut stored = Vec::new();
    let mut omitted = Vec::new();
    for origin in origins {
        if !state::origin_in_site(origin, &boundary) {
            return Err(state::INVALID);
        }
        let result = synthetic(client, origin, "export", &json!({})).await?;
        for omission in result["omitted"].as_array().ok_or(FAILED)? {
            omitted
                .push(json!({"origin":origin,"what":omission["what"],"bytes":omission["bytes"]}));
        }
        stored.push(json!({"origin":origin,"localStorage":result["localStorage"],"indexedDB":result["indexedDB"]}));
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
        "chromeMajor":chrome_major,"cookies":cookies,"origins":stored,"omitted":omitted});
    // Register before omitting oversized storage: a captured value can also
    // appear in the page while its persistence is refused by the size cap.
    let unbounded: SiteState = serde_json::from_value(value.clone()).map_err(|_| FAILED)?;
    unbounded.register(&client.site_context().values);
    bound_storage(&mut value)?;
    let state = SiteState::read(value, site)?;
    state.register(&client.site_context().values);
    Ok(state)
}

fn bound_storage(value: &mut Value) -> Result<(), &'static str> {
    let origins = value["origins"].as_array().ok_or(FAILED)?.len();
    for index in (0..origins).rev() {
        for (field, what) in [
            ("indexedDB", "indexed_db"),
            ("localStorage", "local_storage"),
        ] {
            if serde_json::to_vec(value).map_err(|_| FAILED)?.len() <= state::MAX_BYTES {
                return Ok(());
            }
            let origin = value["origins"][index]["origin"].clone();
            let bytes = serde_json::to_vec(&value["origins"][index][field])
                .map_err(|_| FAILED)?
                .len();
            value["origins"][index][field] = json!([]);
            value["omitted"]
                .as_array_mut()
                .ok_or(FAILED)?
                .push(json!({"origin":origin,"what":what,"bytes":bytes}));
        }
    }
    (serde_json::to_vec(value).map_err(|_| FAILED)?.len() <= state::MAX_BYTES)
        .then_some(())
        .ok_or(FAILED)
}

pub(crate) async fn clear(client: &CdpClient, state: &SiteState) -> Result<(), &'static str> {
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
    for cookie in &state.cookies {
        let mut deletion = json!({"name":cookie.name,"domain":cookie.domain,"path":cookie.path});
        if let Some(partition) = &cookie.partition_key {
            deletion["partitionKey"] = json!({
            "topLevelSite":partition.top_level_site,"hasCrossSiteAncestor":partition.has_cross_site_ancestor });
        }
        command(client, "Network.deleteCookies", deletion, Some(&session))
            .await
            .map_err(|_| "The browser could not clear the site's cookies.")?;
    }
    for origin in &state.origins {
        command(
            client,
            "Storage.clearDataForOrigin",
            json!({"origin":origin.origin,"storageTypes":"all"}),
            Some(&session),
        )
        .await
        .map_err(|_| "The browser could not clear the site's origin storage.")?;
    }
    Ok(())
}

async fn synthetic(
    client: &CdpClient,
    origin: &str,
    mode: &str,
    input: &Value,
) -> Result<Value, &'static str> {
    let target = command(
        client,
        "Target.createTarget",
        json!({"url":"about:blank","background":true,"hidden":true}),
        None,
    )
    .await?;
    let target = target["targetId"].as_str().ok_or(FAILED)?.to_owned();
    let context = client.site_context();
    context.hold_target(&target);
    let outcome = tokio::time::timeout(Duration::from_secs(8), async {
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
        let expression = format!("({SCRIPT})({},{})", serde_json::to_string(mode).map_err(|_| FAILED)?, input);
        let result = command(client, "Runtime.evaluate", json!({"expression":expression,"awaitPromise":true,"returnByValue":true}), Some(session)).await?;
        client.unsubscribe_session(session);
        if result.get("exceptionDetails").is_some() { return Err(FAILED); }
        result["result"].get("value").cloned().ok_or(FAILED)
    }).await.map_err(|_| FAILED).and_then(|result| result);
    let closed = command(
        client,
        "Target.closeTarget",
        json!({"targetId":target}),
        None,
    )
    .await;
    context.release_target(&target);
    if closed.is_err() {
        return Err(FAILED);
    }
    outcome
}
