use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

use std::collections::HashMap;

use super::cdp::client::CdpClient;
use super::cdp::types::*;
use super::element::RefMap;

const ANNOTATION_OVERLAY_ID: &str = "__agent_browser_annotations__";

#[derive(Debug, Clone)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone)]
struct RawAnnotation {
    ref_id: String,
    number: u64,
    role: String,
    name: Option<String>,
    rect: Rect,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnnotationBox {
    pub x: i64,
    pub y: i64,
    pub width: i64,
    pub height: i64,
}

#[derive(Debug, Clone)]
pub struct ScreenshotAnnotation {
    pub ref_id: String,
    pub number: u64,
    pub role: String,
    pub name: Option<String>,
    pub box_: AnnotationBox,
}

#[derive(Debug, Clone)]
pub struct ScreenshotResult {
    pub path: String,
    pub base64: String,
    pub annotations: Vec<ScreenshotAnnotation>,
}

#[derive(Debug, Clone)]
pub struct ScreenshotOptions {
    pub selector: Option<String>,
    pub path: Option<String>,
    pub full_page: bool,
    pub format: String,
    pub quality: Option<i32>,
    pub annotate: bool,
    pub output_dir: Option<String>,
}

impl Default for ScreenshotOptions {
    fn default() -> Self {
        Self {
            selector: None,
            path: None,
            full_page: false,
            format: "png".to_string(),
            quality: None,
            annotate: false,
            output_dir: None,
        }
    }
}

impl Serialize for ScreenshotAnnotation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("ScreenshotAnnotation", 5)?;
        state.serialize_field("ref", &self.ref_id)?;
        state.serialize_field("number", &self.number)?;
        state.serialize_field("role", &self.role)?;
        if let Some(name) = &self.name {
            state.serialize_field("name", name)?;
        }
        state.serialize_field("box", &self.box_)?;
        state.end()
    }
}

/// Named and diff screenshots share the optional foreground picture source.
pub(crate) async fn take_for(
    state: &super::actions::DaemonState,
    options: &ScreenshotOptions,
) -> Result<ScreenshotResult, String> {
    #[cfg(target_os = "linux")]
    if let Some(picture) = super::viewport_screenshot::take(state, options).await? {
        return Ok(picture);
    }
    let browser = state.browser.as_ref().ok_or("Browser not launched")?;
    take_screenshot(
        &browser.client,
        browser.active_session_id()?,
        &state.ref_map,
        options,
        &state.iframe_sessions,
    )
    .await
}

/// Captures a screenshot via CDP and optionally overlays numbered annotations
/// that mirror the Node.js screenshot `annotate` mode.
pub async fn take_screenshot(
    client: &Arc<CdpClient>,
    session_id: &str,
    ref_map: &RefMap,
    options: &ScreenshotOptions,
    iframe_sessions: &HashMap<String, String>,
) -> Result<ScreenshotResult, String> {
    let generation = client.page_generation(session_id);
    let target_rect = if options.annotate {
        match options.selector.as_deref() {
            Some(selector) => {
                get_rect_for_selector(client, session_id, ref_map, selector, iframe_sessions)
                    .await?
            }
            None => None,
        }
    } else {
        None
    };

    let raw_annotations = if options.annotate {
        collect_annotations(client, session_id, ref_map).await?
    } else {
        Vec::new()
    };

    let overlay_items = filter_annotations(raw_annotations, target_rect.as_ref());
    let params = capture_params(client, session_id, ref_map, options, iframe_sessions).await?;
    let base64 = capture_prepared(
        client.clone(),
        session_id.into(),
        generation,
        params,
        overlay_items.clone(),
    )
    .await?;
    let annotations = if options.annotate {
        let scroll = if options.full_page {
            Some(get_scroll_offsets(client, session_id).await?)
        } else {
            None
        };
        project_annotations(&overlay_items, target_rect.as_ref(), scroll)
    } else {
        Vec::new()
    };

    let ext = if options.format == "jpeg" {
        "jpg"
    } else {
        "png"
    };
    let path = save_screenshot(
        &base64,
        options.path.as_deref(),
        ext,
        options.output_dir.as_deref(),
    )?;

    Ok(ScreenshotResult {
        path,
        base64,
        annotations,
    })
}

pub(crate) async fn capture_screenshot_base64(
    client: &Arc<CdpClient>,
    session_id: &str,
    ref_map: &RefMap,
    options: &ScreenshotOptions,
    iframe_sessions: &HashMap<String, String>,
) -> Result<String, String> {
    let generation = client.page_generation(session_id);
    let params = capture_params(client, session_id, ref_map, options, iframe_sessions).await?;
    capture_prepared(
        client.clone(),
        session_id.into(),
        generation,
        params,
        Vec::new(),
    )
    .await
}

async fn capture_params(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    options: &ScreenshotOptions,
    iframe_sessions: &HashMap<String, String>,
) -> Result<CaptureScreenshotParams, String> {
    let mut params = CaptureScreenshotParams {
        format: Some(options.format.clone()),
        quality: if options.format == "jpeg" {
            options.quality.or(Some(80))
        } else {
            None
        },
        clip: None,
        from_surface: Some(true),
        capture_beyond_viewport: Some(options.full_page),
    };

    if options.full_page {
        let metrics: Value = client
            .send_command_no_params("Page.getLayoutMetrics", Some(session_id))
            .await?;

        let content_size = metrics
            .get("contentSize")
            .or_else(|| metrics.get("cssContentSize"));
        if let Some(size) = content_size {
            let width = size.get("width").and_then(|v| v.as_f64()).unwrap_or(1280.0);
            let height = size.get("height").and_then(|v| v.as_f64()).unwrap_or(720.0);

            params.clip = Some(Viewport {
                x: 0.0,
                y: 0.0,
                width,
                height,
                scale: 1.0,
            });
        }
    } else if let Some(ref selector) = options.selector {
        if let Some(rect) =
            get_rect_for_selector(client, session_id, ref_map, selector, iframe_sessions).await?
        {
            params.clip = Some(Viewport {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                scale: 1.0,
            });
        }
    }

    Ok(params)
}

/// The temporary presentation belongs to this cleanup task, rather than
/// its caller's lifetime. A cancelled caller cannot strand the clip or
/// annotation raster; source admission stays fenced until restoration.
async fn capture_prepared(
    client: Arc<CdpClient>,
    session: String,
    generation: String,
    params: CaptureScreenshotParams,
    overlay: Vec<RawAnnotation>,
) -> Result<String, String> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = async {
            let _order = client.presentations.order.clone().lock_owned().await;
            // A queued cancelled request has changed nothing. Retain cleanup
            // only once this task owns and begins the temporary presentation.
            if reply.is_closed() {
                return Err("The screenshot request ended before capture.".into());
            }
            if client.page_generation(&session) != generation {
                return Err("The page changed before screenshot capture.".into());
            }
            let temporary = params.clip.is_some() || !overlay.is_empty();
            if temporary {
                client.presentations.begin(&session, &generation);
            }
            let injected = if overlay.is_empty() {
                Ok(())
            } else {
                inject_annotation_overlay(&client, &session, &overlay).await
            };
            let captured: Result<CaptureScreenshotResult, String> = match injected {
                Ok(()) => {
                    client
                        .send_command_typed("Page.captureScreenshot", &params, Some(&session))
                        .await
                }
                Err(error) => Err(error),
            };
            // Never clean up or paint by retargeting a successor document.
            if client.page_generation(&session) != generation {
                return Err("The page changed during screenshot capture.".into());
            }
            let removed = if overlay.is_empty() {
                Ok(())
            } else {
                remove_annotation_overlay(&client, &session).await
            };
            let painted = if temporary {
                paint_viewport_unlocked(&client, &session).await
            } else {
                Ok(())
            };
            if (temporary || captured.is_ok())
                && removed.is_ok()
                && painted.is_ok()
                && client.page_generation(&session) == generation
            {
                client.presentations.publish(&session, &generation);
            }
            let captured = captured?;
            removed?;
            painted?;
            if client.page_generation(&session) != generation {
                return Err("The page changed during screenshot capture.".into());
            }
            Ok(captured.data)
        }
        .await;
        let _ = reply.send(result);
    });
    answer
        .await
        .map_err(|error| format!("The screenshot cleanup stopped: {error}"))?
}

/// Publish the full current viewport without creating another temporary
/// clip. This is a rendering fence after capture-only presentation changes,
/// and the channel's conditional first-paint guarantee before discrete input.
pub(crate) async fn paint_viewport(
    client: &Arc<CdpClient>,
    session_id: &str,
) -> Result<(), String> {
    let _order = client.presentations.order.clone().lock_owned().await;
    let generation = client.page_generation(session_id);
    paint_viewport_unlocked(client, session_id).await?;
    if client.page_generation(session_id) == generation {
        client.presentations.publish(session_id, &generation);
    }
    Ok(())
}

async fn paint_viewport_unlocked(client: &CdpClient, session_id: &str) -> Result<(), String> {
    client
        .send_command(
            "Page.captureScreenshot",
            Some(serde_json::json!({
                "format":"jpeg", "quality":1, "fromSurface":true,
                "captureBeyondViewport":false,
            })),
            Some(session_id),
        )
        .await
        .map(|_| ())
}

async fn collect_annotations(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
) -> Result<Vec<RawAnnotation>, String> {
    let entries = ref_map.entries_sorted();
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    // Collect entries that have backend_node_ids for batch resolution.
    let with_backend_ids: Vec<(String, super::element::RefEntry, i64)> = entries
        .iter()
        .filter_map(|(ref_id, entry)| {
            entry
                .backend_node_id
                .map(|bid| (ref_id.clone(), entry.clone(), bid))
        })
        .collect();

    if with_backend_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Batch-resolve all backend_node_ids to object IDs using concurrent CDP calls.
    let resolve_futures: Vec<_> = with_backend_ids
        .iter()
        .map(|(_, _, backend_node_id)| {
            client.send_command(
                "DOM.resolveNode",
                Some(serde_json::json!({
                    "backendNodeId": backend_node_id,
                    "objectGroup": "agent-browser-annotate"
                })),
                Some(session_id),
            )
        })
        .collect();

    let resolve_results = futures_util::future::join_all(resolve_futures).await;

    // Collect resolved object IDs paired with their ref info.
    let mut resolved: Vec<(String, super::element::RefEntry, String)> = Vec::new();
    for (i, result) in resolve_results.into_iter().enumerate() {
        if let Ok(val) = result {
            if let Some(oid) = val
                .get("object")
                .and_then(|o| o.get("objectId"))
                .and_then(|v| v.as_str())
            {
                let (ref_id, entry, _) = &with_backend_ids[i];
                resolved.push((ref_id.clone(), entry.clone(), oid.to_string()));
            }
        }
    }

    if resolved.is_empty() {
        return Ok(Vec::new());
    }

    // Batch-get bounding rects for all resolved elements using concurrent CDP calls.
    let rect_futures: Vec<_> = resolved
        .iter()
        .map(|(_, _, object_id)| get_rect_for_object(client, session_id, object_id))
        .collect();

    let rect_results = futures_util::future::join_all(rect_futures).await;

    let mut annotations = Vec::new();
    for (i, rect_result) in rect_results.into_iter().enumerate() {
        let rect = match rect_result {
            Ok(Some(r)) if r.width > 0.0 && r.height > 0.0 => r,
            _ => continue,
        };

        let (ref_id, entry, _) = &resolved[i];
        let number = ref_id
            .strip_prefix('e')
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);

        annotations.push(RawAnnotation {
            ref_id: ref_id.clone(),
            number,
            role: entry.role.clone(),
            name: (!entry.name.is_empty()).then_some(entry.name.clone()),
            rect,
        });
    }

    Ok(annotations)
}

async fn get_rect_for_selector(
    client: &CdpClient,
    session_id: &str,
    ref_map: &RefMap,
    selector: &str,
    iframe_sessions: &HashMap<String, String>,
) -> Result<Option<Rect>, String> {
    let (object_id, effective_session_id) = super::element::resolve_element_object_id(
        client,
        session_id,
        ref_map,
        selector,
        iframe_sessions,
    )
    .await?;
    get_rect_for_object(client, &effective_session_id, &object_id).await
}

async fn get_rect_for_object(
    client: &CdpClient,
    session_id: &str,
    object_id: &str,
) -> Result<Option<Rect>, String> {
    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.callFunctionOn",
            &CallFunctionOnParams {
                function_declaration: r#"function() {
                    const rect = this.getBoundingClientRect();
                    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
                }"#
                .to_string(),
                object_id: Some(object_id.to_string()),
                arguments: None,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(session_id),
        )
        .await?;

    Ok(result.result.value.as_ref().and_then(parse_rect))
}

fn parse_rect(value: &Value) -> Option<Rect> {
    Some(Rect {
        x: value.get("x")?.as_f64()?,
        y: value.get("y")?.as_f64()?,
        width: value.get("width")?.as_f64()?,
        height: value.get("height")?.as_f64()?,
    })
}

fn filter_annotations(
    annotations: Vec<RawAnnotation>,
    target_rect: Option<&Rect>,
) -> Vec<RawAnnotation> {
    let mut items = annotations
        .into_iter()
        .filter(|annotation| match target_rect {
            Some(target) => overlaps(&annotation.rect, target),
            None => true,
        })
        .collect::<Vec<_>>();

    items.sort_by_key(|annotation| annotation.number);
    items
}

fn overlaps(left: &Rect, right: &Rect) -> bool {
    let left_x2 = left.x + left.width;
    let left_y2 = left.y + left.height;
    let right_x2 = right.x + right.width;
    let right_y2 = right.y + right.height;

    left.x < right_x2 && left_x2 > right.x && left.y < right_y2 && left_y2 > right.y
}

async fn inject_annotation_overlay(
    client: &CdpClient,
    session_id: &str,
    annotations: &[RawAnnotation],
) -> Result<(), String> {
    let overlay_data = annotations
        .iter()
        .map(|annotation| {
            serde_json::json!({
                "number": annotation.number,
                "x": round(annotation.rect.x),
                "y": round(annotation.rect.y),
                "width": round(annotation.rect.width),
                "height": round(annotation.rect.height),
            })
        })
        .collect::<Vec<_>>();

    let expression = format!(
        r#"(() => {{
            var items = {items};
            var id = {overlay_id};
            var existing = document.getElementById(id);
            if (existing) existing.remove();
            var sx = window.scrollX || 0;
            var sy = window.scrollY || 0;
            var c = document.createElement('div');
            c.id = id;
            c.style.cssText = 'position:absolute;top:0;left:0;width:0;height:0;pointer-events:none;z-index:2147483647;';
            for (var i = 0; i < items.length; i++) {{
                var it = items[i];
                var dx = it.x + sx;
                var dy = it.y + sy;
                var b = document.createElement('div');
                b.style.cssText = 'position:absolute;left:' + dx + 'px;top:' + dy + 'px;width:' + it.width + 'px;height:' + it.height + 'px;border:2px solid rgba(255,0,0,0.8);box-sizing:border-box;pointer-events:none;';
                var l = document.createElement('div');
                l.textContent = String(it.number);
                var labelTop = dy < 14 ? '2px' : '-14px';
                l.style.cssText = 'position:absolute;top:' + labelTop + ';left:-2px;background:rgba(255,0,0,0.9);color:#fff;font:bold 11px/14px monospace;padding:0 4px;border-radius:2px;white-space:nowrap;';
                b.appendChild(l);
                c.appendChild(b);
            }}
            document.documentElement.appendChild(c);
            return true;
        }})()"#,
        items = serde_json::to_string(&overlay_data).unwrap_or_else(|_| "[]".to_string()),
        overlay_id =
            serde_json::to_string(ANNOTATION_OVERLAY_ID).unwrap_or_else(|_| "\"\"".to_string()),
    );

    let _: EvaluateResult = client
        .send_command_typed(
            "Runtime.evaluate",
            &EvaluateParams {
                expression,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(session_id),
        )
        .await?;

    Ok(())
}

async fn remove_annotation_overlay(client: &CdpClient, session_id: &str) -> Result<(), String> {
    let expression = format!(
        r#"(() => {{
            var el = document.getElementById({overlay_id});
            if (el) el.remove();
            return true;
        }})()"#,
        overlay_id =
            serde_json::to_string(ANNOTATION_OVERLAY_ID).unwrap_or_else(|_| "\"\"".to_string()),
    );

    let _: EvaluateResult = client
        .send_command_typed(
            "Runtime.evaluate",
            &EvaluateParams {
                expression,
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(session_id),
        )
        .await?;

    Ok(())
}

async fn get_scroll_offsets(client: &CdpClient, session_id: &str) -> Result<(f64, f64), String> {
    let result: EvaluateResult = client
        .send_command_typed(
            "Runtime.evaluate",
            &EvaluateParams {
                expression: "({x: window.scrollX || 0, y: window.scrollY || 0})".to_string(),
                return_by_value: Some(true),
                await_promise: Some(false),
            },
            Some(session_id),
        )
        .await?;

    let value = result.result.value.unwrap_or(Value::Null);
    let x = value.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let y = value.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
    Ok((x, y))
}

fn project_annotations(
    annotations: &[RawAnnotation],
    target_rect: Option<&Rect>,
    scroll: Option<(f64, f64)>,
) -> Vec<ScreenshotAnnotation> {
    annotations
        .iter()
        .map(|annotation| {
            let rect = if let Some(target) = target_rect {
                Rect {
                    x: annotation.rect.x - target.x,
                    y: annotation.rect.y - target.y,
                    width: annotation.rect.width,
                    height: annotation.rect.height,
                }
            } else if let Some((scroll_x, scroll_y)) = scroll {
                Rect {
                    x: annotation.rect.x + scroll_x,
                    y: annotation.rect.y + scroll_y,
                    width: annotation.rect.width,
                    height: annotation.rect.height,
                }
            } else {
                annotation.rect.clone()
            };

            ScreenshotAnnotation {
                ref_id: annotation.ref_id.clone(),
                number: annotation.number,
                role: annotation.role.clone(),
                name: annotation.name.clone(),
                box_: AnnotationBox {
                    x: round(rect.x),
                    y: round(rect.y),
                    width: round(rect.width),
                    height: round(rect.height),
                },
            }
        })
        .collect()
}

pub(crate) fn save_screenshot(
    base64_data: &str,
    explicit_path: Option<&str>,
    ext: &str,
    output_dir: Option<&str>,
) -> Result<String, String> {
    let save_path = match explicit_path {
        Some(path) => super::output_file::prepare(path)?
            .to_string_lossy()
            .to_string(),
        None => {
            let dir = match output_dir {
                Some(d) => PathBuf::from(d),
                None => get_screenshot_dir(),
            };
            let _ = std::fs::create_dir_all(&dir);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let name = format!("screenshot-{}.{}", timestamp, ext);
            dir.join(name).to_string_lossy().to_string()
        }
    };

    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64_data)
        .map_err(|e| format!("Failed to decode screenshot: {}", e))?;

    std::fs::write(&save_path, &bytes)
        .map_err(|e| format!("Failed to save screenshot to {}: {}", save_path, e))?;

    Ok(save_path)
}

fn round(value: f64) -> i64 {
    value.round() as i64
}

fn get_screenshot_dir() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".agent-browser").join("tmp").join("screenshots")
    } else {
        std::env::temp_dir()
            .join("agent-browser")
            .join("screenshots")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};

    async fn controlled_peer() -> (
        Arc<CdpClient>,
        tokio::sync::mpsc::Receiver<(Value, tokio::sync::oneshot::Sender<Value>)>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (send, receive) = tokio::sync::mpsc::channel(8);
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = ws.next().await {
                let Ok(text) = message.to_text() else {
                    continue;
                };
                let Ok(command) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                let (ack, answer) = tokio::sync::oneshot::channel::<Value>();
                if send.send((command.clone(), ack)).await.is_err() {
                    break;
                }
                let Ok(mut answer) = answer.await else { break };
                answer["id"] = command["id"].clone();
                if ws
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        answer.to_string(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let client = Arc::new(
            CdpClient::connect(&format!("ws://{address}"))
                .await
                .unwrap(),
        );
        (client, receive, peer)
    }

    fn clipped_params() -> CaptureScreenshotParams {
        CaptureScreenshotParams {
            format: Some("png".into()),
            quality: None,
            clip: Some(Viewport {
                x: 10.,
                y: 10.,
                width: 20.,
                height: 20.,
                scale: 1.,
            }),
            from_surface: Some(true),
            capture_beyond_viewport: Some(false),
        }
    }

    #[tokio::test]
    async fn cancelled_queued_capture_starts_no_temporary_presentation() {
        let (client, mut commands, peer) = controlled_peer().await;
        let generation = client.page_generation("page");
        let order = client.presentations.order.lock().await;
        {
            let pending = capture_prepared(
                client.clone(),
                "page".into(),
                generation.clone(),
                clipped_params(),
                Vec::new(),
            );
            tokio::pin!(pending);
            assert!(futures_util::poll!(pending.as_mut()).is_pending());
        } // Cancels the request before the presentation lock is available.
        drop(order);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), commands.recv())
                .await
                .is_err()
        );
        assert_eq!(client.presentations.observe("page", &generation), Some(0));
        peer.abort();
        client.closed().await;
    }

    #[tokio::test]
    async fn cancelling_a_clip_keeps_restoration_owned_and_pixels_fenced_until_ack() {
        let (client, mut commands, peer) = controlled_peer().await;
        let generation = client.page_generation("page");
        let owner = client.clone();
        let original = generation.clone();
        let caller = tokio::spawn(async move {
            capture_prepared(owner, "page".into(), original, clipped_params(), Vec::new()).await
        });
        let (clip, ack) = commands.recv().await.unwrap();
        assert!(clip["params"].get("clip").is_some());
        assert_eq!(client.presentations.observe("page", &generation), None);
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        ack.send(serde_json::json!({"result":{"data":"clip"}}))
            .unwrap();
        let (restore, ack) =
            tokio::time::timeout(std::time::Duration::from_secs(1), commands.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(restore["method"], "Page.captureScreenshot");
        assert!(restore["params"].get("clip").is_none());
        assert_eq!(restore["params"]["captureBeyondViewport"], false);
        assert_eq!(client.presentations.observe("page", &generation), None);
        ack.send(serde_json::json!({"result":{"data":"viewport"}}))
            .unwrap();
        let _finished = client.presentations.order.lock().await;
        assert_eq!(client.presentations.observe("page", &generation), Some(1));
        peer.abort();
        client.closed().await;
    }

    #[tokio::test]
    async fn failed_restoration_cannot_admit_native_pixels() {
        let (client, mut commands, peer) = controlled_peer().await;
        let generation = client.page_generation("page");
        let owner = client.clone();
        let original = generation.clone();
        let caller = tokio::spawn(async move {
            capture_prepared(owner, "page".into(), original, clipped_params(), Vec::new()).await
        });
        let (_, ack) = commands.recv().await.unwrap();
        ack.send(serde_json::json!({"result":{"data":"clip"}}))
            .unwrap();
        let (_, ack) = commands.recv().await.unwrap();
        ack.send(serde_json::json!({"error":{"code":-1,"message":"restored paint unavailable"}}))
            .unwrap();
        assert!(caller
            .await
            .unwrap()
            .unwrap_err()
            .contains("restored paint unavailable"));
        assert_eq!(client.presentations.observe("page", &generation), None);
        // Failed fallback capture cannot publish the stranded presentation.
        let owner = client.clone();
        let original = generation.clone();
        let mut params = clipped_params();
        params.clip = None;
        let fallback = tokio::spawn(async move {
            capture_prepared(owner, "page".into(), original, params, Vec::new()).await
        });
        let (_, ack) = commands.recv().await.unwrap();
        ack.send(serde_json::json!({"error":{"code":-1,"message":"fallback also unavailable"}}))
            .unwrap();
        assert!(fallback.await.unwrap().is_err());
        assert_eq!(client.presentations.observe("page", &generation), None);
        peer.abort();
        client.closed().await;
    }

    #[tokio::test]
    async fn original_capture_cleanup_never_paints_a_successor_document() {
        let (client, mut commands, peer) = controlled_peer().await;
        let generation = client.page_generation("page");
        let owner = client.clone();
        let original = generation.clone();
        let caller = tokio::spawn(async move {
            capture_prepared(owner, "page".into(), original, clipped_params(), Vec::new()).await
        });
        let (_, ack) = commands.recv().await.unwrap();
        client.rotate_page_generation("page");
        ack.send(serde_json::json!({"result":{"data":"clip"}}))
            .unwrap();
        assert!(caller.await.unwrap().unwrap_err().contains("page changed"));
        assert!(commands.try_recv().is_err());
        assert_eq!(client.presentations.observe("page", &generation), None);
        assert_eq!(
            client
                .presentations
                .observe("page", &client.page_generation("page")),
            Some(0)
        );
        peer.abort();
        client.closed().await;
    }

    #[tokio::test]
    async fn cancelling_annotation_capture_still_removes_overlay_and_publishes_viewport() {
        let (client, mut commands, peer) = controlled_peer().await;
        let generation = client.page_generation("page");
        let owner = client.clone();
        let original = generation.clone();
        let overlay = vec![RawAnnotation {
            ref_id: "e1".into(),
            number: 1,
            role: "button".into(),
            name: None,
            rect: Rect {
                x: 10.,
                y: 10.,
                width: 20.,
                height: 20.,
            },
        }];
        let caller = tokio::spawn(async move {
            capture_prepared(owner, "page".into(), original, clipped_params(), overlay).await
        });
        let (inject, ack) = commands.recv().await.unwrap();
        assert_eq!(inject["method"], "Runtime.evaluate");
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        ack.send(serde_json::json!({"result":{"result":{"type":"boolean","value":true}}}))
            .unwrap();
        let (_, ack) = commands.recv().await.unwrap();
        ack.send(serde_json::json!({"result":{"data":"clip"}}))
            .unwrap();
        let (remove, ack) = commands.recv().await.unwrap();
        assert!(remove["params"]["expression"]
            .as_str()
            .unwrap()
            .contains("var el"));
        ack.send(serde_json::json!({"result":{"result":{"type":"boolean","value":true}}}))
            .unwrap();
        let (restore, ack) = commands.recv().await.unwrap();
        assert!(restore["params"].get("clip").is_none());
        ack.send(serde_json::json!({"result":{"data":"viewport"}}))
            .unwrap();
        let _finished = client.presentations.order.lock().await;
        assert_eq!(client.presentations.observe("page", &generation), Some(1));
        peer.abort();
        client.closed().await;
    }

    #[test]
    fn filters_annotations_to_target_overlap() {
        let annotations = vec![
            RawAnnotation {
                ref_id: "e1".to_string(),
                number: 1,
                role: "button".to_string(),
                name: Some("Inside".to_string()),
                rect: Rect {
                    x: 10.0,
                    y: 10.0,
                    width: 50.0,
                    height: 20.0,
                },
            },
            RawAnnotation {
                ref_id: "e2".to_string(),
                number: 2,
                role: "button".to_string(),
                name: Some("Outside".to_string()),
                rect: Rect {
                    x: 200.0,
                    y: 200.0,
                    width: 40.0,
                    height: 20.0,
                },
            },
        ];

        let target = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };

        let filtered = filter_annotations(annotations, Some(&target));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].ref_id, "e1");
    }

    #[test]
    fn projects_selector_annotations_relative_to_target() {
        let annotations = vec![RawAnnotation {
            ref_id: "e1".to_string(),
            number: 1,
            role: "button".to_string(),
            name: Some("Inside".to_string()),
            rect: Rect {
                x: 25.0,
                y: 35.0,
                width: 40.0,
                height: 20.0,
            },
        }];

        let target = Rect {
            x: 10.0,
            y: 15.0,
            width: 100.0,
            height: 100.0,
        };

        let projected = project_annotations(&annotations, Some(&target), None);
        assert_eq!(projected[0].box_.x, 15);
        assert_eq!(projected[0].box_.y, 20);
    }

    #[test]
    fn projects_full_page_annotations_to_document_space() {
        let annotations = vec![RawAnnotation {
            ref_id: "e1".to_string(),
            number: 1,
            role: "button".to_string(),
            name: Some("Bottom".to_string()),
            rect: Rect {
                x: 5.0,
                y: 12.0,
                width: 40.0,
                height: 20.0,
            },
        }];

        let projected = project_annotations(&annotations, None, Some((10.0, 1000.0)));
        assert_eq!(projected[0].box_.x, 15);
        assert_eq!(projected[0].box_.y, 1012);
    }
}
