# Batch charge: estimate vs live

`get_batch_charge_estimate(users)` and `batch_charge(users)` both return one
[`ChargeResult`](../GLOSSARY.md) per address. Keepers use the estimate for
dry-runs, dashboards and alerting, so any difference between the two surfaces
must be deliberate and documented. This page is the source of truth for those
differences, and the randomized property test in
`contract/src/test.rs` enforces it.

- Estimate: `contract/src/lib.rs` → `get_batch_charge_estimate`
- Live: `contract/src/batch.rs` → `batch_charge`
- Shared precheck: `contract/src/charge_exec.rs` → `precheck_charge`
- Property test: `test_estimate_matches_live_batch_charge_property`
- Generator + divergence whitelist: `contract/src/scenario.rs`

## Shared behaviour (must stay equal)

Both paths read the same subscription snapshot, run the same
`precheck_charge`, and compare the SAC allowance against the **gross**
`sub.amount`. For every case below the outcomes are identical:

| Snapshot state                                          | Estimate result        | Live result            |
| ------------------------------------------------------- | ---------------------- | ---------------------- |
| No subscription record                                  | `NoSubscription`       | `NoSubscription`       |
| Cancelled (`active = false`, not paused)                | `Inactive`             | `Inactive`             |
| Paused, no expiry or expiry in the future               | `Paused`               | `Paused`               |
| Paused, expiry passed (auto-resumed) and not due        | `Skipped`              | `Skipped`              |
| Active, interval not elapsed                            | `Skipped`              | `Skipped`              |
| Active, due, inside the grace window                    | continues to allowance | continues to allowance |
| Active, due, past `next + grace_period`                 | `GracePeriodElapsed`   | `GracePeriodElapsed`   |
| Active, due, `allowance < sub.amount`                   | `AllowanceInsufficient`| `AllowanceInsufficient`|
| Active, due, `allowance >= sub.amount`                  | `Charged` (no transfer)| `Charged` (transfers)  |

The last row is the intended meaning of the estimate: `Charged` means “the live
path would attempt the transfer now”, not “funds moved”.

## Documented divergences

These are the only tolerated differences. Anything else is a bug and makes the
property test fail.

### 1. Protocol pause

Live `batch_charge` calls `ensure_contract_not_paused` first and panics with
`ContractError::ContractPaused` (18) for the whole call. The estimate has no
pause gate: it still returns per-address results and can report `Charged`
while the protocol is paused.

Callers that act on an estimate must check
`get_contract_config().paused` (or `is_contract_paused()`) themselves.
`simulate_charge` is the pause-aware single-user alternative.

Test: `test_estimate_ignores_contract_pause_while_live_rejects`.

### 2. Batch size cap

- Estimate: hardcoded cap of **200** addresses, then `BatchTooLarge`.
- Live: `get_max_batch_size()`, default **50**, admin-configurable up to 200.

A batch of 51–200 addresses is valid for the estimate but rejected by live
`batch_charge` with the default configuration. Estimators must use
`get_max_batch_size()` for batches they intend to submit.

### 3. The estimate is not a pure view

When a bounded pause (`pause_until`) has expired, the estimate calls the real
`try_auto_resume`. That **writes** the subscription, sets `active = true`,
clears `PauseExpiry` and emits `subscription_auto_resumed`. Live does the same.
The outcomes still match; only the assumption “estimate does not mutate state”
is wrong.

Test: `test_estimate_persists_expired_pause_auto_resume`.

### 4. Repeated address in one batch

The live path processes sequentially and persists `last_charged` after a
successful charge. If the same address appears again later in the batch, it is
no longer due and returns `Skipped`. The estimate never persists the charge, so
every position is evaluated against the pre-batch snapshot and reports
`Charged`.

Whitelisted pair: `(estimate = Charged, live = Skipped)` when the address was
already `Charged` earlier in the same batch. Callers should de-duplicate
addresses rather than rely on this.

Test: `test_estimate_vs_live_duplicate_address_divergence` and the whitelist in
`scenario::documented_outcome_divergence`.

### 5. Transfer-time failures abort live only

After the shared precheck and allowance test pass, live execution can still
fail and abort the entire invocation (Soroban has no per-user try/catch). The
estimate does not model these:

- subscriber **balance** below `sub.amount` (allowance is not a balance);
- hourly **global volume cap** exceeded (`GlobalVolumeExceeded`);
- SAC-level failures (frozen/unauthorised token, trustline limits, etc.).

In these cases the estimate reports `Charged` while live panics and no address
in the batch is charged. Keepers must preflight balances and the volume cap.
The property test deliberately keeps generated states inside the safe envelope
(balance ≥ 3× amount, batch volume below the cap) so it compares outcomes
rather than host aborts.

### 6. Fees change balances, not outcomes

With a configured protocol fee the live path debits the subscriber by the gross
amount in two transfer legs (fee to the collector, net to the merchant). The
allowance preflight covers the gross amount and `fee + net == gross`, so the
outcome is unchanged, but balances differ because the estimate never transfers.
The property test randomises the fee rate and still requires equal outcomes.

### 7. Live-only side effects

Live `Charged` also updates `last_charged`, TTL, charge history, merchant
revenue stats, the global volume window and emits `charged` /
`batch_charge_skips`. The estimate does none of that (auto-resume excepted).
These are effects, not `ChargeResult` differences.

## Property test

`test_estimate_matches_live_batch_charge_property` in `contract/src/test.rs`
runs **64 deterministic seeds**. For each seed it:

1. configures a random grace period (0, 1 h, 1 day) and fee rate (0, 250, 1000 bps);
2. generates 1–3 subscription snapshots (`scenario::generate`) covering
   missing, cancelled, paused (indefinite / unexpired / expired), not-due, due
   and grace-lapsed states, with boundary allowances (0, half, one short,
   exact, 2×);
3. writes the snapshots directly into storage and funds each address;
4. builds a batch of 1–6 positions drawn from those addresses, so repeated
   addresses occur;
5. calls the estimate, then live `batch_charge`, and compares the results
   position by position.

A pair is accepted only if it is equal or
`scenario::documented_outcome_divergence` recognises it (currently only the
repeated-address case). A failure message contains the seed, batch layout,
state, grace period and fee rate needed to reproduce it. The test also asserts
that every `ChargeResult` variant and at least one whitelisted divergence were
actually exercised, so a degenerate generator cannot pass silently.

Run it with:

```bash
cd contract
cargo test test_estimate_matches_live_batch_charge_property -- --nocapture
```

## Adding a new divergence

1. Add a section to this page explaining the cause and the safe caller
   workaround.
2. Add the matching arm to
   `scenario::documented_outcome_divergence` (with a comment).
3. Add a focused test that pins the pair, or extend the generator so the
   property test produces it.
4. Do not weaken the whitelist to a catch-all: undocumented pairs must fail.
