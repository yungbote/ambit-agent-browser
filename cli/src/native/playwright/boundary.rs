//! The DevTools boundary between a model-written program and the person's
//! browser (`transport::serve`). The person's sign-ins never cross it: no
//! cookie value and no credential header value reaches the program, and the
//! program cannot set or clear cookies, read or change site storage, supply
//! a password, or open DevTools past the boundary. The domains a program may
//! use are a closed list, read from the protocol the image's Chrome speaks
//! and the calls its Playwright client makes (the tests hold both): a domain
//! outside it is refused.
//!
//! Page script is outside the boundary: `page.evaluate` reads what the
//! page's own script can, `document.cookie` without its HttpOnly cookies
//! included.

use serde_json::{Map, Value};

/// What a cookie's or a credential header's value reads as past the
/// boundary.
pub(super) const REDACTED: &str = "[redacted credential]";

/// Headers whose value the browser attaches from what it holds: a cookie,
/// an HTTP or a proxy authentication. A page's own script sets the others,
/// and reads them anyway.
const CREDENTIAL_HEADERS: [&str; 4] = [
    "cookie",
    "set-cookie",
    "authorization",
    "proxy-authorization",
];

/// The domains whose answers and events carry cookies or credential
/// headers, so they pass `scrub`.
const CARRIERS: [&str; 4] = ["Network", "Fetch", "Storage", "Audits"];

/// The domains a program may use: every domain of the image's protocol
/// that holds none of the person's sign-ins, and the carriers, whose
/// credential methods are refused one by one.
const OPEN: [&str; 49] = [
    "Accessibility",
    "Ads",
    "Animation",
    "Audits",
    "BackgroundService",
    "BluetoothEmulation",
    "Browser",
    "CSS",
    "Cast",
    "Console",
    "CrashReportContext",
    "DOM",
    "DOMDebugger",
    "DOMSnapshot",
    "Debugger",
    "DeviceAccess",
    "DeviceOrientation",
    "Emulation",
    "EventBreakpoints",
    "FedCm",
    "Fetch",
    "HeadlessExperimental",
    "HeapProfiler",
    "IO",
    "Input",
    "Inspector",
    "LayerTree",
    "Log",
    "Media",
    "Memory",
    "Network",
    "Overlay",
    "PWA",
    "Page",
    "Performance",
    "PerformanceTimeline",
    "Preload",
    "Profiler",
    "Runtime",
    "Schema",
    "Security",
    "ServiceWorker",
    "SmartCardEmulation",
    "Storage",
    "SystemInfo",
    "Target",
    "WebAudio",
    "WebAuthn",
    "WebMCP",
];

const SITE_DATA: &str = "this browser keeps the person's sign-ins and site data, so a program never sets or clears cookies or reads or changes site storage (it lists cookies without their values)";
const PERSONS: &str =
    "a program never supplies a password or fills a payment field; the person does";
const PAST_BOUNDARY: &str =
    "it would open DevTools past the boundary between a program and the person's browser";
const CAPTURE: &str = "a trace records request headers and cookies";
const EXTENSIONS: &str = "this browser runs no extensions";
const UNLISTED: &str = "its domain is not one a program may use in this browser";

/// Why the boundary refuses a program's `method` with `params`; `None`
/// admits it.
pub(super) fn refusal(method: &str, params: &Value) -> Option<String> {
    let (domain, name) = method.split_once('.').unwrap_or((method, ""));
    let why = refused_domain(domain).or_else(|| refused_method(domain, name, params))?;
    Some(format!("{method} is refused: {why}."))
}

/// Why a whole domain is refused; `None` for a domain of `OPEN`, whose
/// methods are judged one by one.
fn refused_domain(domain: &str) -> Option<&'static str> {
    match domain {
        "DOMStorage" | "IndexedDB" | "CacheStorage" | "FileSystem" => Some(SITE_DATA),
        "Autofill" => Some(PERSONS),
        "Tethering" => Some(PAST_BOUNDARY),
        "Tracing" => Some(CAPTURE),
        "Extensions" => Some(EXTENSIONS),
        domain if OPEN.contains(&domain) => None,
        _ => Some(UNLISTED),
    }
}

/// Why a method of an open domain is refused, by what it would do: write
/// the person's cookies or site storage, supply a password, or open
/// DevTools past the boundary.
fn refused_method(domain: &str, name: &str, params: &Value) -> Option<&'static str> {
    Some(match (domain, name) {
        ("Storage", name) if name != "getCookies" => SITE_DATA,
        (
            "Network",
            "setCookie"
            | "setCookies"
            | "deleteCookies"
            | "clearBrowserCookies"
            | "setCookieControls"
            | "deleteDeviceBoundSession",
        )
        | ("Page", "deleteCookie") => SITE_DATA,
        ("Fetch", "continueWithAuth") | ("Network", "continueInterceptedRequest")
            if params["authChallengeResponse"]["response"] == "ProvideCredentials" =>
        {
            PERSONS
        }
        ("Target", "exposeDevToolsProtocol" | "sendMessageToTarget" | "openDevTools") => {
            PAST_BOUNDARY
        }
        ("Target", "createTarget") | ("Page", "navigate") if browser_ui(&params["url"]) => {
            PAST_BOUNDARY
        }
        _ => return None,
    })
}

/// Whether `url` is the browser's own interface (DevTools, its settings and
/// internal pages), which can show what the boundary keeps from programs.
fn browser_ui(url: &Value) -> bool {
    let mut url = url.as_str().unwrap_or_default().trim_start();
    loop {
        let Some((scheme, rest)) = url.split_once(':') else {
            return false;
        };
        match scheme.to_ascii_lowercase().as_str() {
            "devtools" | "chrome" | "chrome-untrusted" => return true,
            "view-source" => url = rest.trim_start(),
            _ => return false,
        }
    }
}

/// Whether a message of `method` may carry the person's cookies or
/// credential headers, so it passes `scrub` on its way to the program.
pub(super) fn carries_credentials(method: &str) -> bool {
    method
        .split_once('.')
        .is_some_and(|(domain, _)| CARRIERS.contains(&domain))
}

/// Replaces, anywhere in `value`, every cookie's value and every credential
/// header's value with `REDACTED`: in header maps, header entries, cookies,
/// raw cookie lines and raw header text. Answers whether anything changed.
pub(super) fn scrub(value: &mut Value) -> bool {
    match value {
        Value::Array(items) => items.iter_mut().map(scrub).fold(false, |a, b| a | b),
        Value::Object(object) => {
            let holds = holds_credential_value(object);
            object
                .iter_mut()
                .map(|(key, item)| match item {
                    Value::String(text)
                        if credential_header(key)
                            || key == "cookieLine"
                            || key == "rawCookieLine"
                            || (holds && key == "value") =>
                    {
                        redact(text)
                    }
                    Value::String(text) if key == "headersText" || key == "requestHeadersText" => {
                        redact_header_lines(text)
                    }
                    _ => scrub(item),
                })
                .fold(false, |a, b| a | b)
        }
        _ => false,
    }
}

/// A header entry that names a credential header (`{name: "Cookie",
/// value}`), or a cookie: a name and a value beside its domain or path.
fn holds_credential_value(object: &Map<String, Value>) -> bool {
    let Some(name) = object.get("name").and_then(Value::as_str) else {
        return false;
    };
    object.get("value").is_some_and(Value::is_string)
        && (credential_header(name) || object.contains_key("domain") || object.contains_key("path"))
}

fn credential_header(name: &str) -> bool {
    CREDENTIAL_HEADERS
        .iter()
        .any(|header| header.eq_ignore_ascii_case(name.trim()))
}

fn redact(text: &mut String) -> bool {
    let changed = text != REDACTED;
    if changed {
        *text = REDACTED.to_owned();
    }
    changed
}

/// Raw header text, one `Name: value` per line: a credential header's value
/// is redacted, every other line kept as it is.
fn redact_header_lines(text: &mut String) -> bool {
    let redacted: String = text
        .split_inclusive('\n')
        .map(|line| match line.split_once(':') {
            Some((name, _)) if credential_header(name) => {
                let ending = &line[line.trim_end_matches(['\r', '\n']).len()..];
                format!("{name}: {REDACTED}{ending}")
            }
            _ => line.to_owned(),
        })
        .collect();
    let changed = redacted != *text;
    *text = redacted;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every domain of the protocol the image's Chrome speaks (Chromium
    /// r1234, the revision playwright-core 1.62.1 pins: its
    /// `server/chromium/protocol.d.ts`).
    const PROTOCOL_DOMAINS: [&str; 57] = [
        "Accessibility",
        "Ads",
        "Animation",
        "Audits",
        "Autofill",
        "BackgroundService",
        "BluetoothEmulation",
        "Browser",
        "CSS",
        "CacheStorage",
        "Cast",
        "CrashReportContext",
        "DOM",
        "DOMDebugger",
        "DOMSnapshot",
        "DOMStorage",
        "DeviceAccess",
        "DeviceOrientation",
        "Emulation",
        "EventBreakpoints",
        "Extensions",
        "FedCm",
        "Fetch",
        "FileSystem",
        "HeadlessExperimental",
        "IO",
        "IndexedDB",
        "Input",
        "Inspector",
        "LayerTree",
        "Log",
        "Media",
        "Memory",
        "Network",
        "Overlay",
        "PWA",
        "Page",
        "Performance",
        "PerformanceTimeline",
        "Preload",
        "Security",
        "ServiceWorker",
        "SmartCardEmulation",
        "Storage",
        "SystemInfo",
        "Target",
        "Tethering",
        "Tracing",
        "WebAudio",
        "WebAuthn",
        "WebMCP",
        "Console",
        "Debugger",
        "HeapProfiler",
        "Profiler",
        "Runtime",
        "Schema",
    ];

    /// Every command playwright-core 1.62.1's Chromium client sends
    /// (`server/chromium/*.ts`), by domain.
    const PLAYWRIGHT: [(&str, &[&str]); 18] = [
        (
            "Browser",
            &[
                "cancelDownload",
                "close",
                "getVersion",
                "getWindowBounds",
                "getWindowForTarget",
                "grantPermissions",
                "resetPermissions",
                "setDockTile",
                "setDownloadBehavior",
                "setWindowBounds",
            ],
        ),
        (
            "CSS",
            &[
                "disable",
                "enable",
                "getStyleSheetText",
                "startRuleUsageTracking",
                "stopRuleUsageTracking",
            ],
        ),
        (
            "DOM",
            &[
                "describeNode",
                "disable",
                "enable",
                "getBoxModel",
                "getContentQuads",
                "getFrameOwner",
                "resolveNode",
                "scrollIntoViewIfNeeded",
                "setFileInputFiles",
            ],
        ),
        (
            "Debugger",
            &[
                "disable",
                "enable",
                "getScriptSource",
                "resume",
                "setSkipAllPauses",
            ],
        ),
        (
            "Emulation",
            &[
                "setDefaultBackgroundColorOverride",
                "setDeviceMetricsOverride",
                "setEmulatedMedia",
                "setFocusEmulationEnabled",
                "setGeolocationOverride",
                "setLocaleOverride",
                "setScriptExecutionDisabled",
                "setTimezoneOverride",
                "setTouchEmulationEnabled",
                "setUserAgentOverride",
            ],
        ),
        (
            "Fetch",
            &[
                "continueRequest",
                "continueWithAuth",
                "disable",
                "enable",
                "failRequest",
                "fulfillRequest",
            ],
        ),
        ("HeapProfiler", &["collectGarbage"]),
        ("IO", &["close", "read"]),
        (
            "Input",
            &[
                "dispatchDragEvent",
                "dispatchKeyEvent",
                "dispatchMouseEvent",
                "dispatchTouchEvent",
                "insertText",
                "setInterceptDrags",
            ],
        ),
        ("Log", &["enable"]),
        (
            "Network",
            &[
                "clearBrowserCache",
                "emulateNetworkConditions",
                "enable",
                "getResponseBody",
                "loadNetworkResource",
                "setCacheDisabled",
                "setExtraHTTPHeaders",
            ],
        ),
        (
            "Page",
            &[
                "addScriptToEvaluateOnNewDocument",
                "bringToFront",
                "captureScreenshot",
                "close",
                "createIsolatedWorld",
                "enable",
                "getFrameTree",
                "getLayoutMetrics",
                "getNavigationHistory",
                "handleJavaScriptDialog",
                "navigate",
                "navigateToHistoryEntry",
                "printToPDF",
                "reload",
                "removeScriptToEvaluateOnNewDocument",
                "screencastFrameAck",
                "setBypassCSP",
                "setFontFamilies",
                "setInterceptFileChooserDialog",
                "setLifecycleEventsEnabled",
                "startScreencast",
                "stopScreencast",
            ],
        ),
        (
            "Profiler",
            &[
                "disable",
                "enable",
                "startPreciseCoverage",
                "stopPreciseCoverage",
                "takePreciseCoverage",
            ],
        ),
        (
            "Runtime",
            &[
                "addBinding",
                "callFunctionOn",
                "enable",
                "evaluate",
                "getProperties",
                "releaseObject",
                "runIfWaitingForDebugger",
            ],
        ),
        ("Security", &["setIgnoreCertificateErrors"]),
        ("Storage", &["clearCookies", "getCookies", "setCookies"]),
        (
            "Target",
            &[
                "attachToBrowserTarget",
                "attachToTarget",
                "closeTarget",
                "createBrowserContext",
                "createTarget",
                "detachFromTarget",
                "disposeBrowserContext",
                "getBrowserContexts",
                "getTargetInfo",
                "setAutoAttach",
            ],
        ),
        ("Tracing", &["end", "start"]),
    ];

    /// The list is closed over the whole protocol: each of its domains is
    /// open or refused, and nothing else is open.
    #[test]
    fn every_domain_of_the_protocol_is_open_or_refused() {
        let refused: Vec<_> = PROTOCOL_DOMAINS
            .into_iter()
            .filter(|domain| refused_domain(domain).is_some())
            .collect();
        assert_eq!(
            refused,
            [
                "Autofill",
                "CacheStorage",
                "DOMStorage",
                "Extensions",
                "FileSystem",
                "IndexedDB",
                "Tethering",
                "Tracing",
            ]
        );
        assert!(OPEN.iter().all(|domain| PROTOCOL_DOMAINS.contains(domain)));
        assert_eq!(OPEN.len() + refused.len(), PROTOCOL_DOMAINS.len());
        assert!(refusal("Cookies.getAll", &json!({}))
            .unwrap()
            .ends_with("its domain is not one a program may use in this browser."));
    }

    /// Playwright keeps every call it makes, except the three that would
    /// set or clear the person's cookies or trace their requests.
    #[test]
    fn playwright_keeps_every_call_that_leaves_sign_ins_alone() {
        let mut refused = Vec::new();
        for (domain, names) in PLAYWRIGHT {
            for name in names {
                let method = format!("{domain}.{name}");
                if refusal(&method, &json!({ "url": "about:blank" })).is_some() {
                    refused.push(method);
                }
            }
        }
        assert_eq!(
            refused,
            [
                "Storage.clearCookies",
                "Storage.setCookies",
                "Tracing.end",
                "Tracing.start"
            ]
        );
    }

    /// The methods that write the person's cookies, supply a password or
    /// open DevTools past the boundary are refused by what they would do;
    /// the same methods doing nothing of the kind pass.
    #[test]
    fn credential_methods_are_refused_by_what_they_would_do() {
        let provide = json!({ "requestId": "1",
            "authChallengeResponse": { "response": "ProvideCredentials", "username": "a", "password": "b" } });
        let default =
            json!({ "requestId": "1", "authChallengeResponse": { "response": "Default" } });
        for (method, params, why) in [
            ("Network.setCookie", json!({}), SITE_DATA),
            ("Network.clearBrowserCookies", json!({}), SITE_DATA),
            ("Network.deleteDeviceBoundSession", json!({}), SITE_DATA),
            ("Page.deleteCookie", json!({}), SITE_DATA),
            ("Storage.clearDataForOrigin", json!({}), SITE_DATA),
            ("DOMStorage.getDOMStorageItems", json!({}), SITE_DATA),
            ("Fetch.continueWithAuth", provide.clone(), PERSONS),
            ("Network.continueInterceptedRequest", provide, PERSONS),
            ("Autofill.trigger", json!({}), PERSONS),
            ("Target.exposeDevToolsProtocol", json!({}), PAST_BOUNDARY),
            ("Target.sendMessageToTarget", json!({}), PAST_BOUNDARY),
            ("Target.openDevTools", json!({}), PAST_BOUNDARY),
            (
                "Target.createTarget",
                json!({ "url": "devtools://devtools/bundled/inspector.html" }),
                PAST_BOUNDARY,
            ),
            (
                "Page.navigate",
                json!({ "url": " Chrome://settings/cookies" }),
                PAST_BOUNDARY,
            ),
            (
                "Page.navigate",
                json!({ "url": "view-source:chrome://inspect" }),
                PAST_BOUNDARY,
            ),
            ("Tethering.bind", json!({ "port": 9222 }), PAST_BOUNDARY),
            ("Tracing.start", json!({}), CAPTURE),
            ("Extensions.loadUnpacked", json!({}), EXTENSIONS),
        ] {
            assert_eq!(
                refusal(method, &params),
                Some(format!("{method} is refused: {why}.")),
                "{method} {params}"
            );
        }
        for (method, params) in [
            ("Fetch.continueWithAuth", default),
            ("Storage.getCookies", json!({})),
            ("Network.getAllCookies", json!({})),
            (
                "Target.createTarget",
                json!({ "url": "https://example.test/" }),
            ),
            (
                "Page.navigate",
                json!({ "url": "view-source:https://example.test/" }),
            ),
            (
                "Runtime.evaluate",
                json!({ "expression": "document.cookie" }),
            ),
        ] {
            assert_eq!(refusal(method, &params), None, "{method} {params}");
        }
    }

    /// Cookies and credential headers are blanked wherever the carriers
    /// hold them; everything else, a cookie's name and a page's own
    /// headers included, reaches the program as the browser sent it.
    #[test]
    fn scrub_blanks_cookie_and_credential_header_values_and_nothing_else() {
        let cookie = |value: &str| {
            json!({ "name": "sid", "value": value, "domain": "shop.test", "path": "/",
                "httpOnly": true, "size": 20 })
        };
        let mut sent = json!({
            "headers": { "Cookie": "sid=s3cr3t", "User-Agent": "Chrome", "X-Api-Key": "page-set" },
            "associatedCookies": [{ "blockedReasons": [], "cookie": cookie("s3cr3t") }],
        });
        assert!(scrub(&mut sent));
        assert_eq!(
            sent,
            json!({
                "headers": { "Cookie": REDACTED, "User-Agent": "Chrome", "X-Api-Key": "page-set" },
                "associatedCookies": [{ "blockedReasons": [], "cookie": cookie(REDACTED) }],
            })
        );
        let mut received = json!({
            "headers": { "set-cookie": "sid=s3cr3t; HttpOnly", "content-type": "text/html" },
            "headersText": "HTTP/1.1 200 OK\r\nSet-Cookie: sid=s3cr3t; HttpOnly\r\nContent-Type: text/html\r\n\r\n",
            "blockedCookies": [{ "blockedReasons": ["SameSiteLax"], "cookieLine": "sid=s3cr3t; HttpOnly", "cookie": cookie("s3cr3t") }],
        });
        assert!(scrub(&mut received));
        assert_eq!(
            received,
            json!({
                "headers": { "set-cookie": REDACTED, "content-type": "text/html" },
                "headersText": format!("HTTP/1.1 200 OK\r\nSet-Cookie: {REDACTED}\r\nContent-Type: text/html\r\n\r\n"),
                "blockedCookies": [{ "blockedReasons": ["SameSiteLax"], "cookieLine": REDACTED, "cookie": cookie(REDACTED) }],
            })
        );
        let mut paused = json!({
            "request": { "url": "https://shop.test/", "headers": { "authorization": "Basic YTpi" } },
            "responseHeaders": [{ "name": "Set-Cookie", "value": "sid=s3cr3t" }, { "name": "Location", "value": "/home" }],
        });
        assert!(scrub(&mut paused));
        assert_eq!(
            paused,
            json!({
                "request": { "url": "https://shop.test/", "headers": { "authorization": REDACTED } },
                "responseHeaders": [{ "name": "Set-Cookie", "value": REDACTED }, { "name": "Location", "value": "/home" }],
            })
        );
        let mut issue = json!({ "issue": { "details": { "cookieIssueDetails": {
            "cookie": { "name": "sid", "domain": "shop.test", "path": "/" },
            "rawCookieLine": "sid=s3cr3t; SameSite=None" } } } });
        assert!(scrub(&mut issue));
        assert_eq!(
            issue["issue"]["details"]["cookieIssueDetails"],
            json!({ "cookie": { "name": "sid", "domain": "shop.test", "path": "/" }, "rawCookieLine": REDACTED })
        );
        let mut plain = json!({ "request": { "url": "https://shop.test/?q=cookie", "postData": "value=1" },
            "type": "Document", "entries": [{ "name": "q", "value": "cookie" }] });
        let before = plain.clone();
        assert!(!scrub(&mut plain));
        assert_eq!(plain, before);
        assert!(carries_credentials("Network.responseReceivedExtraInfo"));
        assert!(carries_credentials("Storage.getCookies"));
        assert!(!carries_credentials("Runtime.consoleAPICalled"));
    }
}
