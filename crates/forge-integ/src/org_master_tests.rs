#![allow(clippy::expect_used, clippy::panic)]
//! The registry's org lists as GitStation validates them (ADR-0038 §1; the
//! PFS contract of MR !28): every field, the envelope, duplicates, the
//! limits. Synthetic codes and names only.

use serde_json::{json, Value};

use super::*;

fn envelope(items: Value) -> Vec<u8> {
    json!({"success": true, "data": {"items": items}})
        .to_string()
        .into_bytes()
}

fn unit() -> Value {
    json!({
        "plantCode": "P001",
        "code": "SYN-SEC-01",
        "level": "section",
        "parentCode": "SYN-DEP-01",
        "name": {"ja": "合成課", "en": "Synthetic Section", "id": null},
        "costCenter": "CC100",
        "headStaffId": "SYN0001",
        "validFrom": "2026-04-01",
        "version": 3
    })
}

fn position() -> Value {
    json!({
        "plantCode": "P001",
        "code": "manager",
        "name": "マネージャー",
        "rank": 55,
        "sortOrder": 6,
        "active": true,
        "version": 1
    })
}

fn unit_with(field: &str, value: Value) -> Vec<u8> {
    let mut u = unit();
    u[field] = value;
    envelope(json!([u]))
}

#[test]
fn a_valid_unit_and_position_list_is_read_field_by_field() {
    let units = parse_units(&envelope(json!([unit()]))).expect("units");
    assert_eq!(units.len(), 1);
    let u = &units[0];
    assert_eq!(u.plant_code, "P001");
    assert_eq!(u.level, UnitLevel::Section);
    assert_eq!(u.parent_code.as_deref(), Some("SYN-DEP-01"));
    assert_eq!(u.name.ja, "合成課");
    assert_eq!(u.name.en.as_deref(), Some("Synthetic Section"));
    assert_eq!(u.name.id, None);
    assert_eq!(u.valid_from.as_deref(), Some("2026-04-01"));
    assert_eq!(u.version, 3);
    // Optional fields may be absent, null or empty; a timestamp keeps its date.
    let mut bare = unit();
    let map = bare.as_object_mut().expect("object");
    map.remove("parentCode");
    map.remove("costCenter");
    map.insert("headStaffId".to_owned(), Value::Null);
    map.insert("validFrom".to_owned(), json!("2026-04-01T00:00:00Z"));
    map.insert("extra".to_owned(), json!("ignored"));
    let bare = parse_units(&envelope(json!([bare]))).expect("bare");
    assert_eq!(bare[0].parent_code, None);
    assert_eq!(bare[0].valid_from.as_deref(), Some("2026-04-01"));

    let positions = parse_positions(&envelope(json!([position()]))).expect("positions");
    assert_eq!(positions[0].rank, 55);
    assert_eq!(positions[0].name.ja, "マネージャー");
    let mut named = position();
    named["name"] = json!({"ja": "部長", "en": "General manager", "id": "Manajer umum"});
    let named = parse_positions(&envelope(json!([named]))).expect("named");
    assert_eq!(named[0].name.id.as_deref(), Some("Manajer umum"));
    // No positions yet is valid; no units is not.
    assert!(parse_positions(&envelope(json!([])))
        .expect("empty")
        .is_empty());
    assert_eq!(parse_units(&envelope(json!([]))), Err(Invalid::of("empty")));
}

#[test]
fn every_unit_field_is_checked() {
    for (field, value) in [
        ("plantCode", json!("p001")),
        ("plantCode", json!("P0001")),
        ("code", json!("syn-sec")),
        ("code", json!("-SEC")),
        ("code", json!("A".repeat(21))),
        ("level", json!("unit")),
        ("level", json!(2)),
        ("parentCode", json!("bad code")),
        ("name", json!({"en": "no ja"})),
        ("name", json!({"ja": ""})),
        ("name", json!({"ja": "a\u{202E}b"})),
        ("name", json!({"ja": "x".repeat(201)})),
        ("name", json!(7)),
        ("costCenter", json!("c".repeat(65))),
        ("costCenter", json!(5)),
        ("headStaffId", json!("SYN 0001")),
        ("validFrom", json!("2026-02-30")),
        ("validFrom", json!("2026/04/01")),
        ("validFrom", json!(20260401)),
        ("version", json!(-1)),
        ("version", json!("3")),
        ("version", Value::Null),
    ] {
        let err = parse_units(&unit_with(field, value.clone())).expect_err(field);
        assert_eq!(err.reason, "field", "{field}: {value}");
        assert_eq!(err.field, Some(field), "{field}: {value}");
        assert_eq!(err.index, Some(0));
    }
    let err = parse_units(&unit_with("parentCode", json!("SYN-SEC-01"))).expect_err("self");
    assert_eq!(err.reason, "self_parent");
}

#[test]
fn every_position_field_is_checked() {
    for (field, value) in [
        ("plantCode", json!("")),
        ("code", json!("Manager")),
        ("code", json!("1st")),
        ("name", json!(null)),
        ("rank", json!(1000)),
        ("rank", json!(-1)),
        ("sortOrder", json!(1.5)),
        ("active", json!("true")),
        ("version", json!(null)),
    ] {
        let mut p = position();
        p[field] = value.clone();
        let err = parse_positions(&envelope(json!([p]))).expect_err(field);
        assert_eq!(err.field, Some(field), "{field}: {value}");
    }
}

#[test]
fn the_envelope_duplicates_and_limits_are_checked() {
    for body in [
        b"not json".to_vec(),
        json!({"success": false, "data": {"items": []}})
            .to_string()
            .into_bytes(),
        json!({"success": true, "data": {}})
            .to_string()
            .into_bytes(),
        json!({"success": true, "data": {"items": {}}})
            .to_string()
            .into_bytes(),
        json!([unit()]).to_string().into_bytes(),
    ] {
        assert_eq!(parse_units(&body), Err(Invalid::of("envelope")));
    }
    let twice = parse_units(&envelope(json!([unit(), unit()]))).expect_err("twice");
    assert_eq!((twice.reason, twice.index), ("duplicate", Some(1)));
    // The same code in another plant is another unit.
    let mut other = unit();
    other["plantCode"] = json!("P002");
    assert_eq!(
        parse_units(&envelope(json!([unit(), other])))
            .expect("two plants")
            .len(),
        2
    );
    let item = parse_units(&envelope(json!([unit(), "x"]))).expect_err("item");
    assert_eq!((item.reason, item.index), ("item", Some(1)));
    let many: Vec<Value> = (0..=MAX_ITEMS).map(|_| json!({})).collect();
    assert_eq!(
        parse_positions(&envelope(Value::Array(many))),
        Err(Invalid::of("too_many"))
    );
    assert_eq!(
        parse_units(&vec![b' '; MAX_BODY_BYTES + 1]),
        Err(Invalid::of("too_many"))
    );
}

#[test]
fn a_failure_names_the_list_field_and_index_never_a_value() {
    let e = invalid(
        "units",
        Invalid {
            reason: "field",
            field: Some("name"),
            index: Some(4),
        },
    );
    assert_eq!(e.code(), ErrorCode::OrgMasterInvalid);
    let text = e.to_string();
    for want in ["list=units", "reason=field", "field=name", "index=4"] {
        assert!(text.contains(want), "{text}");
    }
}
