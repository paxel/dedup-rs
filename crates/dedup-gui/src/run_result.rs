//! The shared report a finished operation leaves behind.
//!
//! Every long-running command — a group push, a transfer, a grooming run —
//! ends the same way: some counts, possibly some individual failures, possibly
//! cancelled. Before this, each tab flattened that into one status line and
//! joined its errors into a single string, so a run that failed on forty files
//! said "40 error(s)" and the user never learned which ones.
//!
//! [`RunReport`] is that outcome as data, and [`ResultModal`] renders it the
//! same way everywhere: counts, the failures listed one per line, and a
//! headline that says plainly whether the run actually finished.

use crate::theme;
use egui::{Id, RichText};

/// How many individual failures a report keeps. A run that fails on tens of
/// thousands of files does not need every one in memory to be understood — the
/// count stays exact either way.
pub const MAX_PROBLEMS: usize = 200;

/// What one finished operation did.
pub struct RunReport {
    /// What ran, e.g. `"Sync group 'offsite'"`.
    title: String,
    /// Labelled totals, in display order.
    counts: Vec<(String, u64)>,
    /// Individual failures, capped at [`MAX_PROBLEMS`].
    problems: Vec<String>,
    /// Failures beyond the cap.
    problems_dropped: u64,
    /// Anything else the user must know — which sinks are now stale, say.
    notes: Vec<String>,
    cancelled: bool,
}

impl RunReport {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            counts: Vec::new(),
            problems: Vec::new(),
            problems_dropped: 0,
            notes: Vec::new(),
            cancelled: false,
        }
    }

    /// Add a total. Zero counts are kept: "deleted 0" is worth seeing on a
    /// mirror the user expected to delete something.
    pub fn count(mut self, label: impl Into<String>, value: u64) -> Self {
        self.counts.push((label.into(), value));
        self
    }

    pub fn cancelled(mut self, cancelled: bool) -> Self {
        self.cancelled = cancelled;
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn problem(&mut self, problem: impl Into<String>) {
        if self.problems.len() < MAX_PROBLEMS {
            self.problems.push(problem.into());
        } else {
            self.problems_dropped += 1;
        }
    }

    pub fn problems(mut self, problems: impl IntoIterator<Item = String>) -> Self {
        for problem in problems {
            self.problem(problem);
        }
        self
    }

    /// True when the run did everything it set out to do.
    pub fn complete(&self) -> bool {
        !self.cancelled && self.problems.is_empty() && self.problems_dropped == 0
    }

    /// How many individual failures are known (including any past the cap).
    pub fn problem_count(&self) -> u64 {
        self.total_problems()
    }

    fn total_problems(&self) -> u64 {
        self.problems.len() as u64 + self.problems_dropped
    }

    /// The one-line version, for the tab's status strip. Says "incomplete"
    /// rather than "done" whenever anything went wrong — a run that half
    /// happened must never read like one that finished.
    pub fn headline(&self) -> String {
        let counts = self
            .counts
            .iter()
            .map(|(label, value)| format!("{label} {value}"))
            .collect::<Vec<_>>()
            .join(", ");
        if self.complete() {
            return format!("{} done: {counts}.", self.title);
        }
        let mut why = Vec::new();
        if self.cancelled {
            why.push("cancelled".to_string());
        }
        if self.total_problems() > 0 {
            why.push(format!("{} problem(s)", self.total_problems()));
        }
        format!("{} incomplete — {counts}; {}.", self.title, why.join(", "))
    }

    fn accent(&self) -> egui::Color32 {
        if self.total_problems() > 0 {
            theme::RED
        } else if self.cancelled {
            theme::AMBER
        } else {
            theme::GREEN
        }
    }
}

/// Holds the report until the user dismisses it.
#[derive(Default)]
pub struct ResultModal {
    report: Option<RunReport>,
}

impl ResultModal {
    pub fn open(&mut self, report: RunReport) {
        self.report = Some(report);
    }

    pub fn close(&mut self) {
        self.report = None;
    }

    /// Render the report, if one is waiting. Returns true while it is on screen
    /// so callers can suppress their own shortcuts behind it.
    pub fn show(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(report) = &self.report else {
            return false;
        };
        let mut close = false;
        let response = egui::Modal::new(Id::new("run-result")).show(&ui.ctx().clone(), |ui| {
            ui.set_width(520.0);
            ui.label(
                RichText::new(if report.complete() {
                    "RUN COMPLETE"
                } else {
                    "RUN INCOMPLETE"
                })
                .color(report.accent())
                .size(16.0)
                .strong(),
            );
            ui.add_space(4.0);
            ui.colored_label(theme::TEXT, &report.title);
            ui.add_space(10.0);

            for (label, value) in &report.counts {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(label).color(theme::TAN).size(12.0));
                    ui.label(
                        RichText::new(value.to_string())
                            .color(theme::TEXT)
                            .size(12.0)
                            .strong(),
                    );
                });
            }

            if report.cancelled {
                ui.add_space(8.0);
                ui.colored_label(
                    theme::AMBER,
                    "Cancelled before it finished — what is listed above is all that was done.",
                );
            }
            for note in &report.notes {
                ui.add_space(6.0);
                ui.colored_label(theme::AMBER, note);
            }

            if report.total_problems() > 0 {
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!("{} PROBLEM(S)", report.total_problems()))
                        .color(theme::RED)
                        .size(13.0)
                        .strong(),
                );
                ui.add_space(4.0);
                // Listed individually, not joined: the point of the panel is
                // that the user can see which files actually failed.
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for problem in &report.problems {
                            ui.colored_label(theme::RED, problem);
                        }
                        if report.problems_dropped > 0 {
                            ui.colored_label(
                                theme::TAN,
                                format!(
                                    "…and {} more (see the session log for the full list).",
                                    report.problems_dropped
                                ),
                            );
                        }
                    });
            }

            ui.add_space(12.0);
            if ui
                .add(egui::Button::new(
                    RichText::new("CLOSE").color(theme::BLACK),
                ))
                .clicked()
            {
                close = true;
            }
        });
        if close || response.should_close() {
            self.report = None;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_run_reads_as_done() {
        let report = RunReport::new("Sync group 'offsite'")
            .count("copied", 3)
            .count("deleted", 0);
        assert!(report.complete());
        assert_eq!(
            report.headline(),
            "Sync group 'offsite' done: copied 3, deleted 0."
        );
    }

    /// The bug this whole panel exists for: a run that copied nothing and
    /// failed on everything must not read as success.
    #[test]
    fn failures_and_cancellation_make_it_incomplete() {
        let report = RunReport::new("Sync group 'offsite'")
            .count("copied", 0)
            .cancelled(true)
            .problems(["a.txt: drive gone".to_string()]);
        assert!(!report.complete());
        let headline = report.headline();
        assert!(headline.contains("incomplete"), "{headline}");
        assert!(headline.contains("cancelled"), "{headline}");
        assert!(headline.contains("1 problem(s)"), "{headline}");
    }

    /// Individual failures are kept, but a catastrophic run cannot pin the
    /// heap — the count stays exact past the cap.
    #[test]
    fn problems_are_capped_but_still_counted() {
        let report = RunReport::new("Transfer")
            .problems((0..MAX_PROBLEMS + 50).map(|i| format!("file{i}.txt: failed")));
        assert_eq!(report.problems.len(), MAX_PROBLEMS, "kept up to the cap");
        assert_eq!(report.problems_dropped, 50, "the rest are still counted");
        assert!(
            report
                .headline()
                .contains(&format!("{} problem(s)", MAX_PROBLEMS + 50))
        );
        assert!(!report.complete());
    }

    #[test]
    fn the_modal_holds_a_report_until_dismissed() {
        let mut modal = ResultModal::default();
        assert!(modal.report.is_none());
        modal.open(RunReport::new("Grooming").count("moved", 2));
        assert!(modal.report.is_some());
        modal.close();
        assert!(modal.report.is_none());
    }

    /// The panel actually lays out and shows each failure on its own line — a
    /// static assert on the rendered tree, per the headless-verification rule.
    #[test]
    fn the_panel_renders_each_failure() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;

        let report = RunReport::new("Sync group 'offsite'")
            .count("copied", 1)
            .cancelled(true)
            .problems([
                "photos/a.jpg: permission denied".to_string(),
                "docs/b.pdf: no space left".to_string(),
            ]);
        let mut init = false;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(600.0, 700.0))
            .build_ui_state(
                move |ui, modal: &mut ResultModal| {
                    if !init {
                        crate::theme::apply(ui.ctx());
                        init = true;
                    }
                    modal.show(ui);
                },
                {
                    let mut modal = ResultModal::default();
                    modal.open(report);
                    modal
                },
            );
        harness.run();

        assert!(
            harness.query_by_label("RUN INCOMPLETE").is_some(),
            "a cancelled run is not headed 'complete'"
        );
        assert!(
            harness
                .query_by_label_contains("photos/a.jpg: permission denied")
                .is_some(),
            "the first failure is shown in full"
        );
        assert!(
            harness
                .query_by_label_contains("docs/b.pdf: no space left")
                .is_some(),
            "and so is the second — listed, not joined"
        );
    }
}
