//! The driver's edge of ambit.browser-site-state.v1. Public-suffix authority
//! is the authenticated host's offered schemeful site, never a page claim.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use super::redaction::Redaction;

pub(crate) const FORMAT: &str = "ambit.browser-site-state.v1";
pub(crate) const MAX_BYTES: usize = 8 << 20;
pub(crate) const INVALID: &str = "The site state does not satisfy ambit.browser-site-state.v1.";

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SiteState {
    pub(crate) format: String,
    pub(crate) site: String,
    pub(crate) captured_at: String,
    pub(crate) chrome_major: u64,
    pub(crate) cookies: Vec<Cookie>,
    pub(crate) origins: Vec<Origin>,
    pub(crate) omitted: Vec<Omission>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Cookie {
    pub(crate) name: String,
    pub(crate) value: String,
    pub(crate) domain: String,
    pub(crate) path: String,
    pub(crate) expires: Option<f64>,
    pub(crate) http_only: bool,
    pub(crate) secure: bool,
    pub(crate) same_site: Option<String>,
    pub(crate) priority: String,
    pub(crate) source_scheme: String,
    pub(crate) partition_key: Option<Partition>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Partition {
    pub(crate) top_level_site: String,
    pub(crate) has_cross_site_ancestor: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Origin {
    pub(crate) origin: String,
    pub(crate) local_storage: Vec<(String, String)>,
    #[serde(rename = "indexedDB")]
    pub(crate) indexed_db: Vec<Database>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Database {
    pub(crate) name: String,
    pub(crate) version: u64,
    pub(crate) stores: Vec<Store>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Store {
    pub(crate) name: String,
    pub(crate) key_path: Option<Value>,
    pub(crate) auto_increment: bool,
    /// Historical v1 absence retains explicitly unknown generator fidelity.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_value"
    )]
    pub(crate) next_key: Option<Value>,
    pub(crate) indexes: Vec<Index>,
    pub(crate) records: Vec<Record>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Index {
    pub(crate) name: String,
    pub(crate) key_path: Value,
    pub(crate) unique: bool,
    pub(crate) multi_entry: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, deserialize_with = "present_value")]
    pub(crate) key: Option<Value>,
    pub(crate) value: Value,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Omission {
    pub(crate) origin: String,
    pub(crate) what: String,
    pub(crate) bytes: u64,
}

pub(crate) fn site_url(site: &str) -> Result<Url, &'static str> {
    let url = Url::parse(site).map_err(|_| INVALID)?;
    let host = url.host_str().ok_or(INVALID)?;
    if !matches!(url.scheme(), "http" | "https")
        || site != format!("{}://{host}", url.scheme())
        || host.len() > 253
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err(INVALID);
    }
    Ok(url)
}

pub(crate) fn host_in_site(host: &str, site: &Url) -> bool {
    let boundary = site.host_str().unwrap_or_default();
    host == boundary
        || host
            .strip_suffix(boundary)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

pub(crate) fn origin_in_site(origin: &str, site: &Url) -> bool {
    Url::parse(origin).is_ok_and(|url| {
        url.origin().ascii_serialization() == origin
            && url.scheme() == site.scheme()
            && url.host_str().is_some_and(|host| host_in_site(host, site))
    })
}

fn unique(values: impl IntoIterator<Item = String>) -> bool {
    let mut seen = std::collections::HashSet::new();
    values.into_iter().all(|value| seen.insert(value))
}

fn key_path(value: &Value) -> bool {
    value.is_string()
        || value
            .as_array()
            .is_some_and(|values| values.iter().all(Value::is_string))
}

fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

fn exact(value: &Value, keys: &[&str]) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key))
    })
}

fn required_shape(value: &Value) -> bool {
    if !exact(
        value,
        &[
            "format",
            "site",
            "capturedAt",
            "chromeMajor",
            "cookies",
            "origins",
            "omitted",
        ],
    ) {
        return false;
    }
    let all = |key: &str, predicate: fn(&Value) -> bool| {
        value[key]
            .as_array()
            .is_some_and(|values| values.iter().all(predicate))
    };
    all("cookies", |cookie| {
        exact(
            cookie,
            &[
                "name",
                "value",
                "domain",
                "path",
                "expires",
                "httpOnly",
                "secure",
                "sameSite",
                "priority",
                "sourceScheme",
                "partitionKey",
            ],
        ) && (cookie["partitionKey"].is_null()
            || exact(
                &cookie["partitionKey"],
                &["topLevelSite", "hasCrossSiteAncestor"],
            ))
    }) && all("origins", |origin| {
        exact(origin, &["origin", "localStorage", "indexedDB"])
            && origin["indexedDB"].as_array().is_some_and(|databases| {
                databases.iter().all(|database| {
                    exact(database, &["name", "version", "stores"])
                        && database["stores"].as_array().is_some_and(|stores| {
                            stores.iter().all(|store| {
                                (exact(
                                    store,
                                    &["name", "keyPath", "autoIncrement", "indexes", "records"],
                                ) || exact(
                                    store,
                                    &[
                                        "name",
                                        "keyPath",
                                        "autoIncrement",
                                        "nextKey",
                                        "indexes",
                                        "records",
                                    ],
                                )) && store["indexes"].as_array().is_some_and(|indexes| {
                                    indexes.iter().all(|index| {
                                        exact(index, &["name", "keyPath", "unique", "multiEntry"])
                                    })
                                }) && store["records"].as_array().is_some_and(|records| {
                                    records.iter().all(|record| {
                                        exact(
                                            record,
                                            if store["keyPath"].is_null() {
                                                &["key", "value"]
                                            } else {
                                                &["value"]
                                            },
                                        )
                                    })
                                })
                            })
                        })
                })
            })
    }) && all("omitted", |omission| {
        exact(omission, &["origin", "what", "bytes"])
    })
}

impl SiteState {
    pub(crate) fn read(value: Value, expected: &str) -> Result<Self, &'static str> {
        if !required_shape(&value)
            || serde_json::to_vec(&value).map_err(|_| INVALID)?.len() > MAX_BYTES
        {
            return Err(INVALID);
        }
        let state: Self = serde_json::from_value(value).map_err(|_| INVALID)?;
        let site = site_url(expected)?;
        if state.format != FORMAT
            || state.site != expected
            || state.chrome_major == 0
            || chrono::DateTime::parse_from_rfc3339(&state.captured_at).is_err()
            || !unique(state.cookies.iter().map(|cookie| {
                serde_json::to_string(&(
                    &cookie.name,
                    &cookie.domain,
                    &cookie.path,
                    &cookie
                        .partition_key
                        .as_ref()
                        .map(|key| (&key.top_level_site, key.has_cross_site_ancestor)),
                ))
                .unwrap_or_default()
            }))
            || !unique(state.origins.iter().map(|origin| origin.origin.clone()))
        {
            return Err(INVALID);
        }
        for cookie in &state.cookies {
            let domain = cookie.domain.strip_prefix('.').unwrap_or(&cookie.domain);
            let domain_url = site_url(&format!("{}://{domain}", site.scheme()))?;
            let belongs = match &cookie.partition_key {
                Some(partition) => {
                    site_url(&partition.top_level_site).is_ok()
                        && partition.top_level_site == expected
                }
                None => host_in_site(domain_url.host_str().unwrap_or_default(), &site),
            };
            if !belongs
                || !cookie.path.starts_with('/')
                || cookie
                    .expires
                    .is_some_and(|expiry| !expiry.is_finite() || expiry < 0.0)
                || cookie
                    .same_site
                    .as_ref()
                    .is_some_and(|value| !["strict", "lax", "none"].contains(&value.as_str()))
                || !["low", "medium", "high"].contains(&cookie.priority.as_str())
                || !["unset", "non_secure", "secure"].contains(&cookie.source_scheme.as_str())
            {
                return Err(INVALID);
            }
        }
        for origin in &state.origins {
            if !origin_in_site(&origin.origin, &site)
                || !unique(origin.local_storage.iter().map(|(key, _)| key.clone()))
                || !unique(
                    origin
                        .indexed_db
                        .iter()
                        .map(|database| database.name.clone()),
                )
            {
                return Err(INVALID);
            }
            for database in &origin.indexed_db {
                if database.version == 0
                    || database.version > (1 << 53) - 1
                    || !unique(database.stores.iter().map(|store| store.name.clone()))
                {
                    return Err(INVALID);
                }
                for store in &database.stores {
                    if let Some(next) = &store.next_key {
                        let valid = if store.auto_increment {
                            next == "exhausted"
                                || next.as_f64().is_some_and(|key| {
                                    key.is_finite()
                                        && key.fract() == 0.0
                                        && (1.0..=9007199254740992.0).contains(&key)
                                })
                        } else {
                            next.is_null()
                        };
                        if !valid {
                            return Err(INVALID);
                        }
                    }
                    if store.key_path.as_ref().is_some_and(|path| !key_path(path))
                        || !unique(store.indexes.iter().map(|index| index.name.clone()))
                        || store.indexes.iter().any(|index| !key_path(&index.key_path))
                    {
                        return Err(INVALID);
                    }
                    for record in &store.records {
                        if record.key.is_some() == store.key_path.is_some()
                            || record.key.as_ref().is_some_and(|key| !structured(key, 0))
                            || !structured(&record.value, 0)
                        {
                            return Err(INVALID);
                        }
                    }
                }
            }
        }
        if state.omitted.iter().any(|entry| {
            !origin_in_site(&entry.origin, &site)
                || !["local_storage", "indexed_db"].contains(&entry.what.as_str())
        }) {
            return Err(INVALID);
        }
        Ok(state)
    }

    pub(crate) fn register(&self, registry: &Redaction) {
        for cookie in &self.cookies {
            registry.register(&cookie.value);
        }
        for origin in &self.origins {
            for (key, value) in &origin.local_storage {
                registry.register(key);
                registry.register(value);
            }
            for database in &origin.indexed_db {
                for store in &database.stores {
                    for record in &store.records {
                        if let Some(key) = &record.key {
                            registry.register_tree(key);
                        }
                        registry.register_tree(&record.value);
                    }
                }
            }
        }
    }
}

/// Tagged structured clone, with the same finite-depth contract as the host.
fn structured(value: &Value, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => true,
        Value::Number(value) => value.as_f64().is_some_and(|number| {
            number.is_finite() && !(number == 0.0 && number.is_sign_negative())
        }),
        Value::Array(values) => values.iter().all(|value| structured(value, depth + 1)),
        Value::Object(object) => {
            let keys = |expected: &[&str]| {
                object.len() == expected.len()
                    && expected.iter().all(|key| object.contains_key(*key))
            };
            let string = |key| object.get(key).is_some_and(Value::is_string);
            let nested = |key| {
                object
                    .get(key)
                    .and_then(Value::as_array)
                    .is_some_and(|values| values.iter().all(|value| structured(value, depth + 1)))
            };
            let pairs = |key, names: bool| {
                object
                    .get(key)
                    .and_then(Value::as_array)
                    .is_some_and(|values| {
                        let mut seen = std::collections::HashSet::new();
                        values.iter().all(|value| {
                            value.as_array().is_some_and(|pair| {
                                pair.len() == 2
                                    && (if names {
                                        pair[0]
                                            .as_str()
                                            .is_some_and(|key| seen.insert(key.to_owned()))
                                    } else {
                                        structured(&pair[0], depth + 1)
                                    })
                                    && structured(&pair[1], depth + 1)
                            })
                        })
                    })
            };
            match object.get("$").and_then(Value::as_str) {
                Some("undefined") => keys(&["$"]),
                Some("number") => {
                    keys(&["$", "v"])
                        && object["v"].as_str().is_some_and(|value| {
                            ["NaN", "Infinity", "-Infinity", "-0"].contains(&value)
                        })
                }
                Some("bigint") => {
                    keys(&["$", "v"])
                        && object["v"].as_str().is_some_and(|value| {
                            let digits = value.strip_prefix('-').unwrap_or(value);
                            !digits.is_empty()
                                && (digits == "0" || !digits.starts_with('0'))
                                && digits.bytes().all(|byte| byte.is_ascii_digit())
                        })
                }
                Some("date") => {
                    keys(&["$", "v"])
                        && (object["v"].is_null()
                            || object["v"].as_f64().is_some_and(f64::is_finite))
                }
                Some("regexp") => {
                    keys(&["$", "source", "flags"])
                        && string("source")
                        && object["flags"].as_str().is_some_and(|flags| {
                            flags.chars().all(|flag| "dgimsuvy".contains(flag))
                        })
                }
                Some("object") => keys(&["$", "entries"]) && pairs("entries", true),
                Some("map") => keys(&["$", "entries"]) && pairs("entries", false),
                Some("set") => keys(&["$", "values"]) && nested("values"),
                Some("error") => {
                    keys(&["$", "name", "message"]) && string("name") && string("message")
                }
                Some("binary") => {
                    keys(&["$", "kind", "base64"])
                        && object["kind"].as_str().is_some_and(|kind| {
                            [
                                "ArrayBuffer",
                                "DataView",
                                "Int8Array",
                                "Uint8Array",
                                "Uint8ClampedArray",
                                "Int16Array",
                                "Uint16Array",
                                "Int32Array",
                                "Uint32Array",
                                "Float16Array",
                                "Float32Array",
                                "Float64Array",
                                "BigInt64Array",
                                "BigUint64Array",
                            ]
                            .contains(&kind)
                        })
                        && valid_base64(&object["base64"])
                }
                Some("blob") => {
                    keys(&["$", "type", "base64"])
                        && string("type")
                        && valid_base64(&object["base64"])
                }
                Some("file") => {
                    keys(&["$", "type", "name", "lastModified", "base64"])
                        && string("type")
                        && string("name")
                        && object["lastModified"].as_f64().is_some_and(f64::is_finite)
                        && valid_base64(&object["base64"])
                }
                _ => false,
            }
        }
    }
}

fn valid_base64(value: &Value) -> bool {
    use base64::{engine::general_purpose::STANDARD, Engine};
    value.as_str().is_some_and(|value| {
        STANDARD
            .decode(value)
            .is_ok_and(|bytes| STANDARD.encode(bytes) == value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn fixture() -> Value {
        json!({ "format": FORMAT, "site":"https://example.com", "capturedAt":"2026-09-29T00:00:00.000Z", "chromeMajor":154,
            "cookies":[{"name":"session","value":"fixture-identity-secret","domain":".example.com","path":"/","expires":1900000000,
                "httpOnly":true,"secure":true,"sameSite":"lax","priority":"medium","sourceScheme":"secure","partitionKey":null}],
            "origins":[{"origin":"https://app.example.com","localStorage":[["account","fixture-storage-secret"]],"indexedDB":[]}],"omitted":[] })
    }

    #[test]
    fn shape_site_cookie_partition_and_origin_are_authority_boundaries() {
        assert!(SiteState::read(fixture(), "https://example.com").is_ok());
        for pointer in ["/cookies/0/domain", "/origins/0/origin", "/site"] {
            let mut invalid = fixture();
            *invalid.pointer_mut(pointer).unwrap() = json!(if pointer.ends_with("domain") {
                "example.com.attacker.com"
            } else {
                "https://attacker.com"
            });
            assert_eq!(
                SiteState::read(invalid, "https://example.com").err(),
                Some(INVALID)
            );
        }
        let mut partition = fixture();
        partition["cookies"][0]["domain"] = json!("third-party.com");
        partition["cookies"][0]["partitionKey"] =
            json!({"topLevelSite":"https://example.com","hasCrossSiteAncestor":true});
        assert!(SiteState::read(partition.clone(), "https://example.com").is_ok());
        partition["cookies"][0]["partitionKey"]["topLevelSite"] = json!("https://attacker.com");
        assert!(SiteState::read(partition, "https://example.com").is_err());
    }

    #[test]
    fn malformed_or_oversized_state_never_puts_a_value_in_the_error() {
        let mut invalid = fixture();
        invalid["cookies"][0]["sameSite"] = json!("fixture-private-invalid-value");
        assert_eq!(
            SiteState::read(invalid, "https://example.com").err(),
            Some(INVALID)
        );
        let mut large = fixture();
        large["origins"][0]["localStorage"][0][1] = json!("x".repeat(MAX_BYTES));
        assert!(SiteState::read(large, "https://example.com").is_err());
        for value in [
            json!({"arbitrary":"object"}),
            json!({"$":"binary","kind":"Private","base64":""}),
            json!({"$":"object","entries":[["duplicate",1],["duplicate",2]]}),
        ] {
            assert!(!structured(&value, 0));
        }
    }
}
