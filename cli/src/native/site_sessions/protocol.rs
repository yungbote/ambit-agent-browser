//! Closed host-only frames. Invalid input names a rule, never a state value.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::Value;

use super::state::{self, SiteState};

pub(crate) const VERSION: u64 = 4;
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
    #[serde(default, rename = "expiresAt")]
    pub(crate) expires_at: Option<String>,
}

pub(crate) enum Request {
    Offer(Vec<Offer>),
    Attach {
        request_id: String,
        use_id: String,
        site: String,
        mode: Mode,
        expires_at: Option<String>,
        state: SiteState,
    },
    Transfer(TransferRequest),
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

pub(crate) enum TransferRequest {
    Begin {
        request_id: String,
        use_id: String,
        site: String,
        mode: Mode,
        page_generation: String,
        deadline: chrono::DateTime<chrono::Utc>,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    },
    Finish {
        transfer_id: String,
        bytes: u64,
        sha256: String,
    },
    Part {
        transfer_id: String,
        offset: u64,
        data: Vec<u8>,
    },
    Read {
        transfer_id: String,
        offset: u64,
    },
    Close {
        transfer_id: String,
    },
    Export {
        site: String,
        expected_use: Option<String>,
        deadline: chrono::DateTime<chrono::Utc>,
    },
    Detach {
        site: String,
        expected_use: Option<String>,
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
            | "site_session.attach.begin"
            | "site_session.attach.finish"
            | "site_session.state.part"
            | "site_session.state.read"
            | "site_session.state.close"
    )
}

pub(crate) fn expiry(
    value: Option<&str>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, &'static str> {
    value
        .map(|value| {
            if value.len() > 35 {
                return Err(REFUSED);
            }
            chrono::DateTime::parse_from_rfc3339(value)
                .map(|value| value.with_timezone(&chrono::Utc))
                .map_err(|_| REFUSED)
        })
        .transpose()
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
        let identifier = |key: &str| {
            let value = text(key)?;
            if uuid(&value) {
                Ok(value)
            } else {
                Err(REFUSED)
            }
        };
        let integer = |key: &str| {
            fields
                .get(key)
                .and_then(Value::as_u64)
                .filter(|value| *value <= 9_007_199_254_740_991)
                .ok_or(REFUSED)
        };
        let expected_use = || match fields.get("expectedUseId") {
            Some(Value::Null) => Ok(None),
            Some(Value::String(value)) if uuid(value) => Ok(Some(value.clone())),
            _ => Err(REFUSED),
        };
        let deadline = || expiry(Some(&text("deadlineAt")?))?.ok_or(REFUSED);
        let one_site = || {
            let mut values = sites()?;
            if values.len() != 1 {
                return Err(REFUSED);
            }
            Ok(values.remove(0))
        };
        match kind {
            "site_session.attach.begin"
                if exact(&[
                    "requestId",
                    "useId",
                    "site",
                    "mode",
                    "pageGeneration",
                    "deadlineAt",
                ]) || exact(&[
                    "requestId",
                    "useId",
                    "site",
                    "mode",
                    "pageGeneration",
                    "deadlineAt",
                    "expiresAt",
                ]) =>
            {
                let site = text("site")?;
                state::site_url(&site)?;
                let page_generation = text("pageGeneration")?;
                if page_generation.is_empty() || page_generation.len() > 256 {
                    return Err(REFUSED);
                }
                let expires_at = match fields.get("expiresAt") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(value)) => expiry(Some(value))?,
                    _ => return Err(REFUSED),
                };
                Ok(Self::Transfer(TransferRequest::Begin {
                    request_id: identifier("requestId")?,
                    use_id: identifier("useId")?,
                    site,
                    mode: serde_json::from_value(fields["mode"].clone()).map_err(|_| REFUSED)?,
                    page_generation,
                    deadline: deadline()?,
                    expires_at,
                }))
            }
            "site_session.attach.finish" if exact(&["transferId", "bytes", "sha256"]) => {
                let sha256 = text("sha256")?;
                if sha256.len() != 64
                    || !sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(REFUSED);
                }
                let bytes = integer("bytes")?;
                if bytes == 0 {
                    return Err(REFUSED);
                }
                Ok(Self::Transfer(TransferRequest::Finish {
                    transfer_id: identifier("transferId")?,
                    bytes,
                    sha256,
                }))
            }
            "site_session.state.part" if exact(&["transferId", "offset", "data"]) => {
                let encoded = text("data")?;
                if encoded.is_empty() || encoded.len() > super::bytes::CHUNK_BYTES.div_ceil(3) * 4 {
                    return Err(REFUSED);
                }
                let data = STANDARD.decode(&encoded).map_err(|_| REFUSED)?;
                if data.len() > super::bytes::CHUNK_BYTES || STANDARD.encode(&data) != encoded {
                    return Err(REFUSED);
                }
                Ok(Self::Transfer(TransferRequest::Part {
                    transfer_id: identifier("transferId")?,
                    offset: integer("offset")?,
                    data,
                }))
            }
            "site_session.state.read" if exact(&["transferId", "offset"]) => {
                Ok(Self::Transfer(TransferRequest::Read {
                    transfer_id: identifier("transferId")?,
                    offset: integer("offset")?,
                }))
            }
            "site_session.state.close" if exact(&["transferId"]) => {
                Ok(Self::Transfer(TransferRequest::Close {
                    transfer_id: identifier("transferId")?,
                }))
            }
            "site_session.export"
                if exact(&["sites", "transfer", "expectedUseId", "deadlineAt"])
                    && fields["transfer"] == true =>
            {
                Ok(Self::Transfer(TransferRequest::Export {
                    site: one_site()?,
                    expected_use: expected_use()?,
                    deadline: deadline()?,
                }))
            }
            "site_session.detach" if exact(&["sites", "reason", "expectedUseId"]) => {
                let reason = text("reason")?;
                if !["revoked", "ended"].contains(&reason.as_str()) {
                    return Err(REFUSED);
                }
                Ok(Self::Transfer(TransferRequest::Detach {
                    site: one_site()?,
                    expected_use: expected_use()?,
                    revoked: reason == "revoked",
                }))
            }
            "site_sessions.offer" if exact(&["sites"]) => {
                let offers: Vec<Offer> =
                    serde_json::from_value(fields["sites"].clone()).map_err(|_| REFUSED)?;
                let mut unique = std::collections::HashSet::new();
                for offer in &offers {
                    state::site_url(&offer.site)?;
                    if offer.use_id.as_ref().is_some_and(|value| !uuid(value)) {
                        return Err(REFUSED);
                    }
                    expiry(offer.expires_at.as_deref())?;
                    if !unique.insert(&offer.site) {
                        return Err(REFUSED);
                    }
                }
                Ok(Self::Offer(offers))
            }
            "site_session.attach"
                if exact(&["requestId", "useId", "site", "mode", "state"])
                    || exact(&["requestId", "useId", "site", "mode", "state", "expiresAt"]) =>
            {
                let request_id = text("requestId")?;
                let use_id = text("useId")?;
                let site = text("site")?;
                if !uuid(&request_id) || !uuid(&use_id) {
                    return Err(REFUSED);
                }
                let mode = serde_json::from_value(fields["mode"].clone()).map_err(|_| REFUSED)?;
                let expires_at = match fields.get("expiresAt") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(value)) => Some(value.clone()),
                    _ => return Err(REFUSED),
                };
                expiry(expires_at.as_deref())?;
                let state = SiteState::read(fields["state"].clone(), &site)?;
                Ok(Self::Attach {
                    request_id,
                    use_id,
                    site,
                    mode,
                    expires_at,
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
            "mode":"read","state":null,"expiresAt":"2026-09-29T00:00:00.123456789+00:00"});
        assert!(serde_json::to_vec(&envelope).unwrap().len() - 4 < 1024);
        let reply = json!({"id":9007199254740991u64,"success":true,"data":{"states":[null]}});
        assert!(serde_json::to_vec(&reply).unwrap().len() - 4 < 1024);
        assert_eq!(MAX_FRAME_BYTES, 8389632);
    }

    #[test]
    fn custody_offer_adoption_and_expiry_metadata_are_closed_and_bounded() {
        assert!(Request::read("site_sessions.offer",json!({"sites":[{"site":"https://example.com","mode":"act",
            "useId":"11111111-1111-4111-8111-111111111111","expiresAt":"2026-09-29T00:00:00.000Z"}]})).is_ok());
        for fields in [
            json!({"useId":"11111111-1111-4111-8111-111111111111"}),
            json!({"useId":"not-a-uuid"}),
            json!({"expiresAt":123}),
            json!({"expiresAt":"invalid"}),
        ] {
            let mut offer = json!({"site":"https://example.com","mode":"act"});
            offer
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let admitted = Request::read("site_sessions.offer", json!({"sites":[offer]})).is_ok();
            assert_eq!(
                admitted,
                fields["useId"] == "11111111-1111-4111-8111-111111111111"
            );
        }
    }
}

#[cfg(test)]
mod transfer_tests {
    use super::*;
    use serde_json::json;
    const ID: &str = "11111111-1111-4111-8111-111111111111";
    #[test]
    fn bounded_transfer_frames_have_exact_authority_and_byte_shapes() {
        let begin = json!({"requestId":ID,"useId":ID,"site":"https://example.com","mode":"act","pageGeneration":"page","deadlineAt":"2026-10-01T00:00:00Z"});
        assert!(Request::read("site_session.attach.begin", begin.clone()).is_ok());
        for (key, value) in [
            ("bytes", json!(1)),
            ("sha256", json!("a".repeat(64))),
            ("pageGeneration", json!("")),
            ("deadlineAt", json!("invalid")),
            ("useId", Value::Null),
        ] {
            let mut invalid = begin.clone();
            invalid[key] = value;
            assert!(Request::read("site_session.attach.begin", invalid).is_err());
        }
        let data = vec![b'x'; super::super::bytes::CHUNK_BYTES];
        assert!(Request::read(
            "site_session.state.part",
            json!({"transferId":ID,"offset":0,"data":STANDARD.encode(&data)})
        )
        .is_ok());
        for encoded in [
            STANDARD.encode(vec![b'x'; data.len() + 1]),
            "eA".into(),
            "eB==".into(),
            "eA==\n".into(),
            "".into(),
        ] {
            assert!(Request::read(
                "site_session.state.part",
                json!({"transferId":ID,"offset":0,"data":encoded})
            )
            .is_err());
        }
        assert!(Request::read(
            "site_session.attach.finish",
            json!({"transferId":ID,"bytes":1,"sha256":"a".repeat(64)})
        )
        .is_ok());
        assert!(Request::read(
            "site_session.attach.finish",
            json!({"transferId":ID,"bytes":0,"sha256":"a".repeat(64)})
        )
        .is_err());
        assert!(Request::read("site_session.export",json!({"sites":["https://example.com"],"transfer":true,"expectedUseId":null,"deadlineAt":"2026-10-01T00:00:00Z"})).is_ok());
        assert!(Request::read("site_session.export",json!({"sites":["https://example.com"],"transfer":true,"deadlineAt":"2026-10-01T00:00:00Z"})).is_err());
    }
}
