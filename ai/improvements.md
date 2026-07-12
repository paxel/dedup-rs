# dedup-rs — Improvement Roadmap

## Vision

dedup-rs is a **data-inheritance triage tool**. The scenario it serves: someone dies (or a
machine dies) and leaves behind a NAS, broken PCs, and an unsorted heap of disks full of
redundant backups. A caretaker must find the useful and important material — documents,
photos, crypto wallets, keys — without eyeballing terabytes of duplicates. As the
"no hardcopies" generation ages, this is a recurring, real problem.

The product is a four-stage pipeline:

```
 Stage 1: REDUCE     eliminate duplicates within each disk           (done)
 Stage 2: SANITIZE   copy unique content to a curated dir, then      (done)
                     diff every next disk against it
 Stage 3: REFINE     drop media that exists elsewhere in better      (done)
                     quality — needs fast human review
 Stage 4: ORDER      organize survivors by time/importance; flag     (done)
                     wallets, keys, vital documents
```

Media strategy is **hybrid**: in-app image zoom, in-app audio playback, video as
scrub-able frame strips; one click hands any file to the system's external app for full
fidelity.

All four phases planned in this document (review tooling, sanitize workflow, content
coverage, forensic layer) have shipped. What remains is cross-cutting debt.

---

## Remaining / deferred work

- **Light theme toggle** (M/L, deferred by decision 2026-07-08): requires converting
  `theme.rs` constants to a runtime palette across all views. Still dark-only.
- **Performance at scale**: banded grouping, staged pipelines, and the multi-reference
  diff's merged content index are fine at ~10⁵ files; revisit content-index memory
  (`HashMap<(u64,[u8;32]), _>` across all references) and timeline streaming at 10⁷.
- **Testing discipline** (standing practice, not a task): every GUI feature ships with
  kittest geometric tests + an `--ignored` render snapshot; every core feature with
  temp-repo integration tests; store format changes must include a legacy-decode test
  (pattern: `store.rs::v1_entries_decode_and_flag_images_stale`).
