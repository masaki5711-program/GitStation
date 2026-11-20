//! The PFS Keycloak's organisation claims (ADR-0021 amendment 2026-11-20):
//! read strictly from a verified access token into a
//! [`forge_db::org_profiles::OrgSnapshot`], mirroring the PFS registry's
//! rules (`registry-api/src/domain/org.ts`):
//!
//! - `org_units`: a list (a single value counts as a list of one); each
//!   entry a JSON object `{"code", "primary"?}`, or a string holding one
//!   (Keycloak's multivalued user attribute can only emit strings), or a
//!   bare code (never primary). `code` `^[A-Z0-9][A-Z0-9_-]{0,19}$`,
//!   `primary` a boolean; at most one primary. The primary comes first.
//! - `positions`: `^[a-z][a-z0-9_]{0,63}$` each.
//! - `position_rank`: an integer 0..=999, as a number or a decimal string.
//! - `plants`: `^[A-Z0-9]{1,4}$` each; `plant_code` one such value.
//! - At most 64 entries per list. A missing (or `null`) claim is empty.
//!
//! The first violation is returned as `(field, reason)` with `reason` one of
//! `format`, `too_many`, `multiple_primary`, `type`. Values never leave this
//! module in an error.

use forge_db::org_profiles::{OrgSnapshot, OrgUnit, MAX_ENTRIES};
use serde_json::Value;

/// Why the claims were refused: the claim's name and the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrgClaimsInvalid {
    pub field: &'static str,
    pub reason: &'static str,
}

fn bad(field: &'static str, reason: &'static str) -> OrgClaimsInvalid {
    OrgClaimsInvalid { field, reason }
}

fn org_code_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 20
        && (b[0].is_ascii_uppercase() || b[0].is_ascii_digit())
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

fn position_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

fn plant_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 4
        && s.bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// The claim's entries: absent / `null` → none; a list → its items; one
/// value → a list of one.
fn entries<'a>(claims: &'a Value, field: &'static str) -> Result<Vec<&'a Value>, OrgClaimsInvalid> {
    let items: Vec<&Value> = match claims.get(field) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(one) => vec![one],
    };
    if items.len() > MAX_ENTRIES {
        return Err(bad(field, "too_many"));
    }
    Ok(items)
}

fn string_list(
    claims: &Value,
    field: &'static str,
    ok: fn(&str) -> bool,
) -> Result<Vec<String>, OrgClaimsInvalid> {
    entries(claims, field)?
        .into_iter()
        .map(|v| match v {
            Value::String(s) if ok(s) => Ok(s.clone()), // clone: the snapshot owns its values
            Value::String(_) => Err(bad(field, "format")),
            Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::Array(_)
            | Value::Object(_) => Err(bad(field, "type")),
        })
        .collect()
}

fn org_unit(value: &Value) -> Result<OrgUnit, OrgClaimsInvalid> {
    const FIELD: &str = "org_units";
    let parsed;
    let object = match value {
        Value::Object(_) => value,
        Value::String(s) if s.starts_with('{') => {
            parsed = serde_json::from_str::<Value>(s).map_err(|_| bad(FIELD, "type"))?;
            &parsed
        }
        Value::String(code) => {
            return if org_code_ok(code) {
                Ok(OrgUnit {
                    code: code.clone(), // clone: the snapshot owns its values
                    primary: false,
                })
            } else {
                Err(bad(FIELD, "format"))
            };
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => {
            return Err(bad(FIELD, "type"))
        }
    };
    let Value::Object(map) = object else {
        return Err(bad(FIELD, "type"));
    };
    let code = match map.get("code") {
        Some(Value::String(code)) if org_code_ok(code) => code.clone(),
        Some(Value::String(_)) => return Err(bad(FIELD, "format")),
        _ => return Err(bad(FIELD, "type")),
    };
    let primary = match map.get("primary") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad(FIELD, "type")),
    };
    Ok(OrgUnit { code, primary })
}

fn rank(claims: &Value) -> Result<Option<i32>, OrgClaimsInvalid> {
    const FIELD: &str = "position_rank";
    let n = match claims.get(FIELD) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(n)) => n.as_i64().ok_or_else(|| bad(FIELD, "format"))?,
        Some(Value::String(s)) => {
            if s.is_empty() || s.len() > 3 || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad(FIELD, "format"));
            }
            s.parse::<i64>().map_err(|_| bad(FIELD, "format"))?
        }
        Some(_) => return Err(bad(FIELD, "type")),
    };
    if !(0..=999).contains(&n) {
        return Err(bad(FIELD, "format"));
    }
    i32::try_from(n).map(Some).map_err(|_| bad(FIELD, "format"))
}

/// Read the organisation claims of a verified access token's `claims`.
pub fn parse(claims: &Value) -> Result<OrgSnapshot, OrgClaimsInvalid> {
    let mut org_units = entries(claims, "org_units")?
        .into_iter()
        .map(org_unit)
        .collect::<Result<Vec<_>, _>>()?;
    if org_units.iter().filter(|u| u.primary).count() > 1 {
        return Err(bad("org_units", "multiple_primary"));
    }
    // The primary first, the others in the order sent.
    org_units.sort_by_key(|u| !u.primary);
    let positions = string_list(claims, "positions", position_ok)?;
    let position_rank = rank(claims)?;
    let plants = string_list(claims, "plants", plant_ok)?;
    let plant_code = match claims.get("plant_code") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if plant_ok(s) => Some(s.clone()), // clone: as above
        Some(Value::String(_)) => return Err(bad("plant_code", "format")),
        Some(_) => return Err(bad("plant_code", "type")),
    };
    Ok(OrgSnapshot {
        org_units,
        positions,
        position_rank,
        plants,
        plant_code,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn unit(code: &str, primary: bool) -> OrgUnit {
        OrgUnit {
            code: code.to_owned(),
            primary,
        }
    }

    #[test]
    fn the_pfs_shape_is_read_with_the_primary_first() {
        let claims = json!({
            "org_units": [
                "{\"code\":\"SYN-B-0002\",\"primary\":false}",
                "{\"code\":\"SYN-A-0001\",\"primary\":true}",
            ],
            "positions": ["member", "line_lead"],
            "position_rank": "3",
            "plants": ["P001", "P002"],
            "plant_code": "P001",
        });
        let s = parse(&claims).expect("valid");
        assert_eq!(
            s.org_units,
            vec![unit("SYN-A-0001", true), unit("SYN-B-0002", false)]
        );
        assert_eq!(s.positions, vec!["member", "line_lead"]);
        assert_eq!(s.position_rank, Some(3));
        assert_eq!(s.plants, vec!["P001", "P002"]);
        assert_eq!(s.plant_code.as_deref(), Some("P001"));
    }

    #[test]
    fn objects_bare_codes_numbers_and_single_values_are_accepted() {
        let claims = json!({
            "org_units": [{"code": "SYN-A-0001", "primary": true}, "SYN_C"],
            "positions": "member",
            "position_rank": 999,
        });
        let s = parse(&claims).expect("valid");
        assert_eq!(
            s.org_units,
            vec![unit("SYN-A-0001", true), unit("SYN_C", false)]
        );
        assert_eq!(s.positions, vec!["member"]);
        assert_eq!(s.position_rank, Some(999));
    }

    #[test]
    fn missing_claims_are_an_empty_valid_snapshot() {
        assert_eq!(parse(&json!({})).expect("valid"), OrgSnapshot::default());
        assert_eq!(
            parse(&json!({"org_units": null, "position_rank": null})).expect("valid"),
            OrgSnapshot::default()
        );
    }

    #[test]
    fn every_violation_names_its_field_and_reason() {
        let many: Vec<String> = (0..=MAX_ENTRIES).map(|_| "member".to_owned()).collect();
        let cases = [
            (
                json!({"org_units": ["{\"code\":\"lower\"}"]}),
                "org_units",
                "format",
            ),
            (
                json!({"org_units": [{"code": "A".repeat(21)}]}),
                "org_units",
                "format",
            ),
            (json!({"org_units": ["{not json"]}), "org_units", "type"),
            (json!({"org_units": [7]}), "org_units", "type"),
            (json!({"org_units": [{"name": "x"}]}), "org_units", "type"),
            (
                json!({"org_units": [{"code": "A1", "primary": "yes"}]}),
                "org_units",
                "type",
            ),
            (
                json!({"org_units": [{"code": "A1", "primary": true}, {"code": "B1", "primary": true}]}),
                "org_units",
                "multiple_primary",
            ),
            (json!({"positions": ["Member"]}), "positions", "format"),
            (json!({"positions": [""]}), "positions", "format"),
            (json!({"positions": [1]}), "positions", "type"),
            (json!({"positions": many}), "positions", "too_many"),
            (json!({"position_rank": 1000}), "position_rank", "format"),
            (json!({"position_rank": -1}), "position_rank", "format"),
            (json!({"position_rank": 2.5}), "position_rank", "format"),
            (json!({"position_rank": "3a"}), "position_rank", "format"),
            (json!({"position_rank": true}), "position_rank", "type"),
            (json!({"plants": ["P0001"]}), "plants", "format"),
            (json!({"plants": {"a": 1}}), "plants", "type"),
            (json!({"plant_code": "p1"}), "plant_code", "format"),
            (json!({"plant_code": ["P1"]}), "plant_code", "type"),
        ];
        for (claims, field, reason) in cases {
            assert_eq!(
                parse(&claims).expect_err("invalid"),
                OrgClaimsInvalid { field, reason },
                "{claims}"
            );
        }
    }
}
