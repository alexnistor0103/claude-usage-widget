//! The injectable poll-update logic (M2.2, M1b.5). `apply_fetch` maps one fetch
//! result onto the next display state and the next sleep decision;
//! `apply_refresh` does the same for one token-endpoint result. Both are pure
//! over their inputs so fakes can drive them in tests without a network. The
//! task loop that calls them lives in `http`.

use cuw_core::client::{FetchError, RawResponse};
use cuw_core::model::{AccountState, Usage};
use cuw_core::parse_usage;
use cuw_core::{Credential, RefreshError, Refreshed};
use time::{Duration, OffsetDateTime};

/// Never show a transient-error row as fresh forever: after this long without a
/// success, a transient error downgrades to `unavailable` (plan §3).
const STALE_AFTER: Duration = Duration::minutes(10);

/// Refresh this long before the access token expires, so a slow token endpoint
/// never leaves a poll cycle holding a dead token (plan §4).
pub const REFRESH_LEAD: Duration = Duration::minutes(5);

/// A transient outage must never produce two token POSTs in one minute (plan §4).
pub const REFRESH_BACKOFF_FLOOR: std::time::Duration = std::time::Duration::from_secs(60);

/// Why a row is (or is not) reconnecting, as the wire reports it (plan §8 Q6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshStatus {
    #[default]
    Ok,
    Backoff,
    Rejected,
}

impl RefreshStatus {
    pub fn as_wire(self) -> &'static str {
        match self {
            RefreshStatus::Ok => "ok",
            RefreshStatus::Backoff => "backoff",
            RefreshStatus::Rejected => "rejected",
        }
    }

    fn tag(self) -> u8 {
        match self {
            RefreshStatus::Ok => 0,
            RefreshStatus::Backoff => 1,
            RefreshStatus::Rejected => 2,
        }
    }
}

/// Per-account poll bookkeeping, distinct from the shared display `Row`.
#[derive(Debug, Default)]
pub struct PollState {
    /// Usage-endpoint backoff counter (429/5xx/transport).
    pub attempt: u32,
    /// Token-endpoint backoff counter; separate so neither wipes the other.
    pub refresh_attempt: u32,
    pub last_success: Option<OffsetDateTime>,
    /// Last-good numbers are being held without a fresh 200.
    pub stale: bool,
    /// Set by a 401 on a token that was not just refreshed.
    pub force_refresh: bool,
    /// Between a refresh and the next applied fetch: a 401 now is final.
    pub just_refreshed: bool,
    pub refresh_status: RefreshStatus,
    pub refreshed_at: Option<OffsetDateTime>,
    /// Keyring write failed; retry next cycle.
    pub persist_pending: bool,
    /// Error logged once per failure streak.
    pub persist_logged: bool,
    /// One forced refresh per 429 outage (plan §8 Q12).
    pub forced_for_429: bool,
}

/// What the loop should do after applying a result.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Success or a clean 200-with-bad-shape: sleep the normal jittered interval.
    Normal,
    /// Transient error: sleep `poller::backoff(attempt)`.
    Backoff,
    /// Unauthorized: the token is dead; the task ends until a reconnect.
    Idle,
}

/// What the loop should do after applying one token-endpoint result.
#[derive(Debug)]
pub enum RefreshStep {
    Rotated(Credential),
    Reconnect,
    Backoff,
}

/// Refresh before the lead time, or because a 401 asked for it. An unparseable
/// expiry is treated as expired: refreshing costs one POST, guessing costs a row.
pub fn needs_refresh(cred: &Credential, poll: &PollState, now: OffsetDateTime) -> bool {
    poll.force_refresh || cred.expires_at_utc().is_none_or(|t| t - now < REFRESH_LEAD)
}

/// Token-endpoint backoff: the usage curve, floored so two POSTs can never land
/// inside one minute (plan §4).
pub fn refresh_backoff(attempt: u32) -> std::time::Duration {
    cuw_core::poller::backoff(attempt).max(REFRESH_BACKOFF_FLOOR)
}

/// True when sleeping `sleep` from `now` would carry last-good numbers past
/// `STALE_AFTER` (no success yet → true). The loop uses it to downgrade *before*
/// a long sleep rather than after it, so nothing aged is ever rendered as fresh.
pub fn sleep_crosses_stale(
    poll: &PollState,
    now: OffsetDateTime,
    sleep: std::time::Duration,
) -> bool {
    let Some(last) = poll.last_success else {
        return true;
    };
    let Ok(sleep) = Duration::try_from(sleep) else {
        return true;
    };
    match now.checked_add(sleep) {
        Some(then) => then - last > STALE_AFTER,
        // An absurd sleep is past any staleness horizon.
        None => true,
    }
}

/// Fold one token-endpoint result into the poll bookkeeping. A rejected refresh
/// is terminal; anything else backs off on its own counter so a contract break
/// never turns into a reconnect loop the user cannot fix (plan §4).
pub fn apply_refresh(
    result: Result<Refreshed, RefreshError>,
    cred: &Credential,
    poll: &mut PollState,
    now: OffsetDateTime,
) -> RefreshStep {
    match result {
        Ok(r) => {
            poll.force_refresh = false;
            poll.just_refreshed = true;
            poll.refresh_attempt = 0;
            poll.refresh_status = RefreshStatus::Ok;
            poll.refreshed_at = Some(now);
            RefreshStep::Rotated(cred.rotated(&r, now))
        }
        Err(RefreshError::Rejected(_)) => {
            poll.refresh_status = RefreshStatus::Rejected;
            RefreshStep::Reconnect
        }
        Err(e) => {
            // The version gate (plan §3): a shape or status we did not expect is
            // worth a loud line. `%e` only — no response body ever reaches a log.
            if matches!(e, RefreshError::Contract(_) | RefreshError::BadShape) {
                tracing::error!(error = %e, "token endpoint contract changed");
            }
            poll.refresh_attempt = poll.refresh_attempt.saturating_add(1);
            poll.refresh_status = RefreshStatus::Backoff;
            poll.stale = true;
            RefreshStep::Backoff
        }
    }
}

/// Keep the last-good state until it goes stale, then downgrade (plan §3).
/// Returns the next state and whether it was downgraded.
fn keep_until_stale(
    current: &AccountState,
    poll: &PollState,
    now: OffsetDateTime,
) -> (AccountState, bool) {
    let aged = match poll.last_success {
        Some(t) => now - t > STALE_AFTER,
        None => true,
    };
    if aged {
        (AccountState::Unavailable, true)
    } else {
        (current.clone(), false)
    }
}

/// Fold one fetch result into the next state. Success resets the backoff counter
/// and records the success time. A first `Unauthorized` asks for a refresh and
/// keeps polling; one straight after a refresh is terminal. `Forbidden` needs a
/// differently scoped token, so it idles. A transient error keeps the last-good
/// state (never a wrong number) until it goes stale, then downgrades to
/// `Unavailable`. A 200 that fails to parse is `Unavailable` but keeps polling.
pub fn apply_fetch(
    result: Result<RawResponse, FetchError>,
    current: &AccountState,
    poll: &mut PollState,
    now: OffsetDateTime,
) -> (AccountState, Step) {
    let out = match result {
        Ok(raw) => match parse_usage(&raw) {
            Some(usage) => {
                poll.attempt = 0;
                poll.last_success = Some(now);
                poll.stale = false;
                poll.forced_for_429 = false;
                (AccountState::Available(usage), Step::Normal)
            }
            // The request itself succeeded, so reset backoff; the shape did not.
            None => {
                poll.attempt = 0;
                (AccountState::Unavailable, Step::Normal)
            }
        },
        Err(FetchError::Unauthorized) => {
            if poll.just_refreshed {
                // A 401 on a token minted moments ago will not self-heal (plan §4).
                (AccountState::ReconnectNeeded, Step::Idle)
            } else {
                // Ask for a refresh and sleep the normal interval: the backoff
                // counter belongs to the usage endpoint, and the 60–120 s sleep
                // already honours one request per account per minute.
                let (next, _) = keep_until_stale(current, poll, now);
                poll.force_refresh = true;
                poll.stale = true;
                (next, Step::Normal)
            }
        }
        // A 403 needs a differently scoped token; nothing here can fix it.
        Err(FetchError::Forbidden(_)) => (AccountState::ReconnectNeeded, Step::Idle),
        // RateLimited / Server / Transport are all transient.
        Err(e) => {
            poll.attempt = poll.attempt.saturating_add(1);
            let (next, downgraded) = keep_until_stale(current, poll, now);
            poll.stale = !downgraded;
            // The endpoint may answer 429 rather than 401 to a dead token
            // (plan §8 Q12): force exactly one refresh per outage.
            let no_refresh_since_success = poll
                .refreshed_at
                .is_none_or(|r| poll.last_success.is_none_or(|s| r < s));
            if matches!(e, FetchError::RateLimited)
                && poll.attempt >= 2
                && !poll.forced_for_429
                && no_refresh_since_success
            {
                poll.force_refresh = true;
                poll.forced_for_429 = true;
            }
            (next, Step::Backoff)
        }
    };
    // The "just refreshed" window closes at the first applied fetch, whatever
    // it was.
    poll.just_refreshed = false;
    out
}

/// A compact identity for everything a row displays, used to decide whether a
/// change is worth pushing over SSE. Reset-time drift alone does not trigger a
/// frame.
///
/// The loop must build one side from the values the `Row` currently **stores**
/// and the other from the post-apply values. Taking both sides from the
/// post-apply `PollState` would hide a stale-only or refresh-only flip, and the
/// overlay would keep rendering aged numbers as fresh (plan §3).
pub fn fingerprint(
    state: &AccountState,
    refresh: RefreshStatus,
    stale: bool,
    persist_pending: bool,
) -> (u8, i64, i64, u8, bool, bool, u64) {
    let (tag, five, seven, scoped) = match state {
        AccountState::Unavailable => (0, 0, 0, 0),
        AccountState::ReconnectNeeded => (1, 0, 0, 0),
        AccountState::Available(u) => (
            2,
            u.five_hour.used_pct.round() as i64,
            u.seven_day.used_pct.round() as i64,
            scoped_hash(u),
        ),
    };
    (
        tag,
        five,
        seven,
        refresh.tag(),
        stale,
        persist_pending,
        scoped,
    )
}

/// Hash the scoped windows down to what the overlay actually renders, so a
/// reset-time drift inside one does not push a frame.
fn scoped_hash(u: &Usage) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for s in &u.scoped {
        s.name.hash(&mut h);
        (s.used_pct.round() as i64).hash(&mut h);
        s.is_active.hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cuw_core::model::{ScopedWindow, Usage, Window};
    use serde_json::json;

    const FAKE_ACCESS: &str = "sk-ant-oat01-FAKE00000000000000000000000001";
    const FAKE_REFRESH: &str = "sk-ant-ort01-FAKE00000000000000000000000002";
    const FAKE_ACCESS_2: &str = "sk-ant-oat01-FAKE00000000000000000000000003";
    const FAKE_REFRESH_2: &str = "sk-ant-ort01-FAKE00000000000000000000000004";

    fn usage(five: f32, seven: f32) -> Usage {
        Usage {
            five_hour: Window {
                used_pct: five,
                resets_at: None,
            },
            seven_day: Window {
                used_pct: seven,
                resets_at: None,
            },
            scoped: Vec::new(),
        }
    }

    fn cred(expires_at: i64) -> Credential {
        Credential {
            v: 1,
            access_token: FAKE_ACCESS.into(),
            refresh_token: FAKE_REFRESH.into(),
            expires_at,
            scopes: vec!["user:inference".into(), "user:profile".into()],
        }
    }

    fn refreshed(refresh_token: Option<&str>) -> Refreshed {
        Refreshed {
            access_token: FAKE_ACCESS_2.into(),
            refresh_token: refresh_token.map(Into::into),
            expires_in: std::time::Duration::from_secs(28_800),
            scopes: None,
        }
    }

    #[test]
    fn unauthorized_maps_to_reconnect_needed() {
        // Only after a refresh: a first 401 asks for one instead (plan §4).
        let mut poll = PollState {
            just_refreshed: true,
            ..PollState::default()
        };
        let (state, step) = apply_fetch(
            Err(FetchError::Unauthorized),
            &AccountState::Unavailable,
            &mut poll,
            OffsetDateTime::now_utc(),
        );
        assert!(matches!(state, AccountState::ReconnectNeeded));
        assert_eq!(step, Step::Idle);
    }

    #[test]
    fn forbidden_is_reconnect_needed_and_idles() {
        let mut poll = PollState::default();
        let (state, step) = apply_fetch(
            Err(FetchError::Forbidden("scope".into())),
            &AccountState::Unavailable,
            &mut poll,
            OffsetDateTime::now_utc(),
        );
        assert!(matches!(state, AccountState::ReconnectNeeded));
        assert_eq!(step, Step::Idle);
    }

    #[test]
    fn first_401_forces_refresh_and_sleeps_normal() {
        let now = OffsetDateTime::now_utc();
        let mut poll = PollState {
            last_success: Some(now),
            ..PollState::default()
        };
        let current = AccountState::Available(usage(31.0, 14.0));
        let (state, step) = apply_fetch(Err(FetchError::Unauthorized), &current, &mut poll, now);
        assert!(matches!(state, AccountState::Available(_)));
        assert_eq!(step, Step::Normal);
        assert!(poll.force_refresh);
        // The usage-endpoint backoff counter is not the token endpoint's.
        assert_eq!(poll.attempt, 0);
    }

    #[test]
    fn unauthorized_401_keeps_numbers_but_marks_stale() {
        let now = OffsetDateTime::now_utc();
        let mut poll = PollState {
            last_success: Some(now),
            ..PollState::default()
        };
        let current = AccountState::Available(usage(31.0, 14.0));
        let (state, _) = apply_fetch(Err(FetchError::Unauthorized), &current, &mut poll, now);
        match state {
            AccountState::Available(u) => assert_eq!(u.five_hour.used_pct, 31.0),
            other => panic!("expected last-good Available, got {other:?}"),
        }
        assert!(poll.stale, "kept numbers must be flagged stale");
    }

    #[test]
    fn unauthorized_401_after_refresh_is_reconnect() {
        let now = OffsetDateTime::now_utc();
        let mut poll = PollState {
            last_success: Some(now),
            just_refreshed: true,
            ..PollState::default()
        };
        let current = AccountState::Available(usage(31.0, 14.0));
        let (state, step) = apply_fetch(Err(FetchError::Unauthorized), &current, &mut poll, now);
        assert!(matches!(state, AccountState::ReconnectNeeded));
        assert_eq!(step, Step::Idle);
        assert!(!poll.just_refreshed, "the window closes after one fetch");
    }

    #[test]
    fn refresh_rejected_is_reconnect() {
        let mut poll = PollState::default();
        let step = apply_refresh(
            Err(RefreshError::Rejected(400)),
            &cred(0),
            &mut poll,
            OffsetDateTime::now_utc(),
        );
        assert!(matches!(step, RefreshStep::Reconnect));
        assert_eq!(poll.refresh_status, RefreshStatus::Rejected);
        assert_eq!(poll.refresh_attempt, 0);
    }

    #[test]
    fn refresh_contract_is_backoff_not_reconnect() {
        let mut poll = PollState::default();
        let step = apply_refresh(
            Err(RefreshError::Contract(400)),
            &cred(0),
            &mut poll,
            OffsetDateTime::now_utc(),
        );
        assert!(matches!(step, RefreshStep::Backoff));
        assert_eq!(poll.refresh_status, RefreshStatus::Backoff);
    }

    #[test]
    fn refresh_transient_is_backoff_and_bumps_refresh_attempt_only() {
        let mut poll = PollState {
            attempt: 3,
            refresh_attempt: 1,
            ..PollState::default()
        };
        let step = apply_refresh(
            Err(RefreshError::Server(503)),
            &cred(0),
            &mut poll,
            OffsetDateTime::now_utc(),
        );
        assert!(matches!(step, RefreshStep::Backoff));
        assert_eq!(poll.refresh_attempt, 2);
        assert_eq!(poll.attempt, 3, "the usage counter must not move");
    }

    #[test]
    fn refresh_backoff_keeps_numbers_but_marks_stale() {
        let mut poll = PollState::default();
        let _ = apply_refresh(
            Err(RefreshError::RateLimited),
            &cred(0),
            &mut poll,
            OffsetDateTime::now_utc(),
        );
        assert!(poll.stale, "numbers held across a refresh outage are stale");
    }

    #[test]
    fn refresh_backoff_floor_is_60s() {
        assert!(refresh_backoff(0) >= REFRESH_BACKOFF_FLOOR);
        assert!(refresh_backoff(1) >= REFRESH_BACKOFF_FLOOR);
        // The curve still wins once it passes the floor.
        assert_eq!(refresh_backoff(5), cuw_core::poller::backoff(5));
    }

    #[test]
    fn refresh_ok_does_not_reset_usage_attempt() {
        let mut poll = PollState {
            attempt: 3,
            ..PollState::default()
        };
        let _ = apply_refresh(
            Ok(refreshed(None)),
            &cred(0),
            &mut poll,
            OffsetDateTime::now_utc(),
        );
        assert_eq!(poll.attempt, 3);
        assert_eq!(poll.refresh_attempt, 0);
    }

    #[test]
    fn refresh_ok_rotates_and_flags() {
        let now = OffsetDateTime::now_utc();
        let mut poll = PollState {
            force_refresh: true,
            refresh_attempt: 4,
            refresh_status: RefreshStatus::Backoff,
            ..PollState::default()
        };
        let step = apply_refresh(
            Ok(refreshed(Some(FAKE_REFRESH_2))),
            &cred(0),
            &mut poll,
            now,
        );
        match step {
            RefreshStep::Rotated(next) => {
                assert_eq!(next.access_token, FAKE_ACCESS_2);
                assert_eq!(next.refresh_token, FAKE_REFRESH_2);
                assert_eq!(next.expires_at, now.unix_timestamp() + 28_800);
            }
            other => panic!("expected Rotated, got {other:?}"),
        }
        assert!(!poll.force_refresh);
        assert!(poll.just_refreshed);
        assert_eq!(poll.refresh_attempt, 0);
        assert_eq!(poll.refresh_status, RefreshStatus::Ok);
        assert_eq!(poll.refreshed_at, Some(now));
    }

    #[test]
    fn rotation_keeps_refresh_token_when_absent() {
        let now = OffsetDateTime::now_utc();
        let mut poll = PollState::default();
        let step = apply_refresh(Ok(refreshed(None)), &cred(0), &mut poll, now);
        match step {
            RefreshStep::Rotated(next) => {
                assert_eq!(next.refresh_token, FAKE_REFRESH);
                // Scopes carry over too when the response omits them.
                assert_eq!(next.scopes, vec!["user:inference", "user:profile"]);
            }
            other => panic!("expected Rotated, got {other:?}"),
        }
    }

    #[test]
    fn needs_refresh_within_lead() {
        let now = OffsetDateTime::now_utc();
        let poll = PollState::default();
        let soon = cred((now + Duration::minutes(4)).unix_timestamp());
        assert!(needs_refresh(&soon, &poll, now));

        let later = cred((now + Duration::minutes(10)).unix_timestamp());
        assert!(!needs_refresh(&later, &poll, now));

        let forced = PollState {
            force_refresh: true,
            ..PollState::default()
        };
        assert!(needs_refresh(&later, &forced, now));

        // An expiry that is not a real instant is treated as expired.
        assert!(needs_refresh(&cred(i64::MAX), &poll, now));
    }

    #[test]
    fn second_429_forces_one_refresh_per_outage() {
        let now = OffsetDateTime::now_utc();
        let mut poll = PollState {
            last_success: Some(now),
            ..PollState::default()
        };
        let current = AccountState::Available(usage(31.0, 14.0));

        apply_fetch(Err(FetchError::RateLimited), &current, &mut poll, now);
        assert_eq!(poll.attempt, 1);
        assert!(!poll.force_refresh, "one 429 is just backoff");

        apply_fetch(Err(FetchError::RateLimited), &current, &mut poll, now);
        assert!(poll.force_refresh);
        assert!(poll.forced_for_429);

        // A third 429 must not buy a second token POST.
        poll.force_refresh = false;
        apply_fetch(Err(FetchError::RateLimited), &current, &mut poll, now);
        assert!(!poll.force_refresh);

        // A success ends the outage and re-arms the one forced refresh.
        let raw = json!({
            "five_hour": { "utilization": 31.0, "resets_at": null },
            "seven_day": { "utilization": 14.0, "resets_at": null }
        });
        apply_fetch(Ok(raw), &current, &mut poll, now);
        assert!(!poll.forced_for_429);
    }

    #[test]
    fn sleep_crosses_stale_at_boundary() {
        let now = OffsetDateTime::now_utc();
        let poll = PollState {
            last_success: Some(now - Duration::minutes(9)),
            ..PollState::default()
        };
        assert!(sleep_crosses_stale(
            &poll,
            now,
            std::time::Duration::from_secs(120)
        ));
        assert!(!sleep_crosses_stale(
            &poll,
            now,
            std::time::Duration::from_secs(30)
        ));

        let never = PollState::default();
        assert!(sleep_crosses_stale(
            &never,
            now,
            std::time::Duration::from_secs(1)
        ));
    }

    #[test]
    fn stale_flag_set_on_kept_numbers_and_cleared_on_success() {
        let now = OffsetDateTime::now_utc();
        let mut poll = PollState {
            last_success: Some(now),
            ..PollState::default()
        };
        let current = AccountState::Available(usage(31.0, 14.0));

        apply_fetch(Err(FetchError::Server(503)), &current, &mut poll, now);
        assert!(poll.stale);

        let raw = json!({
            "five_hour": { "utilization": 31.0, "resets_at": null },
            "seven_day": { "utilization": 14.0, "resets_at": null }
        });
        apply_fetch(Ok(raw), &current, &mut poll, now);
        assert!(!poll.stale);

        // Once downgraded there are no kept numbers left to be stale about.
        let mut old = PollState {
            last_success: Some(now - Duration::minutes(11)),
            stale: true,
            ..PollState::default()
        };
        let (state, _) = apply_fetch(Err(FetchError::Server(503)), &current, &mut old, now);
        assert!(matches!(state, AccountState::Unavailable));
        assert!(!old.stale);
    }

    #[test]
    fn fingerprint_changes_on_refresh_status_stale_persist_and_scoped() {
        let base = AccountState::Available(usage(31.0, 14.0));
        let f = fingerprint(&base, RefreshStatus::Ok, false, false);

        assert_ne!(f, fingerprint(&base, RefreshStatus::Backoff, false, false));
        assert_ne!(f, fingerprint(&base, RefreshStatus::Rejected, false, false));
        assert_ne!(f, fingerprint(&base, RefreshStatus::Ok, true, false));
        assert_ne!(f, fingerprint(&base, RefreshStatus::Ok, false, true));

        let mut u = usage(31.0, 14.0);
        u.scoped.push(ScopedWindow {
            name: "Fable".into(),
            used_pct: 12.0,
            resets_at: None,
            is_active: false,
        });
        let with_scoped = AccountState::Available(u.clone());
        assert_ne!(
            f,
            fingerprint(&with_scoped, RefreshStatus::Ok, false, false)
        );

        // A drift in a scoped window's reset time alone is not worth a frame.
        let mut drifted = u.clone();
        drifted.scoped[0].resets_at = Some(OffsetDateTime::now_utc());
        assert_eq!(
            fingerprint(&with_scoped, RefreshStatus::Ok, false, false),
            fingerprint(
                &AccountState::Available(drifted),
                RefreshStatus::Ok,
                false,
                false
            )
        );

        // ...but a change in which scoped window binds is.
        let mut active = u;
        active.scoped[0].is_active = true;
        assert_ne!(
            fingerprint(&with_scoped, RefreshStatus::Ok, false, false),
            fingerprint(
                &AccountState::Available(active),
                RefreshStatus::Ok,
                false,
                false
            )
        );
    }

    #[test]
    fn rate_limited_after_available_keeps_last_good_numbers() {
        let now = OffsetDateTime::now_utc();
        let mut poll = PollState {
            last_success: Some(now),
            ..PollState::default()
        };
        let current = AccountState::Available(usage(31.0, 14.0));
        let (state, step) = apply_fetch(Err(FetchError::RateLimited), &current, &mut poll, now);
        // The displayed numbers must be unchanged — never a wrong number.
        match state {
            AccountState::Available(u) => {
                assert_eq!(u.five_hour.used_pct, 31.0);
                assert_eq!(u.seven_day.used_pct, 14.0);
            }
            other => panic!("expected last-good Available, got {other:?}"),
        }
        assert_eq!(step, Step::Backoff);
        assert_eq!(poll.attempt, 1);
    }

    #[test]
    fn transient_before_any_success_is_unavailable() {
        let mut poll = PollState::default();
        let (state, step) = apply_fetch(
            Err(FetchError::Server(503)),
            &AccountState::Unavailable,
            &mut poll,
            OffsetDateTime::now_utc(),
        );
        assert!(matches!(state, AccountState::Unavailable));
        assert_eq!(step, Step::Backoff);
    }

    #[test]
    fn stale_last_good_downgrades_to_unavailable() {
        let now = OffsetDateTime::now_utc();
        let mut poll = PollState {
            attempt: 3,
            last_success: Some(now - Duration::minutes(11)),
            ..PollState::default()
        };
        let current = AccountState::Available(usage(50.0, 20.0));
        let (state, _) = apply_fetch(
            Err(FetchError::Transport("boom".into())),
            &current,
            &mut poll,
            now,
        );
        assert!(matches!(state, AccountState::Unavailable));
    }

    #[test]
    fn parse_none_on_200_is_unavailable_but_keeps_polling() {
        let mut poll = PollState {
            attempt: 4,
            ..PollState::default()
        };
        let (state, step) = apply_fetch(
            Ok(json!({})),
            &AccountState::Unavailable,
            &mut poll,
            OffsetDateTime::now_utc(),
        );
        assert!(matches!(state, AccountState::Unavailable));
        assert_eq!(step, Step::Normal);
        assert_eq!(poll.attempt, 0);
    }

    #[test]
    fn success_resets_attempt_and_records_time() {
        let now = OffsetDateTime::now_utc();
        let mut poll = PollState {
            attempt: 5,
            ..PollState::default()
        };
        let raw = json!({
            "five_hour": { "utilization": 31.0, "resets_at": null },
            "seven_day": { "utilization": 14.0, "resets_at": null }
        });
        let (state, step) = apply_fetch(Ok(raw), &AccountState::Unavailable, &mut poll, now);
        assert!(matches!(state, AccountState::Available(_)));
        assert_eq!(step, Step::Normal);
        assert_eq!(poll.attempt, 0);
        assert_eq!(poll.last_success, Some(now));
    }
}
