//! One presentation owner for the existing stream, with connection-scoped
//! replacement and a bounded reconnect grace. This never grants browser input.

use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

const RECONNECT_GRACE: Duration = Duration::from_secs(2);

/// Capture rate and encoded-size budget for one viewing situation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FramePacing {
    pub fps: u32,
    pub budget_bytes: u32,
}

impl FramePacing {
    /// A human holds the input lease: input feedback must feel immediate.
    /// Sampling at the display's 60 Hz halves the average wait between a
    /// paint and its capture. Only damage is encoded, and the capture loop
    /// skips ticks the helper cannot keep, so a slow encode lowers the rate
    /// rather than queueing frames.
    pub(crate) const CONTROLLED: Self = Self {
        fps: 60,
        budget_bytes: 120_000,
    };
    /// A presenter is connected and watching the agent work.
    pub(crate) const PRESENTED: Self = Self {
        fps: 30,
        budget_bytes: 200_000,
    };
    /// Secondary viewers only.
    pub(crate) const PASSIVE: Self = Self {
        fps: 15,
        budget_bytes: 300_000,
    };

    pub(crate) fn period(self) -> Duration {
        Duration::from_micros(1_000_000 / u64::from(self.fps))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PresentationConfig {
    pub viewer: Uuid,
    pub width: u32,
    pub height: u32,
    /// The presenter draws frames 1:1 from the top-left cropped to their
    /// `visible` window (`visible=crop`), so it keeps drawing through a
    /// resize and sets the window through `presentation` even while its
    /// person controls input. An older presenter resizes through `viewport`
    /// input while it controls, and its presentation waits until then.
    pub crops: bool,
}

impl PresentationConfig {
    pub(crate) fn parse(viewer: &str, width: &str, height: &str) -> Option<Self> {
        let id = Uuid::parse_str(viewer).ok()?;
        let width = width.parse::<u32>().ok()?;
        let height = height.parse::<u32>().ok()?;
        (id.to_string() == viewer
            && !id.is_nil()
            && (1..=2048).contains(&width)
            && (1..=2048).contains(&height))
        .then_some(Self {
            viewer: id,
            width,
            height,
            crops: false,
        })
    }

    /// The window this presenter asks for, in display pixels.
    fn window(&self) -> (u32, u32) {
        (
            self.width * crate::native::display::DEVICE_SCALE_FACTOR,
            self.height * crate::native::display::DEVICE_SCALE_FACTOR,
        )
    }
}

#[derive(Clone)]
struct Owner {
    connection: Uuid,
    config: PresentationConfig,
    disconnected_until: Option<Instant>,
}

#[derive(Clone)]
struct Attempt {
    connection: Uuid,
    config: PresentationConfig,
    session: String,
    /// What the layout produced; `None` when it failed, which is not retried
    /// until the request changes.
    applied: Option<Applied>,
}

/// An applied window layout: the display's surface (its framebuffer, which a
/// size class may keep larger than the window) and the window, in display
/// pixels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Applied {
    pub surface: crate::native::display::Surface,
    pub window: (u32, u32),
}

#[derive(Clone, Default)]
pub(crate) struct PresentationState {
    owner: Option<Owner>,
    attempt: Option<Attempt>,
}

pub(crate) struct Presentation {
    cell: watch::Sender<PresentationState>,
}

#[derive(Clone)]
pub(crate) struct LayoutRequest {
    connection: Uuid,
    pub config: PresentationConfig,
}

impl Presentation {
    pub(crate) fn new() -> Self {
        let (cell, _) = watch::channel(PresentationState::default());
        Self { cell }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<PresentationState> {
        self.cell.subscribe()
    }

    pub(crate) fn configured(&self) -> bool {
        self.cell.borrow().owner.is_some()
    }

    /// The window size, in CSS pixels, that the owning view has set.
    pub(crate) fn layout(&self) -> Option<(u32, u32)> {
        self.cell
            .borrow()
            .owner
            .as_ref()
            .map(|owner| (owner.config.width, owner.config.height))
    }

    fn presented(&self) -> bool {
        self.cell
            .borrow()
            .owner
            .as_ref()
            .is_some_and(|owner| owner.disconnected_until.is_none())
    }

    /// Frame pacing follows who is looking: a controlling human gets the
    /// interactive rate, a connected presenter a reading rate, anyone else
    /// the passive rate. The budget steers the helper's quality for whole
    /// frames, smallest where frames are most frequent; what a viewer
    /// actually receives is bounded by its acknowledged in-flight window.
    pub(crate) fn capture_pacing(&self, controlled: bool) -> FramePacing {
        if controlled {
            FramePacing::CONTROLLED
        } else if self.presented() {
            FramePacing::PRESENTED
        } else {
            FramePacing::PASSIVE
        }
    }

    pub(crate) fn client_fps(&self, connection: Uuid, requested: u32, controlled: bool) -> u32 {
        let state = self.cell.borrow();
        let Some(owner) = state.owner.as_ref() else {
            return requested;
        };
        let limit = if owner.connection == connection && owner.disconnected_until.is_none() {
            if controlled {
                FramePacing::CONTROLLED.fps
            } else {
                FramePacing::PRESENTED.fps
            }
        } else {
            FramePacing::PASSIVE.fps
        };
        if requested == 0 {
            limit
        } else {
            requested.min(limit)
        }
    }

    pub(crate) fn configure(&self, connection: Uuid, config: PresentationConfig) {
        self.configure_inner(connection, config, true);
    }

    pub(crate) fn claim_if_available(&self, connection: Uuid, config: PresentationConfig) {
        self.configure_inner(connection, config, false);
    }

    fn configure_inner(&self, connection: Uuid, config: PresentationConfig, reconnect: bool) {
        self.cell.send_if_modified(|state| {
            let available = state.owner.as_ref().is_none_or(|owner| {
                owner
                    .disconnected_until
                    .is_some_and(|deadline| deadline <= Instant::now())
            });
            let same = state
                .owner
                .as_ref()
                .is_some_and(|owner| owner.config.viewer == config.viewer);
            if !(available || reconnect && same) {
                return false;
            }
            if state.owner.as_ref().is_some_and(|owner| {
                owner.connection == connection
                    && owner.config == config
                    && owner.disconnected_until.is_none()
            }) {
                return false;
            }
            if same {
                if let Some(attempt) = state
                    .attempt
                    .as_mut()
                    .filter(|attempt| attempt.config == config && attempt.applied.is_some())
                {
                    attempt.connection = connection;
                }
            }
            state.owner = Some(Owner {
                connection,
                config,
                disconnected_until: None,
            });
            true
        });
    }

    pub(crate) fn disconnect(&self, connection: Uuid) {
        self.cell.send_if_modified(|state| {
            let Some(owner) = state
                .owner
                .as_mut()
                .filter(|owner| owner.connection == connection)
            else {
                return false;
            };
            owner.disconnected_until = Some(Instant::now() + RECONNECT_GRACE);
            true
        });
    }

    /// When a disconnected presenter's grace ends, if one is pending.
    pub(crate) fn expiry(&self) -> Option<Instant> {
        self.cell
            .borrow()
            .owner
            .as_ref()
            .and_then(|owner| owner.disconnected_until)
    }

    pub(crate) fn expire(&self) {
        self.cell.send_if_modified(|state| {
            if state.owner.as_ref().is_some_and(|owner| {
                owner
                    .disconnected_until
                    .is_some_and(|deadline| deadline <= Instant::now())
            }) {
                state.owner = None;
                state.attempt = None;
                true
            } else {
                false
            }
        });
    }

    /// The connected presenter's layout for `session` (the display and its
    /// window), unless it was already attempted there and still holds: the
    /// window is the requested size, and the framebuffer equals the window
    /// unless every viewer crops to it (`size_class`). While a person
    /// controls input, only a presenter that crops sets the window this way.
    pub(crate) fn pending(
        &self,
        session: &str,
        size_class: bool,
        controlled: bool,
    ) -> Option<LayoutRequest> {
        let state = self.cell.borrow();
        let owner = state
            .owner
            .as_ref()
            .filter(|owner| owner.disconnected_until.is_none())
            .filter(|owner| !controlled || owner.config.crops)?;
        if state.attempt.as_ref().is_some_and(|attempt| {
            attempt.connection == owner.connection
                && attempt.config == owner.config
                && attempt.session == session
                && attempt.applied.as_ref().is_none_or(|applied| {
                    applied.window == owner.config.window()
                        && (size_class
                            || (applied.surface.width, applied.surface.height) == applied.window)
                })
        }) {
            return None;
        }
        Some(LayoutRequest {
            connection: owner.connection,
            config: owner.config,
        })
    }

    pub(crate) fn complete(
        &self,
        request: &LayoutRequest,
        session: String,
        applied: Option<Applied>,
    ) {
        self.cell.send_if_modified(|state| {
            if !state.owner.as_ref().is_some_and(|owner| {
                owner.connection == request.connection
                    && owner.config == request.config
                    && owner.disconnected_until.is_none()
            }) {
                return false;
            }
            state.attempt = Some(Attempt {
                connection: request.connection,
                config: request.config,
                session,
                applied,
            });
            true
        });
    }

    /// Another layout (the agent's, or a controller's `viewport` input)
    /// changed the window after the presenter's: the presenter's layout is
    /// pending again unless it still holds.
    pub(crate) fn update_surface(
        &self,
        surface: &crate::native::display::Surface,
        window: (u32, u32),
    ) {
        self.cell.send_if_modified(|state| {
            let Some(applied) = state
                .attempt
                .as_mut()
                .and_then(|attempt| attempt.applied.as_mut())
            else {
                return false;
            };
            let next = Applied {
                surface: surface.clone(),
                window,
            };
            if *applied == next {
                return false;
            }
            *applied = next;
            true
        });
    }

    pub(crate) fn acknowledgment(&self, connection: Uuid, config: PresentationConfig) -> Value {
        let state = self.cell.borrow();
        let primary = state.owner.as_ref().is_some_and(|owner| {
            owner.connection == connection && owner.disconnected_until.is_none()
        });
        let mut value = json!({ "type": "presentation", "role": if primary { "primary" } else { "secondary" },
            "requested": { "width": config.width, "height": config.height } });
        if let Some(attempt) = state.attempt.as_ref().filter(|attempt| {
            state.owner.as_ref().is_some_and(|owner| {
                owner.connection == attempt.connection && owner.config == attempt.config
            })
        }) {
            if let Some(applied) = attempt.applied.as_ref() {
                value["applied"] = json!(applied.surface);
            } else if primary {
                value["error"] = json!("viewport_unavailable");
            }
        }
        value
    }

    pub(crate) fn connection(self: &Arc<Self>, id: Uuid) -> PresentationConnection {
        PresentationConnection {
            presentation: self.clone(),
            id,
        }
    }
}

pub(crate) struct PresentationConnection {
    presentation: Arc<Presentation>,
    id: Uuid,
}
impl Drop for PresentationConnection {
    fn drop(&mut self) {
        self.presentation.disconnect(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(width: u32, height: u32) -> Applied {
        Applied {
            surface: crate::native::display::Surface::new(width, height),
            window: (width, height),
        }
    }

    #[test]
    fn one_presenter_reconnects_without_secondary_or_old_disconnect_taking_over() {
        let state = Presentation::new();
        let primary = PresentationConfig {
            viewer: Uuid::new_v4(),
            width: 540,
            height: 620,
            crops: false,
        };
        let secondary = PresentationConfig {
            viewer: Uuid::new_v4(),
            width: 900,
            height: 700,
            crops: false,
        };
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let replacement = Uuid::new_v4();
        state.configure(first, primary);
        state.configure(second, secondary);
        assert_eq!(state.acknowledgment(second, secondary)["role"], "secondary");
        state.disconnect(first);
        state.configure(second, secondary);
        assert!(state.pending("page", false, false).is_none());
        state.configure(
            replacement,
            PresentationConfig {
                width: 640,
                ..primary
            },
        );
        state.disconnect(first);
        let pending = state.pending("page", false, false).unwrap();
        assert_eq!(pending.config.width, 640);
        state.complete(&pending, "page".into(), Some(applied(1280, 1240)));
        assert!(state.pending("page", false, false).is_none());
        assert!(state.pending("new-page", false, false).is_some());
        assert_eq!(
            state.acknowledgment(replacement, pending.config)["applied"]["width"],
            1280
        );
    }

    #[test]
    fn invalid_dimensions_and_viewer_id_do_not_claim_presentation() {
        let id = Uuid::new_v4().to_string();
        assert!(PresentationConfig::parse(&id, "1", "2048").is_some());
        for width in ["0", "-1", "2049", "1.5", "large"] {
            assert!(PresentationConfig::parse(&id, width, "720").is_none());
        }
        assert!(
            PresentationConfig::parse("00000000-0000-0000-0000-000000000000", "640", "480")
                .is_none()
        );
    }

    #[test]
    fn primary_layout_is_restored_after_control_and_failed_reconnect_can_retry() {
        let state = Presentation::new();
        let config = PresentationConfig {
            viewer: Uuid::new_v4(),
            width: 780,
            height: 600,
            crops: false,
        };
        let first = Uuid::new_v4();
        state.configure(first, config);
        let request = state.pending("owned-window", false, false).unwrap();
        state.complete(&request, "owned-window".into(), Some(applied(1560, 1200)));
        state.update_surface(
            &crate::native::display::Surface::new(1280, 960),
            (1280, 960),
        );
        let restore = state.pending("owned-window", false, false).unwrap();
        assert_eq!(restore.config, config);
        state.complete(&restore, "owned-window".into(), None);
        assert!(state.pending("owned-window", false, false).is_none());
        assert_eq!(
            state.acknowledgment(first, config)["error"],
            "viewport_unavailable"
        );
        let reconnect = Uuid::new_v4();
        state.configure(reconnect, config);
        assert!(state.pending("owned-window", false, false).is_some());
        state.disconnect(first);
        assert_eq!(state.acknowledgment(reconnect, config)["role"], "primary");
    }

    #[test]
    fn only_the_connected_primary_gets_interactive_frame_pacing() {
        let state = Presentation::new();
        let primary = Uuid::new_v4();
        let secondary = Uuid::new_v4();
        let config = PresentationConfig {
            viewer: Uuid::new_v4(),
            width: 780,
            height: 600,
            crops: false,
        };
        assert_eq!(state.capture_pacing(false), FramePacing::PASSIVE);
        assert_eq!(state.client_fps(primary, 60, false), 60);
        state.configure(primary, config);
        assert_eq!(state.capture_pacing(false), FramePacing::PRESENTED);
        assert_eq!(state.client_fps(primary, 40, false), 30);
        assert_eq!(state.client_fps(primary, 5, false), 5);
        assert_eq!(state.client_fps(secondary, 20, false), 15);
        assert_eq!(state.client_fps(secondary, 0, false), 15);
        state.disconnect(primary);
        assert_eq!(state.capture_pacing(false), FramePacing::PASSIVE);
        assert_eq!(state.client_fps(primary, 20, false), 15);
        state.configure(secondary, config);
        assert_eq!(state.client_fps(primary, 20, false), 15);
        assert_eq!(state.client_fps(secondary, 40, false), 30);
    }

    /// A human lease raises the rate for the controlling presenter only, and
    /// keeps the smallest whole-frame budget where frames are most frequent.
    #[test]
    fn a_human_lease_raises_the_primary_rate_and_tightens_the_budget() {
        let state = Presentation::new();
        let primary = Uuid::new_v4();
        let secondary = Uuid::new_v4();
        let config = PresentationConfig {
            viewer: Uuid::new_v4(),
            width: 780,
            height: 600,
            crops: false,
        };
        assert_eq!(state.capture_pacing(true), FramePacing::CONTROLLED);
        state.configure(primary, config);
        assert_eq!(state.capture_pacing(true).fps, 60);
        const {
            assert!(
                FramePacing::CONTROLLED.budget_bytes < FramePacing::PRESENTED.budget_bytes
                    && FramePacing::PRESENTED.budget_bytes < FramePacing::PASSIVE.budget_bytes
            );
            assert!(
                FramePacing::CONTROLLED.fps > FramePacing::PRESENTED.fps
                    && FramePacing::PRESENTED.fps > FramePacing::PASSIVE.fps
            );
        }
        assert_eq!(state.client_fps(primary, 0, true), 60);
        assert_eq!(state.client_fps(primary, 20, true), 20);
        assert_eq!(state.client_fps(secondary, 0, true), 15);
        assert_eq!(
            FramePacing::CONTROLLED.period(),
            Duration::from_micros(16_666)
        );
        assert_eq!(
            FramePacing::PRESENTED.period(),
            Duration::from_micros(33_333)
        );
    }

    /// A framebuffer larger than the window satisfies a layout only while
    /// every viewer crops to the window; otherwise it is laid out again.
    #[test]
    fn a_size_class_layout_holds_only_while_every_viewer_crops() {
        let state = Presentation::new();
        let config = PresentationConfig {
            viewer: Uuid::new_v4(),
            width: 780,
            height: 600,
            crops: true,
        };
        state.configure(Uuid::new_v4(), config);
        let request = state.pending("window", true, false).unwrap();
        state.complete(
            &request,
            "window".into(),
            Some(Applied {
                surface: crate::native::display::Surface::new(1792, 1280),
                window: (1560, 1200),
            }),
        );
        assert!(state.pending("window", true, false).is_none());
        assert!(state.pending("window", false, false).is_some());
        // Another layout moved the window: the presenter's is pending again.
        state.update_surface(
            &crate::native::display::Surface::new(1792, 1280),
            (1280, 960),
        );
        assert!(state.pending("window", true, false).is_some());
    }

    /// While a person controls input, an older presenter resizes through its
    /// controller's `viewport` input; only a cropping presenter sets the
    /// window through its presentation then.
    #[test]
    fn under_control_only_a_cropping_presenter_sets_the_window() {
        let state = Presentation::new();
        let connection = Uuid::new_v4();
        let config = PresentationConfig {
            viewer: Uuid::new_v4(),
            width: 780,
            height: 600,
            crops: false,
        };
        state.configure(connection, config);
        assert!(state.pending("window", false, true).is_none());
        assert!(state.pending("window", false, false).is_some());
        state.configure(
            connection,
            PresentationConfig {
                crops: true,
                ..config
            },
        );
        assert!(state.pending("window", true, true).is_some());
    }

    /// A disconnected presenter's grace has a deadline its follower waits on.
    #[test]
    fn a_disconnected_presenter_expires_on_its_deadline() {
        let state = Presentation::new();
        let connection = Uuid::new_v4();
        state.configure(
            connection,
            PresentationConfig {
                viewer: Uuid::new_v4(),
                width: 780,
                height: 600,
                crops: false,
            },
        );
        assert!(state.expiry().is_none());
        state.disconnect(connection);
        let deadline = state.expiry().unwrap();
        assert!(deadline > Instant::now() && deadline <= Instant::now() + RECONNECT_GRACE);
        state.expire();
        assert!(state.configured(), "not before its deadline");
    }
}
