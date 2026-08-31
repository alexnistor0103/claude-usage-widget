use crate::model::{ScopedWindow, Usage, Window};
use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// Turn the untyped response into our model. Any missing or unexpected field
/// yields `None`, so the row falls back to `unavailable` rather than a wrong
/// number (plan §3). Never deserialize straight into required fields.
pub fn parse_usage(raw: &Value) -> Option<Usage> {
    Some(Usage {
        five_hour: parse_window(raw.get("five_hour")?)?,
        seven_day: parse_window(raw.get("seven_day")?)?,
        scoped: parse_scoped(raw),
    })
}

/// A window needs a numeric `utilization`; a bad/missing `resets_at` is fine and
/// just leaves `resets_at: None`, not a parse failure.
fn parse_window(win: &Value) -> Option<Window> {
    let pct = win.get("utilization").and_then(Value::as_f64)?;
    let used_pct = pct.clamp(0.0, 100.0) as f32;
    let resets_at = parse_resets_at(win);
    Some(Window {
        used_pct,
        resets_at,
    })
}

fn parse_resets_at(v: &Value) -> Option<OffsetDateTime> {
    v.get("resets_at")
        .and_then(Value::as_str)
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
}

/// Per-model weekly windows are optional extras (plan §8 Q3): a missing or odd
/// `limits` array never fails the parse, and a bad entry is skipped.
fn parse_scoped(raw: &Value) -> Vec<ScopedWindow> {
    raw.get("limits")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(parse_scoped_entry).collect())
        .unwrap_or_default()
}

fn parse_scoped_entry(v: &Value) -> Option<ScopedWindow> {
    if v.get("kind").and_then(Value::as_str)? != "weekly_scoped" {
        return None;
    }
    let name = v
        .get("scope")?
        .get("model")?
        .get("display_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let used_pct = v.get("percent").and_then(Value::as_f64)?.clamp(0.0, 100.0) as f32;
    Some(ScopedWindow {
        name: name.into(),
        used_pct,
        resets_at: parse_resets_at(v),
        is_active: v.get("is_active").and_then(Value::as_bool).unwrap_or(false),
    })
}
