//! Per-tab copy shown in the HELP window (see [`crate::app::DedupApp`]'s HELP
//! button): what the current tab is for and how its controls fit together.

use crate::app::Tab;

/// The help text for `tab`.
pub fn help_text(tab: Tab) -> &'static str {
    match tab {
        Tab::Repositories => REPOSITORIES,
        Tab::Duplicates => DUPLICATES,
        Tab::Transfer => TRANSFER,
        Tab::Grooming => GROOMING,
        Tab::Browse => BROWSE,
    }
}

const REPOSITORIES: &str = "\
Register, scan, and manage repositories — a repository is just a name linked to a folder \
on disk, tracked by its own index.

ADD REPOSITORY registers a new folder. UPDATE ALL scans every repository (already \
up-to-date ones finish almost instantly). REFRESH STATUS re-checks each repository's \
location/reachability and whether its index is stale, without hashing or writing anything.

Each card shows FILES, SIZE, MISSING, SCANNED, and (once triage-done) TRIAGED stats, plus \
status pills: LOCAL / REMOTE / OFFLINE / MISSING for reachability, and UP TO DATE / UPDATE \
REQUIRED from the last CHECK.

Per-repository actions: UPDATE / SCAN (hash new/changed files), CHECK (dry-run, no writes), \
RENAME (registry name only, the on-disk folder isn't moved), RELOCATE (point at a different \
folder, keep the index), DUPLICATE (clone the whole index into a new repository at a new \
path), and DELETE (remove the registry entry and its index — the on-disk files are never \
touched).";

const DUPLICATES: &str = "\
Find exact or perceptually similar duplicates across one or more repositories, review them \
with real previews, and delete the worse copies.

Click a repo chip's name to include or exclude it from the next FIND; click its padlock to \
protect it from deletion — locked files are never preselected or auto-resolved.

DUPLICATES mode finds exact, byte-for-byte matches (same size + hash). SIMILAR mode finds \
perceptually similar images and videos and reveals a similarity threshold slider — lower \
catches more, and riskier, matches.

Results page 50 groups at a time. Each file can be toggled KEEP / DELETE; the best copy is \
starred. AUTO-RESOLVE REST marks every non-best copy at once. DELETE MARKED runs the batch \
behind a confirmation — or turn on QUICK DELETE to skip confirmation per group.

Click a thumbnail to open the lightbox (zoom, A/B compare, audio/video preview). Right-click \
a card for OPEN and SHOW IN FOLDER.";

const TRANSFER: &str = "\
Copy, move, or sync files between two repositories by content (size + hash — paths never \
matter), with an assisted filter and a preview before anything runs.

Pick a SOURCE and a TARGET repo, optionally add ALSO REF repositories — a source file only \
counts as new when none of the target or the extra references already has its content.

COPY copies source files the target doesn't have (the source is left in place). MOVE does \
the same, then marks the source entries missing. SYNC mirrors the source into the target at \
the same relative path, and can also delete target files whose content the source has lost \
(DELETE MISSING).

Use the FILTER wizard below to narrow which files are considered — conditions combine with \
AND. PREVIEW shows the first matching transfers and a total count without touching disk; RUN \
starts the command on a background thread, live progress and all, behind a confirmation.";

const GROOMING: &str = "\
Prune and reorganize a single repository. Pick a command from the bar at the top; each has \
its own controls below it.

DEDUPE deletes files in a source repo whose content also exists in any selected dupe-pool \
repo. PURGE deletes everything matching a filter. EMPTY DIRS removes empty directories under \
the repo's root. PRUNE drops records of files already deleted from disk and compacts the \
index.

ORGANIZE moves a repo's files into new paths built from templates (date, MIME, camera, \
original name, …), based on an ordered list of rules — each rule is a filter plus a path \
template.

Rules are checked top to bottom: the first rule whose filter matches a file decides its new \
path, and later rules are simply skipped for that file. Every file's match is decided up \
front, before anything moves — so a rename made by one rule never causes another rule to \
gain or lose a match mid-run. Order rules from most specific to most general. Files matching \
no rule are left where they are, and nothing is ever overwritten — a name collision gets \
renamed instead.

Save the current rule list as a preset with STORE PRESET, to reuse on any repo; right-click a \
preset to rename it.

Every destructive command runs behind a PREVIEW and a confirmation dialog, on a background \
thread with a CANCEL button.";

const BROWSE: &str = "\
A directory-based file browser for one repository, built entirely from the index — a \
two-column view of subfolders and files.

Selecting a file drives the preview dock below: images get a thumbnail, audio a waveform, \
text a scrollable view, and anything else a hex-header + strings dump (or force that \
forensic view for any file with the hex/strings toggle).

The file table's columns (NAME, SIZE, TYPE, INFO, MODIFIED, TAGS) are drag-resizable and \
click-to-sort. Use the annotations editor to tag files — tags are also searchable from the \
FILTER wizard used elsewhere in the app, as a TAG condition.

The command column lets you open a file with its default app or reveal it in the file \
manager, without leaving the browser.";
