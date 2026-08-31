//! Session-switch nonces (SWITCHER §4). The CLI token never lands in a process
//! argument, so `POST /accounts/:id/session` mints a single-use, short-lived
//! code and the spawned shim redeems it over localhost for the token itself.
//!
//! Pure and clock-injected, so the expiry and single-use rules are tested
//! without sleeping. A nonce is as good as a credential until it is burned:
//! never log one, and never let one outlive [`NONCE_TTL`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rand::RngCore;

/// How long a minted nonce stays redeemable. Long enough for a terminal to
/// start and issue one HTTP call, short enough that a leaked command line is
/// worthless by the time anyone reads it.
pub const NONCE_TTL: Duration = Duration::from_secs(30);

/// Guards against a mint loop wedging memory if launches keep failing. Far above
/// any real use — one user clicking a button.
const MAX_PENDING: usize = 64;

/// A minted, unredeemed launch. Holds the account id, never a token: the token
/// is read from the store at redemption, so a nonce outliving its account
/// buys nothing.
struct Pending {
    account: String,
    expires_at: Instant,
}

/// The daemon's outstanding launch codes.
#[derive(Default)]
pub struct Nonces {
    pending: HashMap<String, Pending>,
}

impl Nonces {
    /// Mint a code for `account`. 256 bits of randomness, hex — the same charset
    /// `cuw_launch::plan::validate` accepts, so a minted nonce always survives
    /// the argv check.
    pub fn mint(&mut self, account: &str, now: Instant) -> String {
        self.sweep(now);
        // Only reachable if launches are failing en masse; drop the oldest
        // rather than grow without bound.
        if self.pending.len() >= MAX_PENDING {
            if let Some(oldest) = self
                .pending
                .iter()
                .min_by_key(|(_, p)| p.expires_at)
                .map(|(k, _)| k.clone())
            {
                self.pending.remove(&oldest);
            }
        }

        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let nonce: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.pending.insert(
            nonce.clone(),
            Pending {
                account: account.to_string(),
                expires_at: now + NONCE_TTL,
            },
        );
        nonce
    }

    /// Redeem a code, burning it. `None` if it is unknown, already spent, or
    /// expired — the caller must not distinguish those to the shim.
    pub fn redeem(&mut self, nonce: &str, now: Instant) -> Option<String> {
        self.sweep(now);
        self.pending.remove(nonce).map(|p| p.account)
    }

    /// Burn a code without redeeming it — used when the launch that would have
    /// spent it never started.
    pub fn burn(&mut self, nonce: &str) {
        self.pending.remove(nonce);
    }

    /// Drop every code whose TTL has passed. Called on both paths, so an
    /// abandoned launch cannot leave a live code behind.
    pub fn sweep(&mut self, now: Instant) {
        self.pending.retain(|_, p| p.expires_at > now);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonce_redeems_once() {
        let now = Instant::now();
        let mut n = Nonces::default();
        let nonce = n.mint("work-abc12345", now);
        assert_eq!(n.redeem(&nonce, now).as_deref(), Some("work-abc12345"));
        assert_eq!(n.redeem(&nonce, now), None, "a nonce is single-use");
    }

    #[test]
    fn an_expired_nonce_is_refused_and_swept() {
        let now = Instant::now();
        let mut n = Nonces::default();
        let nonce = n.mint("work-abc12345", now);
        let later = now + NONCE_TTL + Duration::from_millis(1);
        assert_eq!(n.redeem(&nonce, later), None);
        assert_eq!(n.len(), 0, "the expired code was swept");
    }

    #[test]
    fn a_nonce_is_still_good_at_the_edge_of_its_ttl() {
        let now = Instant::now();
        let mut n = Nonces::default();
        let nonce = n.mint("work-abc12345", now);
        assert!(n
            .redeem(&nonce, now + NONCE_TTL - Duration::from_millis(1))
            .is_some());
    }

    #[test]
    fn an_unknown_nonce_is_refused() {
        let mut n = Nonces::default();
        assert_eq!(n.redeem("not-a-real-nonce", Instant::now()), None);
    }

    #[test]
    fn burning_removes_it_without_redeeming() {
        let now = Instant::now();
        let mut n = Nonces::default();
        let nonce = n.mint("work-abc12345", now);
        n.burn(&nonce);
        assert_eq!(n.redeem(&nonce, now), None);
        assert_eq!(n.len(), 0);
    }

    /// The charset and length `cuw_launch::plan::validate` demands.
    #[test]
    fn a_minted_nonce_passes_the_launcher_check() {
        let nonce = Nonces::default().mint("work-abc12345", Instant::now());
        assert_eq!(nonce.len(), 64);
        assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn two_mints_never_collide() {
        let now = Instant::now();
        let mut n = Nonces::default();
        let a = n.mint("work-abc12345", now);
        let b = n.mint("work-abc12345", now);
        assert_ne!(a, b);
        assert_eq!(n.len(), 2);
    }

    #[test]
    fn pending_codes_are_bounded() {
        let now = Instant::now();
        let mut n = Nonces::default();
        for _ in 0..MAX_PENDING + 10 {
            n.mint("work-abc12345", now);
        }
        assert!(n.len() <= MAX_PENDING, "{} pending", n.len());
    }
}
