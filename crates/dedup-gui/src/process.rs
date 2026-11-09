//! Pieces every process page shares (ADR 0006): the frames repositories are
//! dropped into, and the preview window that locks the page until GO or
//! CANCEL.

use crate::theme;
use egui::RichText;

/// A frame repositories can be dropped onto: the panel's own colour, outlined
/// in `accent` while a repository is dragged over it. Returns the dropped
/// repository's name.
pub fn drop_zone(
    ui: &mut egui::Ui,
    accent: egui::Color32,
    add: impl FnOnce(&mut egui::Ui),
) -> Option<String> {
    let response = egui::Frame::new()
        .fill(theme::panel())
        .corner_radius(6.0)
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            add(ui);
        })
        .response;
    if response.dnd_hover_payload::<String>().is_some() {
        ui.painter().rect_stroke(
            response.rect,
            6.0,
            egui::Stroke::new(2.0, accent),
            egui::StrokeKind::Inside,
        );
    }
    response
        .dnd_release_payload::<String>()
        .map(|name| (*name).clone())
}

/// Show `body` as a page's preview: a real window of its own, which locks the
/// main window until it closes. Returns whether the user closed the window,
/// which counts as CANCEL. A backend without separate windows draws it as a
/// window inside the main one, with no lock.
pub fn preview_window(
    ctx: &egui::Context,
    key: &str,
    page: &str,
    mut body: impl FnMut(&mut egui::Ui),
) -> bool {
    let closed = ctx.show_viewport_immediate(
        egui::ViewportId::from_hash_of(key),
        egui::ViewportBuilder::default()
            .with_title(format!("dedup — {page} preview"))
            .with_inner_size([1100.0, 800.0]),
        |ui, _class| {
            egui::CentralPanel::default().show(ui, |ui| body(ui));
            ui.input(|i| i.viewport().close_requested())
        },
    );
    if !ctx.embed_viewports() {
        egui::Modal::new(egui::Id::unique(("preview-lock", key))).show(ctx, |ui| {
            ui.label(
                RichText::new("The preview is open in its own window — GO or CANCEL there.")
                    .color(theme::text()),
            );
        });
    }
    closed
}

/// One role card of a process page (ADR 0006).
pub struct CardSpec {
    pub title: &'static str,
    /// What happens to a repository in this card, for the hover text.
    pub hover: &'static str,
    pub accent: egui::Color32,
    /// The lock the card sets: `Some(true)` locks, `Some(false)` unlocks,
    /// `None` leaves it alone.
    pub lock: Option<bool>,
    /// The card holds one repository; a second one dropped in replaces it.
    pub single: bool,
}

/// The pool and the role cards of a process page: which repository sits in
/// which card. A card sets the lock it states; a repository whose lock
/// changes elsewhere so it no longer fits its card falls back to the pool.
pub struct RoleCards {
    specs: Vec<CardSpec>,
    /// `(repository, card index)`, in the order they were placed.
    placed: Vec<(String, usize)>,
    notice: Option<String>,
}

impl RoleCards {
    pub fn new(specs: Vec<CardSpec>) -> Self {
        Self {
            specs,
            placed: Vec::new(),
            notice: None,
        }
    }

    /// The repositories in card `card`, in the order they were placed.
    pub fn members(&self, card: usize) -> Vec<String> {
        self.placed
            .iter()
            .filter(|(_, c)| *c == card)
            .map(|(r, _)| r.clone())
            .collect()
    }

    /// Put `repo` into `card`, or back into the pool (`None`), setting the
    /// lock the card states.
    pub fn place(&mut self, locks: &crate::locks::RepoLocks, repo: &str, card: Option<usize>) {
        self.placed.retain(|(r, _)| r != repo);
        let Some(card) = card else { return };
        let Some(spec) = self.specs.get(card) else {
            return;
        };
        if spec.single {
            self.placed.retain(|(_, c)| *c != card);
        }
        if let Some(read_only) = spec.lock {
            locks.set_read_only(repo, read_only);
        }
        self.placed.push((repo.to_string(), card));
        self.notice = None;
    }

    /// Drop repositories that no longer exist, and send back to the pool
    /// every one whose lock was changed elsewhere against its card.
    pub fn follow(&mut self, locks: &crate::locks::RepoLocks, existing: &[String]) {
        self.placed.retain(|(r, _)| existing.contains(r));
        let misfits: Vec<(String, usize)> = self
            .placed
            .iter()
            .filter(|(r, c)| {
                self.specs
                    .get(*c)
                    .and_then(|s| s.lock)
                    .is_some_and(|ro| locks.read_only(r) != ro)
            })
            .cloned()
            .collect();
        for (repo, card) in misfits {
            self.placed.retain(|(r, _)| *r != repo);
            let title = self.specs.get(card).map_or("", |s| s.title);
            self.notice = Some(format!(
                "'{repo}' went back to the pool: its lock was changed, so it no longer fits {title}"
            ));
        }
    }

    #[cfg(test)]
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// Draw the pool (every repository in no card) and the cards, and apply
    /// drags, drops and (x) clicks. `repos` are `(name, is_main)`.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        id: &str,
        repos: &[(String, bool)],
        locks: &crate::locks::RepoLocks,
        verbosity: crate::settings::TooltipVerbosity,
    ) {
        use crate::util::ExplainExt;
        let mut moves: Vec<(String, Option<usize>)> = Vec::new();
        let pool: Vec<&(String, bool)> = repos
            .iter()
            .filter(|(r, _)| !self.placed.iter().any(|(p, _)| p == r))
            .collect();
        crate::lcars::frame_lcars(
            ui,
            "REPOSITORIES — DRAG INTO A CARD BELOW",
            theme::lilac(),
            |ui| {
                let dropped = drop_zone(ui, theme::lilac(), |ui| {
                    if pool.is_empty() {
                        ui.label(
                            RichText::new("Every repository is in a card.")
                                .color(theme::tan())
                                .size(12.0),
                        );
                    }
                    ui.horizontal_wrapped(|ui| {
                        for (name, is_main) in &pool {
                            ui.dnd_drag_source(
                                egui::Id::unique((id, "pool", name)),
                                name.clone(),
                                |ui| {
                                    crate::repo_chip::repo_chip(
                                        ui,
                                        name,
                                        false,
                                        theme::orange(),
                                        *is_main,
                                        None,
                                    );
                                },
                            )
                            .response
                            .explain(
                                verbosity,
                                "Drag into a card below",
                                "Drag into a card below.",
                            );
                        }
                    });
                });
                if let Some(name) = dropped {
                    moves.push((name, None));
                }
            },
        );
        let per_row = self.specs.len().clamp(1, 3);
        ui.columns(per_row, |cols| {
            for (card, spec) in self.specs.iter().enumerate() {
                let Some(ui) = cols.get_mut(card.checked_rem(per_row).unwrap_or(0)) else {
                    continue;
                };
                let members: Vec<(String, bool)> = repos
                    .iter()
                    .filter(|(r, _)| self.placed.iter().any(|(p, c)| p == r && *c == card))
                    .cloned()
                    .collect();
                let ((), header) = crate::lcars::frame_lcars(ui, spec.title, spec.accent, |ui| {
                    let dropped = drop_zone(ui, spec.accent, |ui| {
                        ui.set_min_height(48.0);
                        if members.is_empty() {
                            ui.label(
                                RichText::new(if spec.single {
                                    "Drop one repository here"
                                } else {
                                    "Drop repositories here"
                                })
                                .color(theme::tan())
                                .size(12.0),
                            );
                        }
                        ui.horizontal_wrapped(|ui| {
                            for (name, is_main) in &members {
                                ui.dnd_drag_source(
                                    egui::Id::unique((id, "card", name)),
                                    name.clone(),
                                    |ui| {
                                        ui.horizontal(|ui| {
                                            crate::repo_chip::repo_chip(
                                                ui,
                                                name,
                                                true,
                                                theme::orange(),
                                                *is_main,
                                                Some(locks.read_only(name)),
                                            );
                                            if ui
                                                .button(
                                                    RichText::new(crate::icon::X)
                                                        .color(theme::black()),
                                                )
                                                .explain(
                                                    verbosity,
                                                    "Back to the pool",
                                                    "Put this repository back into the pool.",
                                                )
                                                .clicked()
                                            {
                                                moves.push((name.clone(), None));
                                            }
                                        });
                                    },
                                );
                            }
                        });
                    });
                    if let Some(name) = dropped {
                        moves.push((name, Some(card)));
                    }
                });
                header.explain(verbosity, spec.title, spec.hover);
            }
        });
        if let Some(notice) = &self.notice {
            ui.colored_label(theme::amber(), notice);
        }
        for (repo, card) in moves {
            self.place(locks, &repo, card);
        }
    }
}
