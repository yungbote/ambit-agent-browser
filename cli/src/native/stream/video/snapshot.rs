//! Demand-only copies of the video producer's raw picture. This owns no
//! framebuffer reader, capture loop or page authority. The producer resolves
//! pending requests inside its existing picture callback; ordinary frames
//! make no copy. The screenshot adapter proves the foreground document and
//! measured content crop before writing a file.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::native::display::pictures::PictureReply;
use crate::native::display::{Rect, Surface};

/// Capture timing in the driver's media clock: CLOCK_MONOTONIC microseconds
/// on Linux. Helper waitUs is a duration, never a different clock's timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaptureBounds {
    pub requested_us: u64,
    pub received_us: u64,
    pub picture_us: u64,
}

/// One coherent producer picture, copied only for a live snapshot request.
/// Cursor pixels are absent, as in both video and the previous CDP screenshot;
/// viewers composite their own cursor separately.
#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub bounds: CaptureBounds,
    pub surface: Surface,
    pub visible: Rect,
    pub layout_epoch: u64,
    /// Existing stream input accounting, honestly scoped to its source. It
    /// is not proof that an agent action has been applied or painted.
    pub input_seq: Option<u64>,
    width: u32,
    height: u32,
    stride: usize,
    pixels: Arc<[u8]>,
}

impl Snapshot {
    fn copy(
        reply: &PictureReply,
        pixels: &[u8],
        surface: Surface,
        layout_epoch: u64,
        bounds: CaptureBounds,
        input_seq: Option<u64>,
    ) -> Result<Self, String> {
        let stride = usize::try_from(reply.stride).map_err(|_| "Invalid picture stride")?;
        let length = stride
            .checked_mul(reply.height as usize)
            .ok_or("Picture size overflow")?;
        if reply.width == 0
            || reply.height == 0
            || reply.stride
                != reply
                    .width
                    .checked_mul(4)
                    .ok_or("Picture stride overflow")?
            || reply.cursor_included
            || bounds.received_us < bounds.requested_us
            || bounds.picture_us < bounds.requested_us
            || bounds.picture_us > bounds.received_us
        {
            return Err("The producer picture does not satisfy the screenshot contract.".into());
        }
        let pixels = pixels
            .get(..length)
            .ok_or("The producer picture has incomplete pixels")?;
        Ok(Self {
            bounds,
            surface,
            visible: reply.window(),
            layout_epoch,
            input_seq,
            width: reply.width,
            height: reply.height,
            stride,
            pixels: Arc::from(pixels),
        })
    }

    /// An owned copy of the content crop. X11 pixels are BGRX; the displayed
    /// picture is opaque, so the unused fourth byte must not become alpha0.
    pub(crate) fn rgba(&self, crop: Rect) -> Result<image::RgbaImage, String> {
        let x =
            u32::try_from(crop.x).map_err(|_| "The screenshot crop starts outside the picture")?;
        let y =
            u32::try_from(crop.y).map_err(|_| "The screenshot crop starts outside the picture")?;
        if crop.width == 0
            || crop.height == 0
            || x.checked_add(crop.width)
                .is_none_or(|right| right > self.width)
            || y.checked_add(crop.height)
                .is_none_or(|bottom| bottom > self.height)
        {
            return Err("The screenshot crop is outside its producer picture.".into());
        }
        Ok(image::RgbaImage::from_fn(
            crop.width,
            crop.height,
            |cx, cy| {
                let index = (y + cy) as usize * self.stride + (x + cx) as usize * 4;
                image::Rgba([
                    self.pixels[index + 2],
                    self.pixels[index + 1],
                    self.pixels[index],
                    255,
                ])
            },
        ))
    }
}

struct Request {
    after_us: u64,
    reply: oneshot::Sender<Result<Arc<Snapshot>, String>>,
}

/// Owned under the producer's existing state lock. A dropped caller consumes
/// no pixels; requests whose lower bound is newer than this capture stay for
/// the next request. One pixel copy serves all eligible callers.
#[derive(Default)]
pub(super) struct Requests {
    pending: Vec<Request>,
}

impl Requests {
    pub(super) fn request(
        &mut self,
        after_us: u64,
    ) -> oneshot::Receiver<Result<Arc<Snapshot>, String>> {
        let (reply, receive) = oneshot::channel();
        self.pending.push(Request { after_us, reply });
        receive
    }

    pub(super) fn pending(&mut self) -> bool {
        self.pending.retain(|request| !request.reply.is_closed());
        !self.pending.is_empty()
    }

    pub(super) fn fail(&mut self, error: &str) {
        for request in self.pending.drain(..) {
            let _ = request.reply.send(Err(error.to_owned()));
        }
    }

    pub(super) fn captured(
        &mut self,
        reply: &PictureReply,
        pixels: &[u8],
        surface: Surface,
        layout_epoch: u64,
        bounds: CaptureBounds,
        input_seq: Option<u64>,
    ) {
        let mut eligible = Vec::new();
        let mut future = Vec::new();
        for request in self.pending.drain(..) {
            if request.reply.is_closed() {
                continue;
            }
            if request.after_us > bounds.requested_us {
                future.push(request);
            } else {
                eligible.push(request);
            }
        }
        self.pending = future;
        if eligible.is_empty() {
            return;
        }
        let snapshot =
            Snapshot::copy(reply, pixels, surface, layout_epoch, bounds, input_seq).map(Arc::new);
        for request in eligible {
            let _ = request.reply.send(snapshot.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture() -> PictureReply {
        PictureReply {
            width: 2,
            height: 2,
            stride: 8,
            rows: vec![[0, 2]],
            cursor_included: false,
            visible: None,
            timings: None,
            pointer: None,
        }
    }
    fn bounds() -> CaptureBounds {
        CaptureBounds {
            requested_us: 100,
            received_us: 120,
            picture_us: 110,
        }
    }
    fn pixels() -> Vec<u8> {
        vec![
            10, 20, 30, 0, 40, 50, 60, 0, 70, 80, 90, 0, 100, 110, 120, 0,
        ]
    }

    #[tokio::test]
    async fn one_copy_serves_live_requests_and_cancellation_copies_nothing() {
        let mut requests = Requests::default();
        let cancelled = requests.request(0);
        drop(cancelled);
        assert!(!requests.pending());
        // No live request: even malformed pixels are never read or allocated.
        requests.captured(&picture(), &[], Surface::new(2, 2), 1, bounds(), None);
        let first = requests.request(100);
        let second = requests.request(80);
        requests.captured(
            &picture(),
            &pixels(),
            Surface::new(2, 2),
            1,
            bounds(),
            Some(4),
        );
        let first = first.await.unwrap().unwrap();
        let second = second.await.unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.input_seq, Some(4));
        assert_eq!(
            first
                .rgba(Rect {
                    x: 1,
                    y: 1,
                    width: 1,
                    height: 1
                })
                .unwrap()
                .into_raw(),
            [120, 110, 100, 255]
        );
    }

    #[tokio::test]
    async fn a_capture_already_in_flight_cannot_satisfy_a_newer_acknowledgement() {
        let mut requests = Requests::default();
        let mut receive = requests.request(105);
        requests.captured(&picture(), &pixels(), Surface::new(2, 2), 1, bounds(), None);
        assert!(matches!(
            receive.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        requests.captured(
            &picture(),
            &pixels(),
            Surface::new(2, 2),
            1,
            CaptureBounds {
                requested_us: 106,
                received_us: 120,
                picture_us: 110,
            },
            None,
        );
        assert_eq!(receive.await.unwrap().unwrap().bounds.requested_us, 106);
    }

    #[tokio::test]
    async fn malformed_bounds_pixels_and_cursor_inclusion_never_publish_an_image() {
        for (reply, pixels, bounds) in [
            (picture(), vec![0], bounds()),
            (
                PictureReply {
                    cursor_included: true,
                    ..picture()
                },
                pixels(),
                bounds(),
            ),
            (
                picture(),
                pixels(),
                CaptureBounds {
                    picture_us: 99,
                    ..bounds()
                },
            ),
            (
                picture(),
                pixels(),
                CaptureBounds {
                    received_us: 99,
                    ..bounds()
                },
            ),
        ] {
            let mut requests = Requests::default();
            let receive = requests.request(0);
            requests.captured(&reply, &pixels, Surface::new(2, 2), 1, bounds, None);
            assert!(receive.await.unwrap().is_err());
        }
    }

    #[test]
    fn content_crop_must_fit_the_picture_and_uses_opaque_bgrx() {
        let snapshot =
            Snapshot::copy(&picture(), &pixels(), Surface::new(2, 2), 1, bounds(), None).unwrap();
        assert_eq!(
            snapshot
                .rgba(Rect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2
                })
                .unwrap()
                .into_raw(),
            [30, 20, 10, 255, 60, 50, 40, 255, 90, 80, 70, 255, 120, 110, 100, 255]
        );
        for crop in [
            Rect {
                x: -1,
                y: 0,
                width: 1,
                height: 1,
            },
            Rect {
                x: 1,
                y: 1,
                width: 2,
                height: 1,
            },
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 1,
            },
        ] {
            assert!(snapshot.rgba(crop).is_err());
        }
    }
}
