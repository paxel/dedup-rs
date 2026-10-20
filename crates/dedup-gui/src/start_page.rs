//! The start page's command list (ADR 0004): every operation the app offers,
//! grouped by intent, always complete and in the same order. A command the
//! current selection does not allow is greyed with the reason on hover, so the
//! list itself teaches what each command needs.
//!
//! The rules live here as pure functions over [`RepoFact`]s so they can be
//! unit-tested without a store or a window; [`command_panel`] only draws them.

use crate::settings::TooltipVerbosity;
use crate::theme;
use crate::util::ExplainExt;
use egui::{Color32, RichText};

/// What a command is for — the headers of the command list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Intent {
    Investigate,
    Maintain,
    Move,
    Clean,
}

impl Intent {
    fn label(self) -> &'static str {
        match self {
            Intent::Investigate => "INVESTIGATE",
            Intent::Maintain => "MAINTAIN",
            Intent::Move => "MOVE",
            Intent::Clean => "CLEAN",
        }
    }

    pub(crate) fn accent(self) -> Color32 {
        match self {
            Intent::Investigate => theme::amber(),
            Intent::Maintain => theme::orange(),
            Intent::Move => theme::blue(),
            Intent::Clean => theme::red(),
        }
    }
}

/// One operation on the command list. The order of the variants inside each
/// intent is the order they are drawn in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum Command {
    // Investigate
    Browse,
    FindDuplicates,
    FindSimilar,
    Diff,
    Check,
    // Maintain
    Update,
    PruneMissing,
    Reorganize,
    Rename,
    Relocate,
    CloneIndex,
    Remove,
    MarkRemote,
    MakeMain,
    AddSink,
    JoinGroup,
    LeaveGroup,
    Ungroup,
    // Move
    Copy,
    Move,
    Sync,
    Mirror,
    GroupSync,
    GroupSyncBack,
    // Clean
    DedupeAgainst,
    Purge,
    EmptyDirs,
}

/// Every command, in list order.
pub(crate) const ALL: [Command; 27] = [
    Command::Browse,
    Command::FindDuplicates,
    Command::FindSimilar,
    Command::Diff,
    Command::Check,
    Command::Update,
    Command::PruneMissing,
    Command::Reorganize,
    Command::Rename,
    Command::Relocate,
    Command::CloneIndex,
    Command::Remove,
    Command::MarkRemote,
    Command::MakeMain,
    Command::AddSink,
    Command::JoinGroup,
    Command::LeaveGroup,
    Command::Ungroup,
    Command::Copy,
    Command::Move,
    Command::Sync,
    Command::Mirror,
    Command::GroupSync,
    Command::GroupSyncBack,
    Command::DedupeAgainst,
    Command::Purge,
    Command::EmptyDirs,
];

impl Command {
    pub(crate) fn intent(self) -> Intent {
        match self {
            Command::Browse
            | Command::FindDuplicates
            | Command::FindSimilar
            | Command::Diff
            | Command::Check => Intent::Investigate,
            Command::Update
            | Command::PruneMissing
            | Command::Reorganize
            | Command::Rename
            | Command::Relocate
            | Command::CloneIndex
            | Command::Remove
            | Command::MarkRemote
            | Command::MakeMain
            | Command::AddSink
            | Command::JoinGroup
            | Command::LeaveGroup
            | Command::Ungroup => Intent::Maintain,
            Command::Copy
            | Command::Move
            | Command::Sync
            | Command::Mirror
            | Command::GroupSync
            | Command::GroupSyncBack => Intent::Move,
            Command::DedupeAgainst | Command::Purge | Command::EmptyDirs => Intent::Clean,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Command::Browse => "BROWSE",
            Command::FindDuplicates => "FIND DUPLICATES",
            Command::FindSimilar => "FIND SIMILAR",
            Command::Diff => "DIFF",
            Command::Check => "CHECK",
            Command::Update => "UPDATE",
            Command::PruneMissing => "PRUNE MISSING",
            Command::Reorganize => "REORGANIZE",
            Command::Rename => "RENAME",
            Command::Relocate => "RELOCATE",
            Command::CloneIndex => "CLONE INDEX",
            Command::Remove => "REMOVE",
            Command::MarkRemote => "MARK REMOTE",
            Command::MakeMain => "MAKE MAIN",
            Command::AddSink => "ADD BACKUP",
            Command::JoinGroup => "JOIN GROUP",
            Command::LeaveGroup => "LEAVE GROUP",
            Command::Ungroup => "UNGROUP",
            Command::Copy => "COPY",
            Command::Move => "MOVE",
            Command::Sync => "SYNC",
            Command::Mirror => "MIRROR",
            Command::GroupSync => "GROUP SYNC",
            Command::GroupSyncBack => "GROUP SYNC BACK",
            Command::DedupeAgainst => "DEDUPE AGAINST",
            Command::Purge => "PURGE BY FILTER",
            Command::EmptyDirs => "EMPTY DIRECTORIES",
        }
    }

    /// The short and the verbose hover text, for an enabled command.
    fn tooltip(self) -> (&'static str, &'static str) {
        match self {
            Command::Browse => (
                "Walk the repository's files by folder",
                "Walk the selected repository's indexed files as a folder tree, preview \
                 them, and annotate them with tags.",
            ),
            Command::FindDuplicates => (
                "Find files whose content is identical",
                "Find exact duplicates — same size, same content — across the selected \
                 repositories, then mark and delete the worse copies.",
            ),
            Command::FindSimilar => (
                "Find files that look or sound alike",
                "Find perceptually similar images, videos, documents and audio across the \
                 selected repositories, above a threshold you set.",
            ),
            Command::Diff => (
                "Reconcile two repositories side by side",
                "Compare the two selected repositories by content or by path, and settle \
                 each difference row by row.",
            ),
            Command::Check => (
                "Report what an UPDATE would do, without writing",
                "Dry-run a scan of each selected repository: how many files are new, \
                 changed or missing compared to its index. Nothing is hashed or written.",
            ),
            Command::Update => (
                "Scan the folder and index new or changed files",
                "Walk each selected repository's folder, hash new or changed files, and \
                 mark vanished files missing. Unchanged files are skipped.",
            ),
            Command::PruneMissing => (
                "Drop records of files no longer on disk",
                "Remove the index records of files that vanished from disk and compact \
                 the index. Nothing on disk changes.",
            ),
            Command::Reorganize => (
                "Move files into new paths by rules",
                "Reorganize the repository's files in place by rule-based path templates, \
                 previewed as old path to new path before anything moves.",
            ),
            Command::Rename => (
                "Rename this repository",
                "Rename the repository's registry entry. The on-disk folder is not moved.",
            ),
            Command::Relocate => (
                "Point this repository at a different folder",
                "Point the repository at a different on-disk folder while keeping its \
                 index — after moving the data to a new location.",
            ),
            Command::CloneIndex => (
                "Copy this repository's index into a new one",
                "Clone the repository's entire index into a new repository at a new path. \
                 The source is left unchanged.",
            ),
            Command::Remove => (
                "Remove this repository and delete its index",
                "Remove the repository's registry entry and its index database. The files \
                 on disk are never touched.",
            ),
            Command::MarkRemote => (
                "Flag this repository as living on a slow mount",
                "Flip the repository's remote flag. A remote repository is skipped when \
                 you update only the local ones.",
            ),
            Command::MakeMain => (
                "Make this repository a backup group's main",
                "Turn the repository into the main of a new backup group, so backups can \
                 be added that it is pushed to.",
            ),
            Command::AddSink => (
                "Add a new backup to this group",
                "Add a backup to this main's group: a clone of the main's index, pointed \
                 at a new folder.",
            ),
            Command::JoinGroup => (
                "Make the ungrouped repository a backup of the main",
                "Add the selected ungrouped repository to the selected main's group as a \
                 backup.",
            ),
            Command::LeaveGroup => (
                "Take this backup out of its group",
                "Remove the repository from its backup group. Both repositories stay; \
                 they are just no longer linked.",
            ),
            Command::Ungroup => (
                "Disband this main's group",
                "Disband the group. Every repository stays; they are just no longer \
                 linked as main and backups.",
            ),
            Command::Copy => (
                "Copy content into another repository or a folder",
                "Copy the files one repository holds that the other lacks, or export \
                 them into a folder on disk.",
            ),
            Command::Move => (
                "Move content into another repository or a folder",
                "Move the files one repository holds that the other lacks, deleting them \
                 from the source once they are safely copied.",
            ),
            Command::Sync => (
                "Add to the target what the source has",
                "Copy into the target every file the source has and the target lacks; \
                 nothing is deleted unless you ask.",
            ),
            Command::Mirror => (
                "Make the target hold exactly the source's content",
                "Copy what the target lacks and delete what the source does not have, so \
                 the target converges on the source.",
            ),
            Command::GroupSync => (
                "Push the main to its backups",
                "Push the group's main to the selected backups, each in its own stored \
                 mode (add only, apply changes, or mirror).",
            ),
            Command::GroupSyncBack => (
                "Pull a backup's changes into the main",
                "Pull one backup back into its main: promote content the main never had, \
                 and offer to resurrect what the main deleted.",
            ),
            Command::DedupeAgainst => (
                "Delete files whose content another repository already holds",
                "Delete from one repository every file whose content any of the other \
                 selected repositories already holds.",
            ),
            Command::Purge => (
                "Delete every file matching a filter",
                "Delete every file in the repository that matches a filter you write, \
                 previewed before anything is deleted.",
            ),
            Command::EmptyDirs => (
                "Remove empty directories",
                "Delete every directory in the repository that holds no files.",
            ),
        }
    }
}

/// What the rules need to know about one repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepoFact {
    pub name: String,
    /// The session lock: nothing may delete or overwrite in a locked repo.
    pub locked: bool,
    /// Its folder is reachable, as of the last status probe (unknown counts
    /// as reachable; a scan then finds out).
    pub reachable: bool,
    /// The group this repository is the main of, if any.
    pub main_of: Option<String>,
    /// The group this repository is a backup (sink) of, if any.
    pub sink_of: Option<String>,
}

impl RepoFact {
    fn grouped(&self) -> bool {
        self.main_of.is_some() || self.sink_of.is_some()
    }
}

/// Whether `cmd` can run on `selected`, or why not. The reason is the hover
/// text of the greyed command, so it is end-user copy.
pub(crate) fn availability(cmd: Command, selected: &[&RepoFact]) -> Result<(), String> {
    let n = selected.len();
    if n == 0 {
        return Err("Select a repository first".into());
    }
    let sinks: Vec<&RepoFact> = selected
        .iter()
        .copied()
        .filter(|r| r.sink_of.is_some())
        .collect();
    let not_sinks: Vec<&RepoFact> = selected
        .iter()
        .copied()
        .filter(|r| r.sink_of.is_none())
        .collect();
    let exactly_one = || -> Result<&RepoFact, String> {
        if n == 1 {
            Ok(selected[0])
        } else {
            Err("Needs exactly one repository".into())
        }
    };
    let not_a_sink = |r: &RepoFact| -> Result<(), String> {
        match &r.sink_of {
            Some(group) => Err(format!(
                "'{}' is a backup in group '{group}' — work through the group's main",
                r.name
            )),
            None => Ok(()),
        }
    };
    let unlocked = |r: &RepoFact| -> Result<(), String> {
        if r.locked {
            Err(format!("'{}' is locked — unlock it on its card", r.name))
        } else {
            Ok(())
        }
    };
    let reachable = |r: &RepoFact| -> Result<(), String> {
        if r.reachable {
            Ok(())
        } else {
            Err(format!("'{}' is unreachable", r.name))
        }
    };
    let two_outside_groups = || -> Result<(), String> {
        if n != 2 {
            return Err("Needs exactly two repositories".into());
        }
        for r in selected {
            not_a_sink(r)?;
        }
        Ok(())
    };
    // The selection is one backup group: its main plus some of its sinks and
    // nothing else. Returns the main and the selected sinks.
    let one_group = || -> Result<(&RepoFact, Vec<&RepoFact>), String> {
        let mains: Vec<&RepoFact> = selected
            .iter()
            .copied()
            .filter(|r| r.main_of.is_some())
            .collect();
        let [main] = mains.as_slice() else {
            return Err("Needs a backup group: its main and at least one of its backups".into());
        };
        let group = main.main_of.as_deref().unwrap_or_default();
        let members: Vec<&RepoFact> = selected
            .iter()
            .copied()
            .filter(|r| r.sink_of.as_deref() == Some(group))
            .collect();
        if members.is_empty() || members.len() + 1 != n {
            return Err("Needs a backup group: its main and at least one of its backups".into());
        }
        Ok((*main, members))
    };

    match cmd {
        Command::Browse => {
            exactly_one()?;
            Ok(())
        }
        Command::FindDuplicates | Command::FindSimilar => {
            if not_sinks.is_empty() {
                let names: Vec<&str> = sinks.iter().map(|r| r.name.as_str()).collect();
                return Err(format!(
                    "{} backups are searched through their group's main",
                    names.join(", ")
                ));
            }
            Ok(())
        }
        Command::Diff | Command::Sync | Command::Mirror => two_outside_groups(),
        Command::Check | Command::Update => {
            for r in selected {
                reachable(r)?;
            }
            Ok(())
        }
        Command::PruneMissing | Command::Reorganize => {
            let r = exactly_one()?;
            not_a_sink(r)
        }
        Command::Rename
        | Command::Relocate
        | Command::CloneIndex
        | Command::Remove
        | Command::MarkRemote => {
            exactly_one()?;
            Ok(())
        }
        Command::MakeMain => {
            let r = exactly_one()?;
            if r.grouped() {
                Err(format!("'{}' is already in a backup group", r.name))
            } else {
                Ok(())
            }
        }
        Command::AddSink | Command::Ungroup => {
            let r = exactly_one()?;
            if r.main_of.is_some() {
                Ok(())
            } else {
                Err("Needs a backup group's main".into())
            }
        }
        Command::JoinGroup => {
            if n != 2 {
                return Err("Needs a group's main and one ungrouped repository".into());
            }
            let has_main = selected.iter().any(|r| r.main_of.is_some());
            let has_free = selected.iter().any(|r| !r.grouped());
            if has_main && has_free {
                Ok(())
            } else {
                Err("Needs a group's main and one ungrouped repository".into())
            }
        }
        Command::LeaveGroup => {
            let r = exactly_one()?;
            if r.sink_of.is_some() {
                Ok(())
            } else {
                Err("Needs a backup (a repository inside a group)".into())
            }
        }
        Command::Copy | Command::Move => {
            if n > 2 {
                return Err("Needs one repository (to a folder) or two (to a repository)".into());
            }
            for r in selected {
                not_a_sink(r)?;
            }
            Ok(())
        }
        Command::GroupSync => {
            one_group()?;
            Ok(())
        }
        Command::GroupSyncBack => {
            let (_, members) = one_group()?;
            if members.len() == 1 {
                Ok(())
            } else {
                Err("Needs a group's main and exactly one of its backups".into())
            }
        }
        Command::DedupeAgainst => {
            if n < 2 {
                return Err("Needs at least two repositories".into());
            }
            for r in selected {
                not_a_sink(r)?;
            }
            Ok(())
        }
        Command::Purge | Command::EmptyDirs => {
            let r = exactly_one()?;
            not_a_sink(r)?;
            unlocked(r)
        }
    }
}

/// A command and the inputs the start page collects for it (ADR 0004): what
/// a favorite saves, and what a body is handed.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Spec {
    pub cmd: Command,
    /// A directed command's source among the selection; the rest of the
    /// selection are its targets (DEDUPE AGAINST's pool).
    pub source: Option<String>,
    /// FIND SIMILAR's threshold, in percent.
    pub threshold: Option<f64>,
}

impl Spec {
    pub(crate) fn new(cmd: Command) -> Self {
        Self {
            cmd,
            source: None,
            threshold: None,
        }
    }
}

/// Whether `cmd` on `selected` needs one of them named as the source. A
/// backup group names its own; a DIFF has no direction; a single repository
/// is the source of a folder export.
pub(crate) fn needs_source(cmd: Command, selected: &[&RepoFact]) -> bool {
    match cmd {
        Command::Copy | Command::Move | Command::Sync | Command::Mirror => selected.len() == 2,
        Command::DedupeAgainst => true,
        _ => false,
    }
}

/// Whether `cmd` needs a similarity threshold.
pub(crate) fn needs_threshold(cmd: Command) -> bool {
    cmd == Command::FindSimilar
}

/// Whether `cmd` has anything left to ask on the start page before it runs.
pub(crate) fn needs_inputs(cmd: Command, selected: &[&RepoFact]) -> bool {
    needs_source(cmd, selected) || needs_threshold(cmd)
}

/// Whether every input `spec` needs has an answer.
pub(crate) fn ready(spec: &Spec, selected: &[&RepoFact]) -> bool {
    let source_ok = !needs_source(spec.cmd, selected)
        || spec
            .source
            .as_deref()
            .is_some_and(|s| selected.iter().any(|r| r.name == s));
    let threshold_ok = !needs_threshold(spec.cmd) || spec.threshold.is_some();
    source_ok && threshold_ok
}

/// The inputs as the header strip says them: "from A to B, C", "at 90 %".
pub(crate) fn inputs_summary(spec: &Spec, selected: &[&RepoFact]) -> String {
    let mut parts = Vec::new();
    if let Some(src) = &spec.source {
        let targets: Vec<&str> = selected
            .iter()
            .map(|r| r.name.as_str())
            .filter(|n| n != src)
            .collect();
        parts.push(format!("from {src} to {}", targets.join(", ")));
    }
    if let Some(t) = spec.threshold {
        parts.push(format!("at {t:.0} %"));
    }
    parts.join(" · ")
}

/// A fully specified command saved under a name (ADR 0004): the selection
/// and the spec, reopened in one click. It saves the specification, never the
/// confirmation — it still lands on the preview.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Favorite {
    pub name: String,
    pub repos: Vec<String>,
    pub spec: Spec,
}

/// Whether `fav` can run today, or why not: a repository it names may be
/// gone, the selection may no longer fit the command (a group dissolved, a
/// repository locked), or an input may be missing.
pub(crate) fn favorite_state(fav: &Favorite, facts: &[RepoFact]) -> Result<(), String> {
    let mut selected = Vec::new();
    for name in &fav.repos {
        match facts.iter().find(|f| &f.name == name) {
            Some(f) => selected.push(f),
            None => return Err(format!("repository '{name}' no longer exists")),
        }
    }
    availability(fav.spec.cmd, &selected)?;
    if ready(&fav.spec, &selected) {
        Ok(())
    } else {
        Err("an input is missing — open the command again".into())
    }
}

/// What a click on the command list asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Pick {
    /// A command was chosen.
    Command(Command),
    /// The pending command's GO: its inputs are complete.
    Go,
}

/// Draw the command list for `selected`; returns what was clicked, if
/// anything. Every command is drawn every time, in the same place; an
/// unavailable one is disabled with its reason as the hover text. The
/// `pending` command's inputs are drawn right under its bar, with GO.
pub(crate) fn command_panel(
    ui: &mut egui::Ui,
    selected: &[&RepoFact],
    pending: Option<&mut Spec>,
    verbosity: TooltipVerbosity,
) -> Option<Pick> {
    let mut picked = None;
    let mut pending = pending;
    for intent in [
        Intent::Investigate,
        Intent::Maintain,
        Intent::Move,
        Intent::Clean,
    ] {
        ui.add_space(4.0);
        ui.label(
            RichText::new(intent.label())
                .color(intent.accent())
                .size(13.0)
                .strong(),
        );
        for cmd in ALL.iter().copied().filter(|c| c.intent() == intent) {
            let state = availability(cmd, selected);
            let enabled = state.is_ok();
            let is_pending = pending.as_ref().is_some_and(|p| p.cmd == cmd);
            let resp = if is_pending {
                crate::lcars::toggle_button(ui, cmd.label(), true, intent.accent())
            } else {
                crate::lcars::action_button(ui, cmd.label(), enabled, intent.accent())
            };
            let resp = match &state {
                Ok(()) => {
                    let (short, long) = cmd.tooltip();
                    resp.explain(verbosity, short, long)
                }
                Err(reason) => resp.on_hover_text(reason.as_str()),
            };
            if resp.clicked() {
                picked = Some(Pick::Command(cmd));
            }
            if is_pending
                && let Some(spec) = pending.as_mut()
                && inputs_box(ui, spec, selected, intent.accent(), verbosity)
            {
                picked = Some(Pick::Go);
            }
        }
    }
    picked
}

/// The pending command's remaining inputs, then GO. Returns whether GO was
/// clicked.
fn inputs_box(
    ui: &mut egui::Ui,
    spec: &mut Spec,
    selected: &[&RepoFact],
    accent: Color32,
    verbosity: TooltipVerbosity,
) -> bool {
    let mut go = false;
    egui::Frame::new()
        .fill(theme::panel())
        .stroke(egui::Stroke::new(1.0, accent))
        .corner_radius(8)
        .inner_margin(8.0)
        .show(ui, |ui| {
            if needs_source(spec.cmd, selected) {
                let (from_hint, to_label) = if spec.cmd == Command::DedupeAgainst {
                    (
                        "Click the repository whose duplicates are deleted",
                        "KEEP IN",
                    )
                } else {
                    ("Click the repository the content comes from", "TO")
                };
                ui.label(RichText::new("FROM").color(theme::tan()).size(11.0));
                ui.horizontal_wrapped(|ui| {
                    for r in selected {
                        let is_src = spec.source.as_deref() == Some(r.name.as_str());
                        let chip = crate::repo_chip::repo_chip(
                            ui,
                            &r.name,
                            is_src,
                            accent,
                            r.main_of.is_some(),
                            None,
                        );
                        if chip.name.explain(verbosity, from_hint, from_hint).clicked() {
                            spec.source = Some(r.name.clone());
                        }
                    }
                });
                if let Some(src) = &spec.source {
                    let targets: Vec<&str> = selected
                        .iter()
                        .map(|r| r.name.as_str())
                        .filter(|n| n != src)
                        .collect();
                    ui.label(
                        RichText::new(format!("{to_label} {}", targets.join(", ")))
                            .color(theme::text())
                            .size(12.0),
                    );
                }
            }
            if needs_threshold(spec.cmd) {
                let mut t = spec.threshold.unwrap_or(99.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("THRESHOLD").color(theme::tan()).size(11.0));
                    ui.add(egui::Slider::new(&mut t, 50.0..=100.0).suffix(" %"))
                        .explain(
                            verbosity,
                            "How alike two files must be",
                            "Similarity in percent: lower catches more, and riskier, matches.",
                        );
                });
                spec.threshold = Some(t);
            }
            ui.add_space(4.0);
            if crate::lcars::action_button(ui, "GO", ready(spec, selected), accent)
                .explain(
                    verbosity,
                    "Run this command on the selection",
                    "Open the command with these repositories and inputs.",
                )
                .clicked()
            {
                go = true;
            }
        });
    go
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free(name: &str) -> RepoFact {
        RepoFact {
            name: name.into(),
            locked: true,
            reachable: true,
            main_of: None,
            sink_of: None,
        }
    }

    fn main(name: &str, group: &str) -> RepoFact {
        RepoFact {
            main_of: Some(group.into()),
            ..free(name)
        }
    }

    fn sink(name: &str, group: &str) -> RepoFact {
        RepoFact {
            sink_of: Some(group.into()),
            ..free(name)
        }
    }

    fn ok(cmd: Command, sel: &[&RepoFact]) -> bool {
        availability(cmd, sel).is_ok()
    }

    fn reason(cmd: Command, sel: &[&RepoFact]) -> String {
        availability(cmd, sel).err().unwrap_or_default()
    }

    #[test]
    fn nothing_selected_greys_everything_with_one_reason() {
        for cmd in ALL {
            assert_eq!(reason(cmd, &[]), "Select a repository first", "{cmd:?}");
        }
    }

    #[test]
    fn one_unlocked_repo_enables_single_repo_commands() {
        let mut a = free("a");
        a.locked = false;
        let sel = [&a];
        for cmd in [
            Command::Browse,
            Command::Check,
            Command::Update,
            Command::PruneMissing,
            Command::Reorganize,
            Command::Rename,
            Command::Relocate,
            Command::CloneIndex,
            Command::Remove,
            Command::MarkRemote,
            Command::MakeMain,
            Command::Copy,
            Command::Move,
            Command::Purge,
            Command::EmptyDirs,
            Command::FindDuplicates,
            Command::FindSimilar,
        ] {
            assert!(
                ok(cmd, &sel),
                "{cmd:?} should be enabled for one unlocked repo"
            );
        }
        for cmd in [Command::Diff, Command::Sync, Command::Mirror] {
            assert_eq!(
                reason(cmd, &sel),
                "Needs exactly two repositories",
                "{cmd:?}"
            );
        }
        assert_eq!(
            reason(Command::DedupeAgainst, &sel),
            "Needs at least two repositories"
        );
        assert!(reason(Command::GroupSync, &sel).starts_with("Needs a backup group"));
    }

    #[test]
    fn a_locked_repo_greys_the_deleting_commands_with_the_lock_reason() {
        let a = free("a");
        let sel = [&a];
        for cmd in [Command::Purge, Command::EmptyDirs] {
            assert_eq!(
                reason(cmd, &sel),
                "'a' is locked — unlock it on its card",
                "{cmd:?}"
            );
        }
        // The lock guards existing files; everything else stays open.
        for cmd in [
            Command::Browse,
            Command::Update,
            Command::Copy,
            Command::Remove,
        ] {
            assert!(ok(cmd, &sel), "{cmd:?}");
        }
    }

    #[test]
    fn two_repos_enable_pair_commands_and_grey_single_ones() {
        let a = free("a");
        let b = free("b");
        let sel = [&a, &b];
        for cmd in [
            Command::Diff,
            Command::Sync,
            Command::Mirror,
            Command::Copy,
            Command::Move,
            Command::DedupeAgainst,
            Command::FindDuplicates,
            Command::Check,
        ] {
            assert!(ok(cmd, &sel), "{cmd:?}");
        }
        for cmd in [Command::Browse, Command::Rename, Command::Reorganize] {
            assert_eq!(reason(cmd, &sel), "Needs exactly one repository", "{cmd:?}");
        }
    }

    #[test]
    fn three_repos_grey_the_pair_commands() {
        let a = free("a");
        let b = free("b");
        let c = free("c");
        let sel = [&a, &b, &c];
        assert_eq!(
            reason(Command::Diff, &sel),
            "Needs exactly two repositories"
        );
        assert!(reason(Command::Copy, &sel).starts_with("Needs one repository"));
        assert!(ok(Command::DedupeAgainst, &sel));
    }

    #[test]
    fn a_whole_group_enables_group_sync_only() {
        let m = main("m", "g");
        let s1 = sink("s1", "g");
        let s2 = sink("s2", "g");
        let sel = [&m, &s1, &s2];
        assert!(ok(Command::GroupSync, &sel));
        assert_eq!(
            reason(Command::GroupSyncBack, &sel),
            "Needs a group's main and exactly one of its backups"
        );
        assert!(ok(Command::GroupSyncBack, &[&m, &s1]));
        // A sink cannot be a diff side; the rule names it.
        assert!(reason(Command::Diff, &[&m, &s1]).contains("backup in group 'g'"));
        // A stray repo breaks the group shape.
        let x = free("x");
        assert!(reason(Command::GroupSync, &[&m, &s1, &x]).starts_with("Needs a backup group"));
    }

    #[test]
    fn group_membership_commands_follow_the_roles() {
        let m = main("m", "g");
        let s = sink("s", "g");
        let x = free("x");
        assert!(ok(Command::AddSink, &[&m]));
        assert!(ok(Command::Ungroup, &[&m]));
        assert_eq!(
            reason(Command::AddSink, &[&x]),
            "Needs a backup group's main"
        );
        assert!(ok(Command::MakeMain, &[&x]));
        assert_eq!(
            reason(Command::MakeMain, &[&m]),
            "'m' is already in a backup group"
        );
        assert!(ok(Command::LeaveGroup, &[&s]));
        assert!(reason(Command::LeaveGroup, &[&x]).starts_with("Needs a backup"));
        assert!(ok(Command::JoinGroup, &[&m, &x]));
        assert!(reason(Command::JoinGroup, &[&m, &s]).starts_with("Needs a group's main"));
    }

    #[test]
    fn sinks_alone_cannot_be_searched_but_say_where_to_go() {
        let s = sink("s", "g");
        assert_eq!(
            reason(Command::FindDuplicates, &[&s]),
            "s backups are searched through their group's main"
        );
        let x = free("x");
        assert!(ok(Command::FindDuplicates, &[&s, &x]));
    }

    #[test]
    fn unreachable_repos_cannot_be_scanned() {
        let mut a = free("a");
        a.reachable = false;
        assert_eq!(reason(Command::Update, &[&a]), "'a' is unreachable");
        assert_eq!(reason(Command::Check, &[&a]), "'a' is unreachable");
        assert!(ok(Command::Browse, &[&a]));
    }

    #[test]
    fn a_pair_needs_a_source_a_group_or_a_diff_does_not() {
        let a = free("a");
        let b = free("b");
        let pair = [&a, &b];
        for cmd in [Command::Copy, Command::Move, Command::Sync, Command::Mirror] {
            assert!(needs_source(cmd, &pair), "{cmd:?}");
            assert!(
                !needs_source(cmd, &[&a]),
                "{cmd:?} on one repo exports to a folder"
            );
        }
        assert!(!needs_source(Command::Diff, &pair));
        assert!(needs_source(Command::DedupeAgainst, &pair));
        let m = main("m", "g");
        let s = sink("s", "g");
        assert!(!needs_source(Command::GroupSync, &[&m, &s]));
        assert!(!needs_inputs(Command::Browse, &[&a]));
        assert!(needs_inputs(Command::FindSimilar, &[&a]));
    }

    #[test]
    fn ready_once_every_input_has_an_answer() {
        let a = free("a");
        let b = free("b");
        let pair = [&a, &b];
        let mut spec = Spec::new(Command::Copy);
        assert!(!ready(&spec, &pair));
        spec.source = Some("zzz".into());
        assert!(
            !ready(&spec, &pair),
            "a source outside the selection does not count"
        );
        spec.source = Some("b".into());
        assert!(ready(&spec, &pair));
        assert_eq!(inputs_summary(&spec, &pair), "from b to a");

        let mut similar = Spec::new(Command::FindSimilar);
        assert!(!ready(&similar, &[&a]));
        similar.threshold = Some(90.0);
        assert!(ready(&similar, &[&a]));
        assert_eq!(inputs_summary(&similar, &[&a]), "at 90 %");
        assert!(ready(&Spec::new(Command::Browse), &[&a]));
    }

    #[test]
    fn a_favorite_is_greyed_when_its_repo_is_gone_or_the_rule_fails() {
        let a = free("a");
        let b = free("b");
        let facts = vec![a.clone(), b.clone()];
        let mut spec = Spec::new(Command::Copy);
        spec.source = Some("a".into());
        let fav = Favorite {
            name: "a to b".into(),
            repos: vec!["a".into(), "b".into()],
            spec: spec.clone(),
        };
        assert_eq!(favorite_state(&fav, &facts), Ok(()));
        let gone = Favorite {
            repos: vec!["a".into(), "zzz".into()],
            ..fav.clone()
        };
        assert_eq!(
            favorite_state(&gone, &facts),
            Err("repository 'zzz' no longer exists".into())
        );
        let locked = Favorite {
            name: "purge a".into(),
            repos: vec!["a".into()],
            spec: Spec::new(Command::Purge),
        };
        assert_eq!(
            favorite_state(&locked, &facts),
            Err("'a' is locked — unlock it on its card".into())
        );
        let unanswered = Favorite {
            spec: Spec::new(Command::Copy),
            ..fav
        };
        assert!(favorite_state(&unanswered, &facts).is_err());
    }

    #[test]
    fn every_command_has_an_intent_and_the_list_is_complete() {
        for intent in [
            Intent::Investigate,
            Intent::Maintain,
            Intent::Move,
            Intent::Clean,
        ] {
            assert!(
                ALL.iter().any(|c| c.intent() == intent),
                "{intent:?} is empty"
            );
        }
        let mut seen = std::collections::HashSet::new();
        for cmd in ALL {
            assert!(seen.insert(cmd), "{cmd:?} listed twice");
        }
    }
}
