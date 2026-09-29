//! Closed host-only frames. Invalid input names a rule, never a state value.

use serde::Deserialize;
use serde_json::Value;

use super::state::{self, SiteState};

pub(crate) const VERSION: u64 = 3;
/// A native state is already JSON. Its enclosing fields have a pinned bound;
/// neither base64 nor a JSON string wrapping another JSON document is used.
pub(crate) const MAX_FRAME_BYTES: usize = state::MAX_BYTES + 1024;
pub(crate) const REFUSED: &str = "The browser site custody frame is invalid.";

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Mode {
    Read,
    Act,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Offer {
    pub(crate) site: String,
    pub(crate) mode: Mode,
    #[serde(default, rename = "useId")]
    pub(crate) use_id: Option<String>,
}

pub(crate) enum Request {
    Offer(Vec<Offer>),
    Attach {
        request_id: String,
        use_id: String,
        site: String,
        mode: Mode,
        state: SiteState,
    },
    Refuse {
        request_id: String,
        site: String,
    },
    Export(String),
    Detach {
        sites: Vec<String>,
        revoked: bool,
    },
}

fn uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .is_ok_and(|uuid| !uuid.is_nil() && uuid.hyphenated().to_string() == value)
}

pub(crate) fn is_kind(kind: &str) -> bool {
    matches!(
        kind,
        "site_sessions.offer"
            | "site_session.attach"
            | "site_session.refuse"
            | "site_session.export"
            | "site_session.detach"
    )
}

impl Request {
    pub(crate) fn read(kind: &str, value: Value) -> Result<Self, &'static str> {
        let mut fields = value.as_object().cloned().ok_or(REFUSED)?;
        fields.remove("id");
        let exact = |expected: &[&str]| {
            fields.len() == expected.len() && expected.iter().all(|key| fields.contains_key(*key))
        };
        let text = |key: &str| {
            fields
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(REFUSED)
        };
        let sites = || -> Result<Vec<String>, &'static str> {
            let values = fields
                .get("sites")
                .and_then(Value::as_array)
                .ok_or(REFUSED)?;
            let mut unique = std::collections::HashSet::new();
            values
                .iter()
                .map(|value| {
                    let value = value.as_str().ok_or(REFUSED)?;
                    state::site_url(value)?;
                    if !unique.insert(value) {
                        return Err(REFUSED);
                    }
                    Ok(value.to_owned())
                })
                .collect()
        };
        match kind {
            "site_sessions.offer" if exact(&["sites"]) => {
                let offers: Vec<Offer> =
                    serde_json::from_value(fields["sites"].clone()).map_err(|_| REFUSED)?;
                let mut unique = std::collections::HashSet::new();
                for offer in &offers {
                    state::site_url(&offer.site)?;
                    if offer.use_id.as_ref().is_some_and(|value| !uuid(value)) {
                        return Err(REFUSED);
                    }
                    if !unique.insert(&offer.site) {
                        return Err(REFUSED);
                    }
                }
                Ok(Self::Offer(offers))
            }
            "site_session.attach" if exact(&["requestId", "useId", "site", "mode", "state"]) => {
                let request_id = text("requestId")?;
                let use_id = text("useId")?;
                let site = text("site")?;
                if !uuid(&request_id) || !uuid(&use_id) {
                    return Err(REFUSED);
                }
                let mode = serde_json::from_value(fields["mode"].clone()).map_err(|_| REFUSED)?;
                let state = SiteState::read(fields["state"].clone(), &site)?;
                Ok(Self::Attach {
                    request_id,
                    use_id,
                    site,
                    mode,
                    state,
                })
            }
            "site_session.refuse" if exact(&["requestId", "site", "reason"]) => {
                let request_id = text("requestId")?;
                if !uuid(&request_id)
                    || !["denied", "expired", "unavailable"].contains(&text("reason")?.as_str())
                {
                    return Err(REFUSED);
                }
                let site = text("site")?;
                state::site_url(&site)?;
                Ok(Self::Refuse { request_id, site })
            }
            "site_session.export" if exact(&["sites"]) => {
                let mut sites = sites()?;
                if sites.len() != 1 {
                    return Err(REFUSED);
                }
                Ok(Self::Export(sites.remove(0)))
            }
            "site_session.detach" if exact(&["sites", "reason"]) => {
                let reason = text("reason")?;
                if !["revoked", "ended"].contains(&reason.as_str()) {
                    return Err(REFUSED);
                }
                Ok(Self::Detach {
                    sites: sites()?,
                    revoked: reason == "revoked",
                })
            }
            _ => Err(REFUSED),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn custody_request_shapes_are_closed_and_export_is_one_site() {
        assert!(Request::read(
            "site_sessions.offer",
            json!({"id":1,"sites":[{"site":"https://example.com","mode":"read"}]})
        )
        .is_ok());
        for value in [
            json!({"id":1,"sites":["https://example.com","https://other.com"]}),
            json!({"id":1,"sites":[]}),
            json!({"id":1,"sites":["https://example.com"],"extra":"private"}),
        ] {
            assert!(Request::read("site_session.export", value).is_err());
        }
        for site in [
            "https://example.com.attacker.com/path",
            "https://example.com:443",
            "https://name:private@example.com",
            "file://private",
        ] {
            assert!(Request::read("site_session.export", json!({"id":1,"sites":[site]})).is_err());
        }
    }

    #[test]
    fn the_largest_attach_envelope_is_bounded_without_expanding_state() {
        let envelope = json!({"action":"ambit_browser_agent","type":"site_session.attach","id":9007199254740991u64,
            "requestId":"ffffffff-ffff-ffff-ffff-ffffffffffff","useId":"ffffffff-ffff-ffff-ffff-ffffffffffff",
            "site":format!("https://{}.{}.{}.{}","x".repeat(63),"x".repeat(63),"x".repeat(63),"x".repeat(61)),
            "mode":"read","state":null});
        assert!(serde_json::to_vec(&envelope).unwrap().len() - 4 < 1024);
        let reply = json!({"id":9007199254740991u64,"success":true,"data":{"states":[null]}});
        assert!(serde_json::to_vec(&reply).unwrap().len() - 4 < 1024);
        assert_eq!(MAX_FRAME_BYTES, 8389632);
    }
}
