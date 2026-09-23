//! Find the renderer that receives a page-coordinate pointer event. CDP hit
//! testing stops at remote frame owners; descend through their actual content
//! quads, retaining the correct coordinate space across process boundaries.

use serde_json::{json, Value};
use std::collections::HashSet;

use super::client::CdpClient;
use crate::native::activity;

pub(super) struct PointerFrame {
    pub session: String,
    pub context: i64,
    pub x: f64,
    pub y: f64,
}

pub(super) async fn locate(
    client: &CdpClient,
    session: &str,
    frame: &str,
    context: i64,
    x: f64,
    y: f64,
) -> Result<PointerFrame, String> {
    let mut current = PointerFrame {
        session: session.into(),
        context,
        x,
        y,
    };
    let mut session_point = (x, y);
    let mut current_frame = frame.to_owned();
    let mut frames = HashSet::from([frame.to_owned()]);
    loop {
        // Native hit testing pierces shadow trees and local frames, stopping
        // at a remote frame's owner. The hit's frame and the node's owned
        // frame are distinct protocol fields; neither is inferred from markup.
        let hit = client.send_command("DOM.getNodeForLocation", Some(json!({"x":session_point.0.round() as i64,"y":session_point.1.round() as i64,"includeUserAgentShadowDOM":true})), Some(&current.session)).await?;
        let described = client
            .send_command(
                "DOM.describeNode",
                Some(json!({"backendNodeId":hit["backendNodeId"]})),
                Some(&current.session),
            )
            .await?;
        let child_frame = described["node"]["frameId"].as_str().or_else(|| {
            hit["frameId"]
                .as_str()
                .filter(|frame| *frame != current_frame)
        });
        let Some(child_frame) = child_frame else {
            return Ok(current);
        };
        if !frames.insert(child_frame.to_owned()) {
            return Err("The pointer frame changed during hit testing.".into());
        }
        let owner = client
            .send_command(
                "DOM.getFrameOwner",
                Some(json!({"frameId":child_frame})),
                Some(&current.session),
            )
            .await?;
        let model = client
            .send_command(
                "DOM.getBoxModel",
                Some(json!({ "backendNodeId": owner["backendNodeId"] })),
                Some(&current.session),
            )
            .await?;
        let Some((u, v)) = unit_point(&model["model"]["content"], session_point) else {
            // The iframe's border belongs to its parent renderer.
            return Ok(current);
        };
        let child_session = client
            .session_for_target(child_frame)
            .unwrap_or_else(|| current.session.clone());
        let world = client
            .send_command(
                "Page.createIsolatedWorld",
                Some(json!({"frameId":child_frame,"worldName":activity::POINTER_WORLD})),
                Some(&child_session),
            )
            .await?;
        let child_context = world["executionContextId"]
            .as_i64()
            .ok_or("The hit frame has no observation realm")?;
        let size = client.send_command("Runtime.evaluate", Some(json!({"expression":"({width:innerWidth,height:innerHeight})","contextId":child_context,"returnByValue":true})), Some(&child_session)).await?;
        let width = size["result"]["value"]["width"]
            .as_f64()
            .filter(|size| size.is_finite() && *size > 0.0)
            .ok_or("The hit frame has no viewport width")?;
        let height = size["result"]["value"]["height"]
            .as_f64()
            .filter(|size| size.is_finite() && *size > 0.0)
            .ok_or("The hit frame has no viewport height")?;
        let point = (u * width, v * height);
        if child_session != current.session {
            session_point = point;
        }
        current_frame = child_frame.into();
        current = PointerFrame {
            session: child_session,
            context: child_context,
            x: point.0,
            y: point.1,
        };
    }
}

/// Invert the homography from a unit rectangle to its rendered quad. This
/// covers translation, scale, rotation and perspective without treating an
/// axis-aligned bounding box as the iframe's coordinate system.
fn unit_point(quad: &Value, point: (f64, f64)) -> Option<(f64, f64)> {
    let values = quad.as_array()?;
    if values.len() != 8 {
        return None;
    }
    let mut q = [0.0; 8];
    for (out, value) in q.iter_mut().zip(values) {
        *out = value.as_f64().filter(|value| value.is_finite())?;
    }
    let (sx, sy) = (q[0] - q[2] + q[4] - q[6], q[1] - q[3] + q[5] - q[7]);
    let (g, h) = if sx.abs() + sy.abs() < 1e-9 {
        (0.0, 0.0)
    } else {
        let (dx1, dx2, dy1, dy2) = (q[2] - q[4], q[6] - q[4], q[3] - q[5], q[7] - q[5]);
        let det = dx1 * dy2 - dx2 * dy1;
        if det.abs() < 1e-9 {
            return None;
        }
        ((sx * dy2 - dx2 * sy) / det, (dx1 * sy - sx * dy1) / det)
    };
    let (a, b, d, e) = (
        q[2] - q[0] + g * q[2] - point.0 * g,
        q[6] - q[0] + h * q[6] - point.0 * h,
        q[3] - q[1] + g * q[3] - point.1 * g,
        q[7] - q[1] + h * q[7] - point.1 * h,
    );
    let determinant = a * e - b * d;
    if determinant.abs() < 1e-9 {
        return None;
    }
    let (x, y) = (point.0 - q[0], point.1 - q[1]);
    let (u, v) = ((x * e - b * y) / determinant, (a * y - x * d) / determinant);
    (u.is_finite() && v.is_finite() && (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v))
        .then_some((u, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_coordinates_cover_scaled_rotated_and_perspective_quads() {
        assert_eq!(
            unit_point(&json!([10, 20, 210, 20, 210, 120, 10, 120]), (60.0, 70.0)),
            Some((0.25, 0.5))
        );
        assert_eq!(
            unit_point(&json!([100, 0, 200, 100, 100, 200, 0, 100]), (100.0, 100.0)),
            Some((0.5, 0.5))
        );
        // Projective quad with x=(100u+20v)/(1+0.5v), y=100v/(1+0.5v).
        let quad = json!([0, 0, 100, 0, 80, 100.0 / 1.5, 20.0 / 1.5, 100.0 / 1.5]);
        let (u, v) = unit_point(&quad, (35.0 / 1.25, 50.0 / 1.25)).unwrap();
        assert!((u - 0.25).abs() < 1e-9 && (v - 0.5).abs() < 1e-9);
        assert!(unit_point(&json!([0, 0, 0, 0, 0, 0, 0, 0]), (0.0, 0.0)).is_none());
        assert!(unit_point(&json!([10, 20, 210, 20, 210, 120, 10, 120]), (0.0, 0.0)).is_none());
    }
}
