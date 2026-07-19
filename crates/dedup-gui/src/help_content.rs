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
Register the folders you want to triage — each becomes a repository: a named folder \
tracked by its own index of file contents.

Add a repository with ADD REPOSITORY, or simply drag folders from your file manager and \
drop them anywhere onto the window. After adding files or changing a folder's contents, \
run UPDATE ALL so the indexes match the disk again — repositories that are already up to \
date finish almost instantly. REFRESH STATUS re-checks each repository's reachability and \
whether it needs an update, without changing anything.

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

Results are shown a page of groups at a time. Each file can be toggled KEEP / DELETE; the \
best copy is starred. AUTO-RESOLVE REST marks every non-best copy at once. DELETE MARKED \
runs the batch behind a confirmation — or turn on QUICK DELETE to skip confirmation per \
group.

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

DIFF is the manual view: it compares the two repos side by side and leaves every decision \
to you. PAIR BY HASH matches files by content, so the same file under two names is one row \
you can settle with a rename; PAIR BY PATH matches by name and folder, so the same name \
holding different content shows up as a conflict. Each row offers what makes sense for it — \
copy the file across, delete it, rename one side to the other's name, or overwrite one side \
with the other — and a side holding several copies of the same content is narrowed down \
first with DELETE ALL or KEEP 1. Nothing happens until you click a row's button; rows that \
are equal on both sides are hidden until you ask for them.

Use the FILTER wizard below to narrow which files are considered — conditions combine with \
AND. PREVIEW shows the matching transfers, paged, plus a total count, without touching \
disk. RUN asks for confirmation, then performs the transfer while you watch its progress — \
CANCEL stops it at any point.";

const GROOMING: &str = "\
Prune and reorganize a single repository. Pick a command from the bar at the top; each has \
its own controls below it.

DEDUPE deletes files in a source repo whose content also exists in any selected dupe-pool \
repo. PURGE deletes everything matching a filter — it requires at least one filter \
condition, so an empty filter can never purge a whole repo. EMPTY DIRS removes empty \
directories under the repo's root. PRUNE drops records of files already deleted from disk \
and compacts the index.

ORGANIZE moves a repo's files into new paths built from an ordered list of rules. Each \
rule is a filter plus a path template: rules are tried top to bottom, and the first rule \
whose filter matches a file decides that file's new path — so order them from most \
specific to most general. Files matching no rule stay where they are, and nothing is ever \
overwritten: a name collision gets renamed instead.

A path template is the file's new location inside the repository, written as literal text \
mixed with {tokens} that are filled in per file. The tokens: {o-path} original folder, \
{o-name} original file name, {o-stem} name without extension, {o-ext} extension, {year} / \
{month} / {day} the file's best-known date (photo capture time when available, otherwise \
the file date), {mimetop} the media kind (image, video, audio, …), {camera} the camera \
model, {origin} the repository a file was transferred from, {size} a coarse size bucket \
(tiny … huge). Click a token chip under the template field to insert it at the cursor.

Inside braces, | tries alternatives left to right until one has a value, and \"quoted\" \
text is a fixed default. Example: {year}/{month}/{o-stem}-{camera|\"nocam\"}.{o-ext} sorts \
photos into year/month folders and tags each name with its camera — or with \"nocam\" \
when the photo doesn't record one. Templates can only place files inside the repository, \
never outside it.

Save the current rule list as a preset with STORE PRESET, to reuse on any repo; right-click a \
preset to rename it.

Every destructive command runs behind a PREVIEW and a confirmation dialog, on a background \
thread with a CANCEL button.";

const BROWSE: &str = "\
Browse one repository folder by folder — subfolders on the left, files in the middle.

Select a file to preview it in the dock below: images and videos get a thumbnail, audio a \
waveform or spectrogram (switchable) beside its ID3 tags, text a scrollable view, and \
anything else a hex-header + strings dump. The hex/strings toggle forces that forensic \
view for any file. Click an image or video preview to open it full-window in the lightbox \
(zoom, pan, Esc to close). For MP3/WAV/AIFF audio, EDIT TAGS rewrites the embedded ID3 \
tags in place — run UPDATE on the repo afterwards so the index sees the changed file.

The file table's columns (NAME, SIZE, TYPE, INFO, MODIFIED, TAGS) are drag-resizable and \
click-to-sort. Use the annotations editor to tag files — tags are also searchable from the \
FILTER wizard used elsewhere in the app, as a TAG condition.

The command column lets you open a file with its default app or reveal it in the file \
manager, without leaving the browser.";
