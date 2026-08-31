use std::time::Duration;

use cuw_core::{error_code, parse_refresh, Credential, RefreshError, Refreshed};
use serde_json::{json, Value};
use time::OffsetDateTime;

const GOLDEN: &str = include_str!("fixtures/token_ok.json");

const FAKE_ACCESS: &str = "sk-ant-oat01-FAKEFAKEFAKEFAKEFAKEFAKEFAKE0001";
const FAKE_REFRESH: &str = "sk-ant-ort01-FAKEFAKEFAKEFAKEFAKEFAKEFAKE0002";
const NEW_ACCESS: &str = "sk-ant-oat01-FAKEFAKEFAKEFAKEFAKEFAKEFAKE0003";
const NEW_REFRESH: &str = "sk-ant-ort01-FAKEFAKEFAKEFAKEFAKEFAKEFAKE0004";

fn golden() -> Value {
    serde_json::from_str(GOLDEN).expect("fixture is valid JSON")
}

fn credential() -> Credential {
    Credential {
        v: 1,
        access_token: FAKE_ACCESS.into(),
        refresh_token: FAKE_REFRESH.into(),
        expires_at: 1_756_577_403,
        scopes: vec!["user:inference".into(), "user:profile".into()],
    }
}

fn refreshed(refresh_token: Option<&str>) -> Refreshed {
    Refreshed {
        access_token: NEW_ACCESS.into(),
        refresh_token: refresh_token.map(str::to_owned),
        expires_in: Duration::from_secs(28_800),
        scopes: None,
    }
}

#[test]
fn golden_parses() {
    let r = parse_refresh(&golden()).expect("golden payload parses");
    assert_eq!(r.access_token, FAKE_ACCESS);
    assert_eq!(r.refresh_token.as_deref(), Some(FAKE_REFRESH));
    assert_eq!(r.expires_in, Duration::from_secs(28_800));
    assert_eq!(
        r.scopes,
        Some(vec!["user:inference".to_owned(), "user:profile".to_owned()])
    );
}

#[test]
fn empty_object_is_none() {
    assert!(parse_refresh(&json!({})).is_none());
}

#[test]
fn access_token_missing_is_none() {
    let mut raw = golden();
    raw.as_object_mut().unwrap().remove("access_token");
    assert!(parse_refresh(&raw).is_none());
}

#[test]
fn access_token_empty_is_none() {
    let mut raw = golden();
    raw["access_token"] = json!("");
    assert!(parse_refresh(&raw).is_none());
}

#[test]
fn expires_in_as_string_is_none() {
    let mut raw = golden();
    raw["expires_in"] = json!("28800");
    assert!(parse_refresh(&raw).is_none());
}

#[test]
fn expires_in_missing_is_none() {
    let mut raw = golden();
    raw.as_object_mut().unwrap().remove("expires_in");
    assert!(parse_refresh(&raw).is_none());
}

#[test]
fn expires_in_negative_is_none() {
    let mut raw = golden();
    raw["expires_in"] = json!(-1);
    assert!(parse_refresh(&raw).is_none());
}

#[test]
fn expires_in_huge_is_clamped() {
    let mut raw = golden();
    raw["expires_in"] = json!(1e12);
    let r = parse_refresh(&raw).expect("clamped, not rejected");
    assert_eq!(r.expires_in, Duration::from_secs(2_592_000));
}

#[test]
fn out_of_range_expires_in_is_clamped() {
    let mut raw = golden();
    raw["expires_in"] = json!(1);
    let r = parse_refresh(&raw).expect("clamped, not rejected");
    assert_eq!(r.expires_in, Duration::from_secs(60));
}

#[test]
fn missing_refresh_token_is_ok_none() {
    let mut raw = golden();
    raw.as_object_mut().unwrap().remove("refresh_token");
    let r = parse_refresh(&raw).expect("refresh_token is optional");
    assert!(r.refresh_token.is_none());

    raw["refresh_token"] = json!("");
    let r = parse_refresh(&raw).expect("an empty refresh_token is treated as absent");
    assert!(r.refresh_token.is_none());
}

#[test]
fn scopes_array_form_parses() {
    let raw = json!({
        "access_token": FAKE_ACCESS,
        "expires_in": 3600,
        "scopes": ["user:profile", 7, "user:inference"],
    });
    let r = parse_refresh(&raw).expect("array form parses");
    assert_eq!(
        r.scopes,
        Some(vec!["user:profile".to_owned(), "user:inference".to_owned()])
    );
}

#[test]
fn no_scope_field_is_none() {
    let raw = json!({ "access_token": FAKE_ACCESS, "expires_in": 3600 });
    let r = parse_refresh(&raw).expect("scopes are optional");
    assert!(r.scopes.is_none());
}

#[test]
fn extra_fields_ignored() {
    let mut raw = golden();
    raw["organization"] = json!({ "uuid": "00000000-0000-0000-0000-000000000000" });
    raw["mystery"] = json!(null);
    let r = parse_refresh(&raw).expect("unknown fields are ignored");
    assert_eq!(r.expires_in, Duration::from_secs(28_800));
}

#[test]
fn refreshed_debug_never_contains_tokens() {
    let r = parse_refresh(&golden()).expect("golden payload parses");
    let dbg = format!("{r:?}");
    assert!(!dbg.contains(FAKE_ACCESS));
    assert!(!dbg.contains(FAKE_REFRESH));
    assert!(!dbg.contains("FAKE"));
    assert!(dbg.contains("expires_in"));
}

#[test]
fn credential_debug_never_contains_tokens() {
    let dbg = format!("{:?}", credential());
    assert!(!dbg.contains(FAKE_ACCESS));
    assert!(!dbg.contains(FAKE_REFRESH));
    assert!(!dbg.contains("FAKE"));
    assert!(dbg.contains("user:profile"));
}

#[test]
fn credential_rotated_keeps_old_refresh_token_when_absent() {
    let now = OffsetDateTime::from_unix_timestamp(1_756_600_000).unwrap();
    let c = credential().rotated(&refreshed(None), now);
    assert_eq!(c.access_token, NEW_ACCESS);
    assert_eq!(c.refresh_token, FAKE_REFRESH);
    assert_eq!(c.scopes, credential().scopes);
    assert_eq!(c.v, 1);

    let c = credential().rotated(&refreshed(Some(NEW_REFRESH)), now);
    assert_eq!(c.refresh_token, NEW_REFRESH);
}

#[test]
fn rotated_sets_expires_at_now_plus_expires_in() {
    let now = OffsetDateTime::from_unix_timestamp(1_756_600_000).unwrap();
    let c = credential().rotated(&refreshed(None), now);
    assert_eq!(c.expires_at, 1_756_600_000 + 28_800);
    assert_eq!(c.expires_at_utc(), Some(now + Duration::from_secs(28_800)));
}

#[test]
fn rotated_replaces_scopes_when_present() {
    let now = OffsetDateTime::from_unix_timestamp(1_756_600_000).unwrap();
    let mut r = refreshed(None);
    r.scopes = Some(vec!["user:inference".into()]);
    let c = credential().rotated(&r, now);
    assert_eq!(c.scopes, vec!["user:inference".to_owned()]);
    assert!(!c.has_usage_scope());
}

#[test]
fn credential_has_usage_scope() {
    assert!(credential().has_usage_scope());
    let mut c = credential();
    c.scopes = vec!["user:inference".into()];
    assert!(!c.has_usage_scope());
    c.scopes.clear();
    assert!(!c.has_usage_scope());
}

#[test]
fn credential_roundtrips_serde_with_default_v() {
    let without_v = json!({
        "access_token": FAKE_ACCESS,
        "refresh_token": FAKE_REFRESH,
        "expires_at": 1_756_577_403,
    })
    .to_string();
    let c: Credential = serde_json::from_str(&without_v).expect("v and scopes default");
    assert_eq!(c.v, 1);
    assert!(c.scopes.is_empty());
    assert_eq!(c.expires_at, 1_756_577_403);

    let text = serde_json::to_string(&credential()).expect("serializes");
    let back: Credential = serde_json::from_str(&text).expect("roundtrips");
    assert_eq!(back.v, 1);
    assert_eq!(back.access_token, FAKE_ACCESS);
    assert_eq!(back.refresh_token, FAKE_REFRESH);
    assert_eq!(back.expires_at, credential().expires_at);
    assert_eq!(back.scopes, credential().scopes);
}

#[test]
fn error_code_whitelist() {
    assert_eq!(
        error_code(br#"{"error":"invalid_grant"}"#).as_deref(),
        Some("invalid_grant")
    );
    assert_eq!(
        error_code(br#"{"error":"invalid_token"}"#).as_deref(),
        Some("invalid_token")
    );
    assert_eq!(
        error_code(br#"{"error":"unauthorized_client"}"#).as_deref(),
        Some("unauthorized_client")
    );
    assert!(error_code(br#"{"error":"invalid_request"}"#).is_none());
    assert!(error_code(br#"{"error":{"type":"invalid_grant"}}"#).is_none());
    assert!(error_code(b"{}").is_none());
    assert!(error_code(b"not json").is_none());
    assert!(error_code(b"").is_none());
}

#[test]
fn error_code_never_returns_body_content() {
    let body = json!({
        "error": "invalid_grant",
        "error_description": format!("bad token {FAKE_REFRESH}"),
        "refresh_token": FAKE_REFRESH,
    })
    .to_string();
    assert_eq!(
        error_code(body.as_bytes()).as_deref(),
        Some("invalid_grant")
    );
}

#[test]
fn rejected_and_contract_display_have_no_body() {
    for e in [RefreshError::Rejected(400), RefreshError::Contract(403)] {
        let display = format!("{e}");
        let debug = format!("{e:?}");
        assert!(!display.contains("sk-ant"));
        assert!(!debug.contains("sk-ant"));
        assert!(!display.contains("FAKE"));
        assert!(!debug.contains("FAKE"));
    }
    assert_eq!(
        RefreshError::Rejected(400).to_string(),
        "refresh rejected (400)"
    );
    assert_eq!(
        RefreshError::Contract(403).to_string(),
        "token endpoint contract changed (403)"
    );
}
