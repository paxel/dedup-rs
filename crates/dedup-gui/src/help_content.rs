//! Per-screen copy shown in the HELP window (see [`crate::app::DedupApp`]'s
//! HELP button): what the current screen is for and how its controls fit
//! together.

use crate::app::Screen;

/// The help text for `screen`.
pub fn help_text(screen: Screen) -> &'static str {
    match screen {
        Screen::Repositories => REPOSITORIES,
        Screen::Duplicates => DUPLICATES,
        Screen::Sync => SYNC,
        Screen::Copy => COPY,
        Screen::Delete => GROOMING,
        Screen::Browse => BROWSE,
    }
}

const REPOSITORIES: &str = "\
The repository page lists every repository as a card. The bar along the top switches \
between this page and the process pages: DUPLICATES, SYNC, COPY and DELETE.

A repository is a named folder on disk, tracked by its own index of file contents. Add one \
with ADD REPOSITORY, or drag folders from your file manager and drop them anywhere onto \
the window. Each card shows the repository's path, its FILES, SIZE, MISSING and UPDATED \
stats, status pills (LOCAL / REMOTE / OFFLINE / MISSING for reachability, UP TO DATE / \
UPDATE REQUIRED from UPDATE's last look) and its padlock: every repository starts locked each \
launch, and while it is locked nothing in the app deletes or overwrites its files.

On the right, the text filter and LOCAL / MAINS / ALL grey out every repository that does \
not match and sort it to the bottom; a greyed-out repository cannot be selected and nothing \
touches it. Click a card's name to select or deselect it, or use SELECT ALL / SELECT NONE; a \
selected card has a thick, bright border. SET ALL RO / SET ALL RW lock or unlock every \
repository that is not greyed out.

UPDATE and DELETE EMPTY DIRS act on the selection. Each first looks at what it would \
change and shows it; only OK changes anything. REFRESH STATUS re-checks every \
repository's reachability at once.

Right-click a card for its own edits — RENAME, RELOCATE, CLONE INDEX, MARK REMOTE, REMOVE \
— and for its place in a backup group: MAKE MAIN, JOIN GROUP, LEAVE GROUP. A backup group \
is framed together, main first; right-click the frame's bar for ADD BACKUP and UNGROUP. A \
backup's push mode is set on its card.";

const SYNC: &str = "\
Push backup groups from their mains to their backups. Drag a main from the pool into the \
card: its backups come along with their push modes (set on the backup's card on the \
repository page). A backup can be left out of this run with its IN THIS RUN switch. \
Backups in APPLY CHANGES or MIRROR mode can lose files, so the card unlocks them; ADD ONLY \
backups only gain files and keep their lock. A backup locked again elsewhere sends its \
group back to the pool.

PREVIEW plans every push and opens it in its own window: per backup what it would gain \
(+) and lose (−). The page stays locked until GO or CANCEL. GO pushes, shows the progress \
and the report in that window, and then plans again so you see what is left.";

const DUPLICATES: &str = "\
Find exact or perceptually similar duplicates across one or more repositories, review them \
with real previews, and delete the worse copies.

Drag repositories from the pool into one of two cards. The READ-ONLY card is safe from \
changes: a repository dropped there is locked and nothing in it is deleted. The READ-WRITE \
card is where copies get deleted: a repository dropped there is unlocked. A repository's \
padlock moves it to the other card, its (x) back to the pool; only repositories in a card \
are searched.

HOW sets what counts as a duplicate: EXACT finds byte-for-byte matches (same size + hash), \
SIMILAR finds perceptually similar images, videos and audio — a lower threshold catches \
more, and riskier, matches. The FILTER keeps only groups with a matching file. PREVIEW \
searches in the activity window and opens the result in its own window; the page is locked \
until that window ends with GO or CANCEL.

The preview shows a page of groups at a time, already decided: the best copy (starred) is \
kept, and every other copy in a read-write repository is marked. You can still toggle a \
file between KEEP and DELETE. GO deletes every marked file, behind one confirmation, and \
then searches again; the progress and the report show in the preview window. CANCEL closes \
it and deletes nothing. QUICK DELETE gives each group a DELETE NOW button instead; every \
deleted file is written to the event log (LOG, top right).

Click a thumbnail to open the lightbox. It opens on Overview — both files' facts, repos and \
mark pills side by side — and its tabs offer whatever the two files can show: Image, Video \
or Audio for zoom, filmstrip and A/B compare; Metadata for ID3 tags and EXIF capture facts; \
Text for a text or hex preview of documents and anything else without a picture. Right-click \
a card for OPEN and SHOW IN FOLDER.";

const COPY: &str = "\
Copy into one repository what it and the reference repositories lack. Drag the sources \
into COPY FROM (they are locked, nothing in them changes), the one target into COPY INTO \
(it only gains files, so its lock stays), and further repositories into ALREADY KNOWN IN \
(locked). A content any reference or the target has ever held is not copied, and content \
in two sources is copied once.

HOW sets where the copies land: an optional SUBFOLDER in the target, and under it an \
optional TEMPLATE such as {year}/{month}/{o-name}; empty keeps each source path. The \
FILTER limits which files take part.

PREVIEW lists every copy in its own window; a name already taken gets a numeric suffix and \
is marked — nothing is ever overwritten. The page stays locked until GO or CANCEL. GO \
copies, shows the progress and the report in that window, and then plans again.";

const GROOMING: &str = "\
Prune and reorganize a single repository. DELETE opens this view on DEDUPE: pick the tool \
in WHAT, the repository (or DEDUPE's source and pool), then HOW (the filter or the rules), \
then PREVIEW and APPLY ALL. After PREVIEW or APPLY ALL the sections fold to their header \
bars; click one to flip it open again.

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

Every command previews before it applies. PREVIEW plans in \
the activity window and fills the board; APPLY ALL performs the plan in the activity window — \
each file as it goes, every problem, and CANCEL — and ends on the result report. A row's \
APPLY answers with a notification card, and every file changed is in the event log behind \
the LOG button.";

const BROWSE: &str = "\
Browse a repository folder by folder — subfolders on the left, files on the board in the \
middle, the preview dock below. SHOW IN BROWSE on a duplicate opens it at that file; the \
navigation bar leads back.

Select a file to preview it in the dock below: images and videos get a thumbnail, audio a \
waveform or spectrogram (switchable) beside its ID3 tags, text a scrollable view, and \
anything else a hex-header + strings dump. The hex/strings toggle forces that forensic \
view for any file. Click an image or video preview to open it full-window in the lightbox \
(zoom, pan, Esc to close). For MP3/WAV/AIFF audio, EDIT TAGS rewrites the embedded ID3 \
tags in place — a notification card confirms it and the event log keeps the line; run \
UPDATE on the repo afterwards so the index sees the changed file. Tag annotations answer \
with a card too, but change only the index.

The file table's columns (NAME, SIZE, TYPE, INFO, MODIFIED, TAGS) are drag-resizable and \
click-to-sort. Use the annotations editor to tag files — tags are also searchable from the \
FILTER wizard used elsewhere in the app, as a TAG condition.

The command column lets you open a file with its default app or reveal it in the file \
manager, without leaving the browser.";
