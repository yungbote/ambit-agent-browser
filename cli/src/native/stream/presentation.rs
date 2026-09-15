//! One presentation owner for the existing stream, with connection-scoped
//! replacement and a bounded reconnect grace. This never grants browser input.

use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use uuid::Uuid;

const RECONNECT_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PresentationConfig {
    pub viewer: Uuid,
    pub width: u32,
    pub height: u32,
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
        })
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
    surface: Option<crate::native::display::Surface>,
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
                    .filter(|attempt| attempt.config == config && attempt.surface.is_some())
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

    pub(crate) fn pending(&self, session: &str) -> Option<LayoutRequest> {
        let state = self.cell.borrow();
        let owner = state
            .owner
            .as_ref()
            .filter(|owner| owner.disconnected_until.is_none())?;
        if state.attempt.as_ref().is_some_and(|attempt| {
            attempt.connection == owner.connection
                && attempt.config == owner.config
                && attempt.session == session
                && attempt.surface.as_ref().is_none_or(|surface| {
                    surface.width
                        == owner.config.width * crate::native::display::DEVICE_SCALE_FACTOR
                        && surface.height
                            == owner.config.height * crate::native::display::DEVICE_SCALE_FACTOR
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
        surface: Option<crate::native::display::Surface>,
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
                surface,
            });
            true
        });
    }

    pub(crate) fn update_surface(&self, surface: &crate::native::display::Surface) {
        self.cell.send_if_modified(|state| {
            let Some(attempt) = state
                .attempt
                .as_mut()
                .filter(|attempt| attempt.surface.is_some())
            else {
                return false;
            };
            if attempt.surface.as_ref() == Some(surface) {
                return false;
            }
            attempt.surface = Some(surface.clone());
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
            if let Some(surface) = attempt.surface.as_ref() {
                value["applied"] = json!(surface);
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

    #[test]
    fn one_presenter_reconnects_without_secondary_or_old_disconnect_taking_over() {
        let state = Presentation::new();
        let primary = PresentationConfig {
            viewer: Uuid::new_v4(),
            width: 540,
            height: 620,
        };
        let secondary = PresentationConfig {
            viewer: Uuid::new_v4(),
            width: 900,
            height: 700,
        };
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let replacement = Uuid::new_v4();
        state.configure(first, primary);
        state.configure(second, secondary);
        assert_eq!(state.acknowledgment(second, secondary)["role"], "secondary");
        state.disconnect(first);
        state.configure(second, secondary);
        assert!(state.pending("page").is_none());
        state.configure(
            replacement,
            PresentationConfig {
                width: 640,
                ..primary
            },
        );
        state.disconnect(first);
        let pending = state.pending("page").unwrap();
        assert_eq!(pending.config.width, 640);
        state.complete(
            &pending,
            "page".into(),
            Some(crate::native::display::Surface::new(1280, 1240)),
        );
        assert!(state.pending("page").is_none());
        assert!(state.pending("new-page").is_some());
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
        };
        let first = Uuid::new_v4();
        state.configure(first, config);
        let request = state.pending("owned-window").unwrap();
        state.complete(
            &request,
            "owned-window".into(),
            Some(crate::native::display::Surface::new(1560, 1200)),
        );
        state.update_surface(&crate::native::display::Surface::new(1280, 960));
        let restore = state.pending("owned-window").unwrap();
        assert_eq!(restore.config, config);
        state.complete(&restore, "owned-window".into(), None);
        assert!(state.pending("owned-window").is_none());
        assert_eq!(
            state.acknowledgment(first, config)["error"],
            "viewport_unavailable"
        );
        let reconnect = Uuid::new_v4();
        state.configure(reconnect, config);
        assert!(state.pending("owned-window").is_some());
        state.disconnect(first);
        assert_eq!(state.acknowledgment(reconnect, config)["role"], "primary");
    }
}
