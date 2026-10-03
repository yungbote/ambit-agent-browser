//! Foreground viewport image bytes from the existing picture producer.
//! Specialized captures keep the canonical CDP path. A display/document
//! change after capture refuses rather than relabelling stale pixels.

use crate::native::actions::DaemonState;
use crate::native::browser_control::ViewportPictureBinding;
use crate::native::screenshot::{ScreenshotOptions, ScreenshotResult};
use crate::native::stream::ViewportSnapshot as Snapshot;

pub(crate) const PAGE_CHANGED: &str = "The page or browser layout changed during screenshot capture. Observe the current page before taking another screenshot.";

fn matches_binding(picture: &Snapshot, binding: &ViewportPictureBinding, after_us: u64) -> bool {
    let surface = &picture.surface;
    let expected = &binding.surface;
    let crop = binding.crop;
    let shown = picture.visible;
    picture.bounds.requested_us >= after_us
        && picture.bounds.received_us >= picture.bounds.requested_us
        && picture.layout_epoch == binding.layout_epoch
        && !surface.cursor_included
        && surface.generation == expected.generation
        && surface.width == expected.width
        && surface.height == expected.height
        && surface.origin_x == expected.origin_x
        && surface.origin_y == expected.origin_y
        && surface.kind == expected.kind
        && surface.coordinate_space == expected.coordinate_space
        && surface.device_scale_factor == expected.device_scale_factor
        && crop.x >= shown.x
        && crop.y >= shown.y
        && i64::from(crop.x) + i64::from(crop.width) <= i64::from(shown.x) + i64::from(shown.width)
        && i64::from(crop.y) + i64::from(crop.height)
            <= i64::from(shown.y) + i64::from(shown.height)
}

pub(crate) async fn capture_base64(
    state: &DaemonState,
    options: &ScreenshotOptions,
) -> Result<Option<String>, String> {
    // Specialized captures preserve their renderer/crop/annotation behavior.
    // Unsupported formats/qualities retain the existing validation boundary.
    if !matches!(options.format.as_str(), "png" | "jpeg")
        || options
            .quality
            .is_some_and(|quality| !(0..=100).contains(&quality))
        || options.full_page
        || options.selector.is_some()
        || options.annotate
    {
        return Ok(None);
    };
    let Some(server) = state.stream_server.as_ref() else {
        return Ok(None);
    };
    let Some(browser) = state.browser.as_ref() else {
        return Ok(None);
    };
    let Some(display) = browser.display_client() else {
        return Ok(None);
    };
    let session = browser.active_session_id()?;
    let before = state
        .browser_control
        .lock()
        .await
        .viewport_picture_binding(&browser.client, session)
        .await?;
    let Some(before) = before else {
        return Ok(None);
    };
    // An input acknowledgement proves dispatch/consumption, not paint. The
    // next frame renders already-consumed effects; the following frame is
    // reached only after that rendering phase. Use the original isolated
    // realm, so page overrides cannot supply this readiness observation.
    // One deadline covers readiness and picture demand together.
    let ready_picture = tokio::time::timeout(std::time::Duration::from_secs(2),async {
        let ready = browser.client.send_command("Runtime.evaluate",Some(serde_json::json!({
            "expression":"new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve(true))))",
            "contextId":before.context,"returnByValue":true,"awaitPromise":true,
        })),Some(session)).await?;
        if ready.get("exceptionDetails").is_some() || ready["result"]["value"]!=true {
            return Err("The original viewport did not reach its rendering frame.".into());
        }
        // Same process/kernel media clock as native/CDP acknowledgement.
        // A picture in flight before this readiness cannot satisfy demand.
        let after_us=crate::native::stream::monotonic_us();
        let picture=server.viewport_picture(&display,after_us).await?;
        Ok::<_,String>((picture,after_us))
    }).await;
    let (picture, after_us) = match ready_picture {
        Ok(Ok(ready)) => ready,
        _ => return Ok(None), // Unavailable original frame/producer retains CDP.
    };
    let after = state
        .browser_control
        .lock()
        .await
        .viewport_picture_binding(&browser.client, session)
        .await?;
    if after.as_ref() != Some(&before) || !matches_binding(&picture, &before, after_us) {
        return Err(PAGE_CHANGED.into());
    }
    let rgba = picture.rgba(before.crop)?;
    let format = options.format.clone();
    let quality = options
        .quality
        .unwrap_or(super::screenshot::DEFAULT_JPEG_QUALITY) as u8;
    let bytes = tokio::task::spawn_blocking(move || {
        // Preserve decoded pixels while avoiding the general image writer's
        // slow adaptive filter search on viewport-sized pictures.
        use image::ImageEncoder;
        let mut bytes = Vec::new();
        if format == "jpeg" {
            let rgb = image::DynamicImage::ImageRgba8(rgba).into_rgb8();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
                .encode_image(&rgb)
                .map_err(|error| format!("The browser picture could not be encoded: {error}"))?;
        } else {
            image::codecs::png::PngEncoder::new_with_quality(
                &mut bytes,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::Up,
            )
            .write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|error| format!("The browser picture could not be encoded: {error}"))?;
        }
        Ok::<_, String>(bytes)
    })
    .await
    .map_err(|error| format!("The browser picture encoder stopped: {error}"))??;
    use base64::Engine;
    let base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(Some(base64))
}

/// Named screenshots and model feedback share the viewport source/encoder.
pub(crate) async fn take(
    state: &DaemonState,
    options: &ScreenshotOptions,
) -> Result<Option<ScreenshotResult>, String> {
    let Some(base64) = capture_base64(state, options).await? else {
        return Ok(None);
    };
    let path = crate::native::screenshot::save_screenshot(
        &base64,
        options.path.as_deref(),
        if options.format == "jpeg" {
            "jpg"
        } else {
            "png"
        },
        options.output_dir.as_deref(),
    )?;
    Ok(Some(ScreenshotResult {
        path,
        base64,
        annotations: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding(picture: &Snapshot) -> ViewportPictureBinding {
        ViewportPictureBinding {
            surface: picture.surface.clone(),
            layout_epoch: picture.layout_epoch,
            page_generation: "original document".into(),
            presentation: 0,
            context: 1,
            crop: picture.visible,
        }
    }

    #[test]
    fn picture_binding_cannot_infer_freshness_from_helper_input_sequence() {
        let mut picture = Snapshot::test_picture();
        let binding = binding(&picture);
        assert!(matches_binding(&picture, &binding, 100));
        picture.input_seq = Some(u64::MAX);
        assert!(!matches_binding(&picture, &binding, 101));
        picture.bounds.received_us = 99;
        assert!(!matches_binding(&picture, &binding, 100));
    }

    #[test]
    fn surface_identity_layout_units_scale_and_visible_crop_must_match() {
        let picture = Snapshot::test_picture();
        let original = binding(&picture);
        let mut changes = Vec::new();
        for field in ["generation", "kind", "coordinate"] {
            let mut binding = original.clone();
            match field {
                "generation" => binding.surface.generation = "other".into(),
                "kind" => binding.surface.kind = "other".into(),
                _ => binding.surface.coordinate_space = "CSS pixels".into(),
            };
            changes.push(binding);
        }
        let mut changed = original.clone();
        changed.layout_epoch += 1;
        changes.push(changed);
        let mut changed = original.clone();
        changed.surface.device_scale_factor += 1;
        changes.push(changed);
        let mut changed = original.clone();
        changed.surface.width += 1;
        changes.push(changed);
        let mut changed = original.clone();
        changed.surface.origin_x += 1;
        changes.push(changed);
        let mut changed = original.clone();
        changed.crop.x += 1;
        changes.push(changed);
        for changed in changes {
            assert!(!matches_binding(&picture, &changed, 100), "{changed:?}")
        }
        let mut cursor = picture.clone();
        cursor.surface.cursor_included = true;
        assert!(!matches_binding(&cursor, &original, 100));
    }
}
