# 02 — Read the sync group's main once per push, not once per sink

Status: ready-for-agent
Spec: ../spec.md

## Problem

Planning and running a sync-group push loops over the group's sinks and calls the shared
per-pair sync planner once per sink. Each of those calls re-opens the main repository and
re-collects its entries over the whole index. A group with five sinks reads the main's index
five times to produce the same data.

Verified in `sync_group.rs`: both the planning and running functions loop over
`group.sinks` calling the pair-wise planner, which performs its own source collection each
time.

## Approach

Collect the main repository's entries and its content-key set **once**, before the sink loop,
and pass that collected view into each per-sink plan.

This touches the shared planning/diffing signatures, which are also used by the Transfer tab
and the CLI — so it needs care. Prefer an additive change (an optional pre-collected source
view, with the existing behaviour when absent) over rewriting every caller, unless a clean
refactor across all callers is genuinely small.

The mirror-source guard that refuses to push from an empty main must keep firing exactly as
it does now, at both plan time and run time.

## Seam and tests

Core seam — `crates/dedup-core/tests/sync_groups.rs`, which already covers group push:

- a push to several sinks produces the same plans as before the change (behaviour parity is
  the point — this is an optimisation, not a feature)
- the empty-main mirror refusal still fires
- a cancelled push still reports per-sink outcomes as it does today

Behaviour parity is what is being asserted; do not add a test that pins the *number* of index
reads unless a natural seam for counting already exists — an artificial counter is
implementation-detail testing.

## Done

Standing gate green. `CHANGELOG.md` and `ai/improvements.md` updated.
