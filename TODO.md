# TODO

## Auth-recovery trigger is unreachable

`launch()` in `src/main.rs` can no longer return `Ok(false)` after the gate-marker
swap (Unity 6000.2.x stopped emitting `"Licensing is initialized"` on warm starts,
so we now key off `"Successfully updated license"` and have no reliable failure
marker). That leaves `run_auth()` plumbed but never invoked.

Restore by detecting one of:
- Unity process death within the polling budget (no success marker emitted, pid gone)
- A failure-pattern hit in the log: `"License activation has failed. Aborting."`,
  `"License error"`, `"LicenseExpired"` (all verified present in the Unity 6000.2.7f2
  binary via `strings`)

## Log rotation

`<project>/Logs/unity-*.log` and `<project>/Logs/unity-auth-*.log` accumulate
forever — every launch writes a new timestamped file. Add a prune step (e.g. keep
last N or last 7 days). Pre-existing, not introduced by the auth-hook migration.
