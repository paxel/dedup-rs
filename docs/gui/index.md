# GUI guide

`dedup` with no arguments opens the desktop app: a single window with tabs for
[Repositories](repositories.md), [Duplicates](duplicates.md), and [Files](files.md) (which
also pushes a backup group's main to its sinks — see [Group sync](files.md#group-sync)) —
plus a settings cog and an about button in the top-right.

Launch with `--ui-scale <0.5–3.0>` to scale the whole interface, e.g. `dedup --ui-scale 1.25`
for a HiDPI display or a projector.

## Top bar

The tab buttons switch views; the current tab is highlighted. On the right:

- **SETTINGS** (cog icon) opens the settings dialog below.
- **ABOUT** shows the version, license, and contact info.

## Settings dialog

![Settings dialog](../screenshots/settings_modal.png)

- **Hashing threads** — how many threads a repo scan uses to hash files in parallel (a
  `DragValue`, drag or click to type). `0` lets the hashing library pick one thread per CPU
  core; raise it on a multi-core machine with a fast disk for quicker scans.
- **Tooltips: SHORT / VERBOSE** — controls hover-text detail throughout the app. **SHORT**
  keeps every tooltip a one-line hint; **VERBOSE** expands them into a fuller paragraph
  explaining what the control does and when to use it. This is the setting to flip on if
  you're new to the app and want the interface to teach you as you hover, or back to SHORT
  once you know your way around.

Both settings persist across launches, written to `gui_settings.json` in the config
directory (`$XDG_CONFIG_HOME/dedup`, or `~/.config/dedup` by default) the moment they change.
Session logs live separately under `$XDG_STATE_HOME/dedup/logs` (default
`~/.local/state/dedup/logs`), ten runs deep, reachable from Settings → OPEN LOG FOLDER. Per-repo read-only state is
deliberately **not** persisted — every repo re-locks on launch as a safety default, since
unlocking for deletion should be a fresh, conscious choice each session.

## What's not here

The important-file scanner (`dedup scan`) and the triage report (`dedup report`) are
CLI-only today — there's no dedicated GUI tab for them yet.

## See also

- [Repositories tab](repositories.md) — register, scan, rename, relocate, duplicate, delete.
- [Duplicates tab](duplicates.md) — find and review exact/similar duplicates, the lightbox,
  A/B compare, audio and video preview.
- [Files tab](files.md) — copy/move/delete between repos by content, the filter builder.
- [CLI reference](../cli.md) — every command the GUI's operations are also available from.
