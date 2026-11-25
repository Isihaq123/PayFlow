//! Deterministic scenario generator for the estimate-vs-live parity property
//! test.
//!
//! This module is compiled for tests only (`#[cfg(test)] mod scenario;` in
//! `lib.rs`), so none of it reaches the wasm build.
//!
//! The generator is deliberately dependency-free: a seeded xorshift64* stream
//! drives every choice, so a failing property case can be replayed from the
//! seed printed in the assertion message. It produces subscription snapshots
//! (not addresses) so the test can write them verbatim into storage before
//! running both the estimate and the live batch charge against identical state.
//!
//! Documented divergences and their whitelist live here next to the generator:
//! see [`documented_outcome_divergence`] and
//! `docs/architecture/batch-estimate-vs-live.md`.

use crate::batch::ChargeResult;

/// Documentation that must list every whitelisted divergence.
pub const DIVERGENCE_DOC: &str = "docs/architecture/batch-estimate-vs-live.md";

/// Amounts used by generated subscriptions.
///
/// All values are valid subscription amounts (positive and far below
/// `MAX_SUBSCRIPTION_AMOUNT`) and small enough that a generated batch stays
/// under the default hourly global volume cap. The parity property only holds
/// below that cap; see the divergence doc.
pub const AMOUNTS: [i128; 6] = [
    1,
    1_000_000,
    50_000_000,
    12_345_678,
    100_000_000,
    999_999_999,
];

/// Intervals used by generated subscriptions (all above the default
/// `min_interval` floor of 3600 seconds).
pub const INTERVALS: [u64; 4] = [3_600, 86_400, 604_800, 2_592_000];

/// The kind of subscription state written for one address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateKind {
    /// No subscription record at all.
    NoSubscription,
    /// Cancelled subscription (`active = false`, not paused).
    Inactive,
    /// `pause()`-style pause: active but paused with no usable expiry.
    PausedIndefinitely,
    /// `pause_until()` whose expiry is still in the future.
    PausedUnexpired,
    /// `pause_until()` whose expiry has passed, so both paths auto-resume.
    PausedExpired,
    /// Active and chargeable, but the interval has not elapsed.
    ActiveNotDue,
    /// Active, due, and (when grace > 0) still inside the grace window.
    ActiveDue,
    /// Active, due, and beyond the grace window. Only generated when
    /// `grace_period > 0`.
    ActiveDueGraceElapsed,
}

/// One generated subscription snapshot. The test writes exactly these fields
/// into contract storage; nothing here depends on the live path.
#[derive(Clone, Copy, Debug)]
pub struct GeneratedSubscription {
    pub kind: StateKind,
    pub has_subscription: bool,
    pub amount: i128,
    pub interval: u64,
    pub last_charged: u64,
    pub active: bool,
    pub paused: bool,
    pub pause_expiry: Option<u64>,
    /// SAC allowance the test grants to the contract.
    pub allowance: i128,
}

/// Seeded xorshift64* generator.
pub struct ScenarioRng {
    state: u64,
}

impl ScenarioRng {
    /// Creates a generator. Distinct seeds yield distinct streams.
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(0xD1B5_4A32_D192_ED03)
                | 1,
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish value in `[0, upper)`. `upper` must be non-zero.
    pub fn next_bounded(&mut self, upper: u64) -> u64 {
        debug_assert!(upper > 0);
        self.next_u64() % upper
    }

    /// Picks one element of `values`.
    pub fn pick<T: Copy>(&mut self, values: &[T]) -> T {
        values[self.next_bounded(values.len() as u64) as usize]
    }
}

/// Generates one subscription snapshot relative to `now`.
///
/// `grace_period` is the contract-wide grace period the test configured for
/// this batch; it is used to build states on both sides of the grace boundary.
pub fn generate(rng: &mut ScenarioRng, now: u64, grace_period: u64) -> GeneratedSubscription {
    let amount = rng.pick(&AMOUNTS);
    let interval = rng.pick(&INTERVALS);

    let roll = rng.next_bounded(100);
    let kind = if roll < 10 {
        StateKind::NoSubscription
    } else if roll < 17 {
        StateKind::Inactive
    } else if roll < 23 {
        StateKind::PausedIndefinitely
    } else if roll < 32 {
        StateKind::PausedUnexpired
    } else if roll < 41 {
        StateKind::PausedExpired
    } else if roll < 56 {
        StateKind::ActiveNotDue
    } else if roll < 85 {
        StateKind::ActiveDue
    } else if grace_period > 0 {
        StateKind::ActiveDueGraceElapsed
    } else {
        StateKind::ActiveDue
    };

    // A paused subscription is due once resumed, so use a due anchor.
    let due_anchor = now.saturating_sub(interval);
    let extra = rng.next_bounded(3_600);

    let (last_charged, active, paused, pause_expiry) = match kind {
        StateKind::NoSubscription => (due_anchor, false, false, None),
        StateKind::Inactive => (due_anchor, false, false, None),
        StateKind::PausedIndefinitely => (due_anchor, true, true, Some(u64::MAX)),
        StateKind::PausedUnexpired => {
            let expiry = now.saturating_add(1).saturating_add(extra);
            (due_anchor, false, true, Some(expiry))
        }
        StateKind::PausedExpired => {
            let expiry = now.saturating_sub(1).saturating_sub(extra);
            (due_anchor, false, true, Some(expiry))
        }
        StateKind::ActiveNotDue => {
            let delta = 1 + rng.next_bounded(interval);
            (due_anchor.saturating_add(delta), true, false, None)
        }
        StateKind::ActiveDue => {
            // Exactly due, or inside the grace window when one is configured.
            let within_grace = if grace_period > 0 {
                rng.next_bounded(grace_period)
            } else {
                0
            };
            (due_anchor.saturating_sub(within_grace), true, false, None)
        }
        StateKind::ActiveDueGraceElapsed => {
            let lapse = interval
                .saturating_add(grace_period)
                .saturating_add(1)
                .saturating_add(extra);
            (now.saturating_sub(lapse), true, false, None)
        }
    };

    // 0, half, one short, exactly enough, or comfortably enough. Exactly-enough
    // and short allowances are the boundaries the shared precheck must agree on.
    let allowance = match rng.next_bounded(5) {
        0 => 0,
        1 => amount / 2,
        2 => (amount - 1).max(0),
        3 => amount,
        _ => amount.saturating_mul(2),
    };

    GeneratedSubscription {
        kind,
        has_subscription: kind != StateKind::NoSubscription,
        amount,
        interval,
        last_charged,
        active,
        paused,
        pause_expiry,
        allowance,
    }
}

/// Facts about one position in a batch that the whitelist needs.
#[derive(Clone, Debug)]
pub struct BatchContext {
    /// True when this address appeared earlier in the same batch.
    pub repeated_address: bool,
    /// True when at least one earlier occurrence of this address was `Charged`.
    /// Later occurrences are no longer due because the live path persisted
    /// `last_charged` on that first charge.
    pub charged_earlier: bool,
}

/// Returns `Some(reason)` when an estimate/live outcome pair is a
/// *documented* divergence, or `None` when it must be treated as a bug.
///
/// Everything not covered by an arm here is an undocumented divergence: the
/// property test fails rather than silently tolerating it. When a new
/// divergence is discovered, add it to
/// `docs/architecture/batch-estimate-vs-live.md` **and** to this function in
/// the same change.
pub fn documented_outcome_divergence(
    estimate: ChargeResult,
    live: ChargeResult,
    context: &BatchContext,
) -> Option<&'static str> {
    if estimate == live {
        return None;
    }

    match (estimate, live) {
        // Live processes a batch sequentially and persists `last_charged` after
        // a successful charge, so the second occurrence of the same address is
        // no longer due. The estimate never mutates state, so every position
        // reports the pre-batch snapshot.
        (ChargeResult::Charged, ChargeResult::Skipped)
            if context.repeated_address && context.charged_earlier =>
        {
            Some(
                "repeated address: live returns Skipped after charging the address earlier in the batch",
            )
        }
        _ => None,
    }
}
