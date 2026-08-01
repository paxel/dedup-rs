# 04 — The Settings control, persisted, defaulting to Dark

Status: ready-for-agent
Spec: ../spec.md
Blocked by: 03

## Problem

The appearance can be resolved but not chosen. A user needs to pick one, have it remembered,
and — critically — have *not* choosing leave their interface exactly as it was.

## Approach

Add the preference to the persisted settings and a three-way **System / Light / Dark** control
to the existing Settings modal, beside tooltip verbosity. No new surface.

**The default is Dark, not System.** Light is strictly opt-in. Colour here is semantic — red
means files will be deleted — and an existing user on a light desktop must not launch after an
update into a repainted colour vocabulary, least of all the delete-versus-differs distinction
that is the most fragile pair in the palette. A "System for new installs, Dark for upgrades"
variant was considered and rejected: behaviour that differs between fresh installs and upgrades
makes bug reports irreproducible.

Because the default is Dark rather than the serialization default of whatever egui prefers, be
explicit about it: a settings file written before this feature existed must load as Dark.

Choosing an option applies it immediately and writes it with the other settings.

Label the options plainly. "System" needs to convey *follows your desktop* — a user who has
never met the term should not have to guess.

## Seam and tests

Settings inline tests, prior art `settings_round_trip_and_default_on_missing` and
`tooltip_verbosity_json_tag_is_stable`:

- the preference round-trips through save and load
- a settings file **without** the field loads as Dark — the upgrade path, and the one that
  protects existing users
- the serialized tag is stable, so a later rename cannot silently reset everyone's choice

GUI inline test in the Settings modal:

- the control offers exactly the three options
- choosing one updates the resolved appearance in the same frame — the observable behaviour,
  rather than that a field was written

## Done

Standing gate green. This is where the feature becomes real, so: `CHANGELOG.md`, `README.md` if
appearance is worth a line there, `ai/improvements.md` (strike the light-theme item), and the
GUI documentation page covering Settings.
