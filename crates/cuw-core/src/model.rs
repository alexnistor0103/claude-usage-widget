use time::OffsetDateTime;

/// One usage window: percent consumed and when it resets.
#[derive(Debug, Clone)]
pub struct Window {
    pub used_pct: f32,
    pub resets_at: Option<OffsetDateTime>,
}

/// A per-model weekly window from `limits[]` (plan §8 Q3). `is_active` marks
/// the currently binding limit, not whether the window exists.
#[derive(Debug, Clone)]
pub struct ScopedWindow {
    pub name: String,
    pub used_pct: f32,
    pub resets_at: Option<OffsetDateTime>,
    pub is_active: bool,
}

/// Both primary windows the endpoint reports per account, plus any scoped ones.
#[derive(Debug, Clone)]
pub struct Usage {
    pub five_hour: Window,
    pub seven_day: Window,
    pub scoped: Vec<ScopedWindow>,
}

/// What a row can show. Unknowns are states, never panics (plan §9).
#[derive(Debug, Clone)]
pub enum AccountState {
    Available(Usage),
    Unavailable,
    ReconnectNeeded,
}
