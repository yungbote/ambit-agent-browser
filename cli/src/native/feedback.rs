//! Native observations captured after a host-bound command settles.
//! Capture failure is secondary: it never replaces the command's outcome.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::actions::DaemonState;
use super::screenshot::{capture_screenshot_base64, ScreenshotOptions};

pub(crate) const REQUEST_FIELD: &str = "ambitFeedback";
pub(crate) const MAX_CALL_MS: u64 = 120_000;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CAPTURE_BYTES: usize = 5 * 1024 * 1024;
const MAX_CAPTURE_PIXELS: u64 = 40_000_000;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ObservationId {
    pub target_id: String,
    pub loader_id: String,
    pub page_generation: String,
    pub geometry_sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FeedbackRequest {
    pub namespace: String,
    pub session: String,
    pub capture_directory: PathBuf,
    pub timeout_ms: u64,
    pub expected_observation: Option<ObservationId>,
    pub launch: Option<Value>,
}

impl FeedbackRequest {
    pub(crate) fn parse(value: &Value, state: &DaemonState) -> Result<Self, String> {
        let request: Self = serde_json::from_value(value.clone())
            .map_err(|_| "Invalid host browser feedback request.".to_string())?;
        if !request.capture_directory.is_absolute()
            || !(1..=MAX_CALL_MS).contains(&request.timeout_ms)
            || request.session != state.session_id
            || request.namespace != std::env::var("AGENT_BROWSER_NAMESPACE").unwrap_or_default()
        {
            return Err("Browser feedback does not match the host session or limits.".to_string());
        }
        if request.launch.as_ref().is_some_and(|launch| {
            !launch.is_object()
                || launch["action"] != "launch"
                || launch.get(REQUEST_FIELD).is_some()
        }) {
            return Err("Invalid host browser launch settings.".into());
        }
        Ok(request)
    }

    pub(crate) fn unavailable(&self, code: &str) -> Value {
        json!({ "namespace": self.namespace, "session": self.session,
            "capture": { "status": "unavailable", "code": code } })
    }
}

struct Observation {
    id: ObservationId,
    page: Value,
    css_width: f64,
    css_height: f64,
    page_scale: f64,
    device_pixel_ratio: f64,
}

fn positive(value: &Value, key: &str) -> Result<f64, &'static str> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && *number > 0.0)
        .ok_or("capture_geometry_unavailable")
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

async fn observe(state: &DaemonState) -> Result<Observation, &'static str> {
    if let Some(error) = state.window_page_error {
        return Err(error);
    }
    let browser = state.browser.as_ref().ok_or("no_active_page")?;
    let session_id = browser.active_session_id().map_err(|_| "no_active_page")?;
    let target_id = browser.active_target_id().map_err(|_| "no_active_page")?;
    let page_generation = browser.client.page_generation(session_id);
    let tree = browser
        .client
        .send_command_no_params("Page.getFrameTree", Some(session_id))
        .await
        .map_err(|_| "capture_page_unavailable")?;
    let frame = &tree["frameTree"]["frame"];
    let frame_id = frame["id"].as_str().ok_or("capture_page_unavailable")?;
    let loader_id = frame["loaderId"]
        .as_str()
        .ok_or("capture_page_unavailable")?;
    let url = frame["url"].as_str().ok_or("capture_page_unavailable")?;
    let metrics = browser
        .client
        .send_command_no_params("Page.getLayoutMetrics", Some(session_id))
        .await
        .map_err(|_| "capture_geometry_unavailable")?;
    let visual = &metrics["cssVisualViewport"];
    let layout = &metrics["cssLayoutViewport"];
    let css_width = positive(visual, "clientWidth")?;
    let css_height = positive(visual, "clientHeight")?;
    let page_scale = positive(visual, "scale")?;
    if !layout.is_object() {
        return Err("capture_geometry_unavailable");
    }

    // An isolated realm obtains native window geometry without consulting
    // getters or functions replaced by the page's JavaScript.
    let world = browser
        .client
        .send_command(
            "Page.createIsolatedWorld",
            Some(json!({
                "frameId": frame_id, "worldName": "agent-browser-observation",
            })),
            Some(session_id),
        )
        .await
        .map_err(|_| "capture_geometry_unavailable")?;
    let context = world["executionContextId"]
        .as_i64()
        .ok_or("capture_geometry_unavailable")?;
    let details = browser
        .client
        .send_command(
            "Runtime.evaluate",
            Some(json!({
                "expression": "({title:document.title,devicePixelRatio:window.devicePixelRatio})",
                "contextId": context, "returnByValue": true,
            })),
            Some(session_id),
        )
        .await
        .map_err(|_| "capture_geometry_unavailable")?;
    let details = &details["result"]["value"];
    let device_pixel_ratio = positive(details, "devicePixelRatio")?;
    let geometry = json!({ "layoutViewport": layout, "visualViewport": visual,
        "devicePixelRatio": device_pixel_ratio });
    let geometry_sha256 =
        digest(&serde_json::to_vec(&geometry).map_err(|_| "capture_geometry_unavailable")?);
    Ok(Observation {
        id: ObservationId {
            target_id: target_id.to_string(),
            loader_id: loader_id.to_string(),
            page_generation: page_generation.clone(),
            geometry_sha256,
        },
        page: json!({ "targetId": target_id, "loaderId": loader_id, "pageGeneration": page_generation, "url": url,
            "title": details["title"].as_str().unwrap_or_default() }),
        css_width,
        css_height,
        page_scale,
        device_pixel_ratio,
    })
}

pub(crate) async fn matches_expected(request: &FeedbackRequest, state: &DaemonState) -> bool {
    let Some(expected) = request.expected_observation.as_ref() else {
        return true;
    };
    matches!(tokio::time::timeout(CAPTURE_TIMEOUT, observe(state)).await,
        Ok(Ok(observation)) if observation.id == *expected)
}

fn write_capture(directory: &Path, bytes: &[u8]) -> Result<PathBuf, &'static str> {
    let directory = directory
        .canonicalize()
        .map_err(|_| "capture_directory_unavailable")?;
    if !directory.is_dir() {
        return Err("capture_directory_unavailable");
    }
    let path = directory.join(format!("{}.jpg", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&path).map_err(|_| "capture_write_failed")?;
    file.write_all(bytes).map_err(|_| "capture_write_failed")?;
    file.sync_data().map_err(|_| "capture_write_failed")?;
    Ok(path)
}

async fn capture(request: &FeedbackRequest, state: &DaemonState) -> Result<Value, &'static str> {
    let before = observe(state).await?;
    let expected_width = before.css_width * before.page_scale * before.device_pixel_ratio;
    let expected_height = before.css_height * before.page_scale * before.device_pixel_ratio;
    if !expected_width.is_finite()
        || !expected_height.is_finite()
        || expected_width * expected_height > MAX_CAPTURE_PIXELS as f64
    {
        return Err("capture_too_large");
    }
    let browser = state.browser.as_ref().ok_or("no_active_page")?;
    let session_id = browser.active_session_id().map_err(|_| "no_active_page")?;
    let options = ScreenshotOptions {
        format: "jpeg".to_string(),
        quality: Some(75),
        ..ScreenshotOptions::default()
    };
    let encoded = capture_screenshot_base64(
        &browser.client,
        session_id,
        &state.ref_map,
        &options,
        &state.iframe_sessions,
    )
    .await
    .map_err(|_| "capture_failed")?;
    if encoded.len() > MAX_CAPTURE_BYTES.div_ceil(3) * 4 {
        return Err("capture_too_large");
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "capture_invalid_image")?;
    if bytes.len() > MAX_CAPTURE_BYTES {
        return Err("capture_too_large");
    }
    let reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|_| "capture_invalid_image")?;
    if reader.format() != Some(image::ImageFormat::Jpeg) {
        return Err("capture_invalid_image");
    }
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| "capture_invalid_image")?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_CAPTURE_PIXELS {
        return Err("capture_too_large");
    }
    let after = observe(state).await?;
    if before.id != after.id {
        return Err("capture_page_changed");
    }
    let sha256 = digest(&bytes);
    let path = write_capture(&request.capture_directory, &bytes)?;
    Ok(
        json!({ "namespace": request.namespace, "session": request.session,
            "page": after.page,
            "capture": { "path": path, "sha256": sha256, "sizeBytes": bytes.len(),
                "mimeType": "image/jpeg", "width": width, "height": height,
                "coordinateSpace": { "name": "viewport-css", "cssWidth": after.css_width,
                    "cssHeight": after.css_height, "pageScaleFactor": after.page_scale,
                    "devicePixelRatio": after.device_pixel_ratio,
                    "geometrySha256": after.id.geometry_sha256,
                    "imageToViewport": { "scaleX": after.css_width / f64::from(width),
                        "scaleY": after.css_height / f64::from(height), "offsetX": 0.0, "offsetY": 0.0 }
                }
            }
        }),
    )
}

pub(crate) async fn attach(request: &FeedbackRequest, response: &mut Value, state: &DaemonState) {
    let controlled = state.browser_control.lock().await.agent_error();
    response["browser"] = if let Some(error) = controlled {
        request.unavailable(error.code)
    } else {
        match tokio::time::timeout(CAPTURE_TIMEOUT, capture(request, state)).await {
            Ok(Ok(browser)) => browser,
            Ok(Err(code)) => request.unavailable(code),
            Err(_) => request.unavailable("capture_timeout"),
        }
    };
    let capture = &response["browser"]["capture"];
    if capture.get("path").is_some() || capture["code"] == "no_active_page" {
        state.browser_control.lock().await.observed();
    }
}
