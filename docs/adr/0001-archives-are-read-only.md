# Archives are read-only

The tool never writes to an archive — no repacking to strip a redundant member,
no in-place mutation, ever. The whole archive is the unit of deletion; a member
can be seen, compared and extracted, but not individually removed.

## Why

A reasonable reader will assume the opposite: this is a deduplication tool, and
a 4 GB zip that is 90% redundant is an obvious candidate for repacking to
reclaim the wasted space. We deliberately don't, for three reasons that compound:

- **Forensic provenance.** The tool exists to triage *inherited* data. Rewriting
  an inherited archive alters the evidence, and a mid-repack failure destroys the
  only copy. A tool you cannot trust not to corrupt an heirloom archive is not a
  triage tool.
- **Encrypted archives make it impossible anyway.** You cannot repack a locked
  archive without its password, so per-member mutation would work for some
  archives and not others — an incoherent capability.
- **Coverage already answers the real question.** "Is this whole archive
  redundant?" is computed from member content identity without touching the
  archive. Whole-archive deletion is coherent and safe; per-member deletion is
  the part with no safe implementation.

## Consequences

- Deleting content that a redundant member duplicates acts on the whole archive
  (when fully covered) or on the loose copy — never on one member.
- Reclaiming space *inside* a fat archive is out of scope. If it is ever wanted,
  it is a separate, explicit "repack" operation, not part of triage.
- Password recovery is layered rather than a built-in GPU cracker (supplied
  password → built-in easy wins → export the hash to hashcat), for the same
  spirit of honesty: an archive you cannot open stays closed and clearly marked,
  rather than the tool pretending to strength it does not have.
