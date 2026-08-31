use cuw_core::parse_usage;
use serde_json::json;

const GOLDEN: &str = include_str!("fixtures/usage_ok.json");

#[test]
fn golden_payload_parses() {
    let raw = serde_json::from_str(GOLDEN).expect("fixture is valid JSON");
    let usage = parse_usage(&raw).expect("golden payload parses");

    assert_eq!(usage.five_hour.used_pct, 31.0);
    assert_eq!(usage.seven_day.used_pct, 14.0);
    assert!(usage.five_hour.resets_at.is_some());
    assert!(usage.seven_day.resets_at.is_some());
}

#[test]
fn golden_scoped_window_parses() {
    let raw = serde_json::from_str(GOLDEN).expect("fixture is valid JSON");
    let usage = parse_usage(&raw).expect("golden payload parses");

    assert_eq!(usage.scoped.len(), 1);
    let w = &usage.scoped[0];
    assert_eq!(w.name, "Fable");
    assert_eq!(w.used_pct, 0.0);
    assert!(w.resets_at.is_none());
    assert!(!w.is_active);
}

#[test]
fn limits_missing_is_empty_scoped() {
    let raw = json!({
        "five_hour": { "utilization": 31.0, "resets_at": null },
        "seven_day": { "utilization": 14.0, "resets_at": null },
    });
    let usage = parse_usage(&raw).expect("no limits array is still a parse");
    assert!(usage.scoped.is_empty());

    let raw = json!({
        "five_hour": { "utilization": 31.0 },
        "seven_day": { "utilization": 14.0 },
        "limits": "not-an-array",
    });
    let usage = parse_usage(&raw).expect("an odd limits value is ignored");
    assert!(usage.scoped.is_empty());
}

#[test]
fn scoped_entry_without_display_name_is_skipped() {
    let raw = json!({
        "five_hour": { "utilization": 31.0 },
        "seven_day": { "utilization": 14.0 },
        "limits": [
            { "kind": "weekly_scoped", "percent": 5, "scope": { "model": { "id": null, "display_name": null } } },
            { "kind": "weekly_scoped", "percent": 5, "scope": { "model": { "display_name": "" } } },
            { "kind": "weekly_scoped", "percent": 5, "scope": null },
            { "kind": "weekly_all", "percent": 14, "scope": { "model": { "display_name": "Nope" } } },
            { "kind": "weekly_scoped", "percent": 7, "scope": { "model": { "display_name": "Kept" } }, "is_active": true },
        ],
    });
    let usage = parse_usage(&raw).expect("bad entries are skipped, not fatal");
    assert_eq!(usage.scoped.len(), 1);
    assert_eq!(usage.scoped[0].name, "Kept");
    assert_eq!(usage.scoped[0].used_pct, 7.0);
    assert!(usage.scoped[0].is_active);
}

#[test]
fn scoped_percent_clamped() {
    let raw = json!({
        "five_hour": { "utilization": 31.0 },
        "seven_day": { "utilization": 14.0 },
        "limits": [
            { "kind": "weekly_scoped", "percent": 250.0, "scope": { "model": { "display_name": "High" } } },
            { "kind": "weekly_scoped", "percent": -5, "scope": { "model": { "display_name": "Low" } } },
            { "kind": "weekly_scoped", "percent": "9", "scope": { "model": { "display_name": "Str" } } },
        ],
    });
    let usage = parse_usage(&raw).expect("clamped, not rejected");
    assert_eq!(usage.scoped.len(), 2);
    assert_eq!(usage.scoped[0].used_pct, 100.0);
    assert_eq!(usage.scoped[1].used_pct, 0.0);
}

#[test]
fn empty_object_is_none() {
    assert!(parse_usage(&json!({})).is_none());
}

#[test]
fn utilization_as_string_is_none() {
    let raw = json!({
        "five_hour": { "utilization": "31.0", "resets_at": null },
        "seven_day": { "utilization": 14.0, "resets_at": null },
    });
    assert!(parse_usage(&raw).is_none());
}

#[test]
fn missing_five_hour_is_none() {
    let raw = json!({ "seven_day": { "utilization": 14.0 } });
    assert!(parse_usage(&raw).is_none());
}

#[test]
fn missing_seven_day_is_none() {
    let raw = json!({ "five_hour": { "utilization": 31.0 } });
    assert!(parse_usage(&raw).is_none());
}

#[test]
fn missing_utilization_is_none() {
    let raw = json!({
        "five_hour": { "resets_at": "2026-08-29T10:40:00+00:00" },
        "seven_day": { "utilization": 14.0 },
    });
    assert!(parse_usage(&raw).is_none());
}

#[test]
fn extra_unknown_fields_still_parse() {
    let raw = json!({
        "five_hour": { "utilization": 31.0, "resets_at": null, "mystery": 7 },
        "seven_day": { "utilization": 14.0, "resets_at": null },
        "nimbus_quill": null,
        "spend": { "percent": 0 },
    });
    let usage = parse_usage(&raw).expect("unknown fields are ignored");
    assert_eq!(usage.five_hour.used_pct, 31.0);
    assert!(usage.five_hour.resets_at.is_none());
}

#[test]
fn bad_timestamp_yields_none_reset_not_failure() {
    let raw = json!({
        "five_hour": { "utilization": 31.0, "resets_at": "not-a-date" },
        "seven_day": { "utilization": 14.0, "resets_at": 12345 },
    });
    let usage = parse_usage(&raw).expect("a bad timestamp is not a parse failure");
    assert!(usage.five_hour.resets_at.is_none());
    assert!(usage.seven_day.resets_at.is_none());
}

#[test]
fn out_of_range_utilization_is_clamped() {
    let raw = json!({
        "five_hour": { "utilization": 250.0 },
        "seven_day": { "utilization": -5.0 },
    });
    let usage = parse_usage(&raw).expect("clamped, not rejected");
    assert_eq!(usage.five_hour.used_pct, 100.0);
    assert_eq!(usage.seven_day.used_pct, 0.0);
}
