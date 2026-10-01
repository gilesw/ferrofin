//! Live item counts and lifecycle for library refresh indicators.
//!
//! Each scan owns a guard. Dropping it removes only that scan, including on
//! cancellation or unwinding. An enclosing scan keeps ownership of a library's
//! indicator while nested refreshes run; their counts are never added twice.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use uuid::Uuid;

/// The work currently being performed by a scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanPhase {
    /// Waiting for the scan worker.
    Queued,
    /// Discovering items; the denominator is not known yet.
    Planning,
    /// Processing the planned items.
    Items,
    /// All items processed; pruning and closing passes are still running.
    Finalizing,
}

/// A consistent view of the work for one library in one scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LibraryScanProgress {
    /// Owning scan generation.
    pub scan_id: u64,
    /// The library's collection-folder id.
    pub library_id: Uuid,
    /// Number of items already handled, including unchanged items.
    pub completed: usize,
    /// Number of planned items; unknown while planning.
    pub total: Option<usize>,
    /// Current work phase.
    pub phase: ScanPhase,
}

impl LibraryScanProgress {
    /// Item completion percentage. An empty or unknown plan starts at zero.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn percent(self) -> f64 {
        match self.total {
            Some(total) if total > 0 => 100.0 * self.completed as f64 / total as f64,
            _ if self.phase == ScanPhase::Finalizing => 100.0,
            _ => 0.0,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    next_id: u64,
    runs: BTreeMap<u64, BTreeMap<Uuid, LibraryScanProgress>>,
}

/// Shared refresh state, injected into the scanner and virtual-folder reader.
#[derive(Clone, Debug, Default)]
pub struct ScanProgressTracker {
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ScanProgressTracker {
    /// Registers a scan before its filesystem walk starts.
    #[must_use]
    pub fn begin(&self, libraries: impl IntoIterator<Item = Uuid>) -> ScanProgressRun {
        let run = self.queued(libraries);
        run.activate();
        run
    }

    /// Registers work waiting for the scan worker.
    #[must_use]
    pub fn queued(&self, libraries: impl IntoIterator<Item = Uuid>) -> ScanProgressRun {
        let mut state = lock(&self.state);
        state.next_id += 1;
        let id = state.next_id;
        let libraries = libraries
            .into_iter()
            .map(|library_id| {
                (
                    library_id,
                    LibraryScanProgress {
                        scan_id: id,
                        library_id,
                        completed: 0,
                        total: None,
                        phase: ScanPhase::Queued,
                    },
                )
            })
            .collect();
        state.runs.insert(id, libraries);
        ScanProgressRun {
            tracker: self.clone(),
            id,
        }
    }

    /// Reads the oldest active scan for a library, so nested work cannot reset it.
    #[must_use]
    pub fn library(&self, library_id: Uuid) -> Option<LibraryScanProgress> {
        lock(&self.state)
            .runs
            .values()
            .find_map(|run| run.get(&library_id).copied())
    }

    /// Reads the visible progress for every active library in one snapshot.
    #[must_use]
    pub fn libraries(&self) -> Vec<LibraryScanProgress> {
        let state = lock(&self.state);
        let mut libraries = BTreeMap::new();
        for run in state.runs.values() {
            for (&id, progress) in run {
                libraries.entry(id).or_insert(*progress);
            }
        }
        libraries.into_values().collect()
    }
}

/// Owns one scan's progress. Removal is guaranteed on every exit path.
#[derive(Debug)]
pub struct ScanProgressRun {
    tracker: ScanProgressTracker,
    id: u64,
}

impl ScanProgressRun {
    /// Marks queued work active before planning begins.
    pub fn activate(&self) {
        if let Some(run) = lock(&self.tracker.state).runs.get_mut(&self.id) {
            for progress in run.values_mut() {
                progress.phase = ScanPhase::Planning;
            }
        }
    }

    /// Sets the denominator once planning finishes; libraries with no items remain.
    pub fn planned(&self, totals: impl IntoIterator<Item = (Uuid, usize)>) {
        let mut state = lock(&self.tracker.state);
        if let Some(run) = state.runs.get_mut(&self.id) {
            for progress in run.values_mut() {
                progress.total = Some(0);
                progress.phase = ScanPhase::Items;
            }
            for (id, total) in totals {
                if let Some(progress) = run.get_mut(&id) {
                    progress.total = Some(total);
                }
            }
        }
    }

    /// Records an item only after the scanner has handled it.
    pub fn advance(&self, library: Uuid) {
        let mut state = lock(&self.tracker.state);
        if let Some(progress) = state
            .runs
            .get_mut(&self.id)
            .and_then(|run| run.get_mut(&library))
        {
            progress.completed = progress
                .completed
                .saturating_add(1)
                .min(progress.total.unwrap_or(0));
        }
    }

    /// Keeps the library active while the closing passes run.
    pub fn finalizing(&self) {
        if let Some(run) = lock(&self.tracker.state).runs.get_mut(&self.id) {
            for progress in run.values_mut() {
                progress.phase = ScanPhase::Finalizing;
            }
        }
    }
}

impl Drop for ScanProgressRun {
    fn drop(&mut self) {
        lock(&self.tracker.state).runs.remove(&self.id);
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_cleanup_follow_the_owning_scan() {
        let tracker = ScanProgressTracker::default();
        let library = Uuid::new_v4();
        let run = tracker.begin([library]);
        assert_eq!(tracker.library(library).unwrap().percent(), 0.0);
        run.planned([(library, 3)]);
        run.advance(library);
        assert!((tracker.library(library).unwrap().percent() - 100.0 / 3.0).abs() < f64::EPSILON);
        let nested = tracker.begin([library]);
        nested.planned([(library, 1)]);
        nested.advance(library);
        assert_eq!(tracker.library(library).unwrap().completed, 1);
        drop(nested);
        assert_eq!(tracker.libraries().len(), 1);
        run.advance(library);
        run.advance(library);
        run.finalizing();
        assert_eq!(
            tracker.library(library).unwrap().phase,
            ScanPhase::Finalizing
        );
        drop(run);
        assert!(tracker.libraries().is_empty());
        let next = tracker.begin([library]);
        assert_eq!(tracker.library(library).unwrap().completed, 0);
        drop(next);
    }

    #[test]
    fn empty_libraries_and_overlapping_scans_have_independent_lifetimes() {
        let tracker = ScanProgressTracker::default();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let first = tracker.begin([a]);
        first.planned([]);
        first.finalizing();
        assert_eq!(tracker.library(a).unwrap().percent(), 100.0);
        let second = tracker.begin([a, b]);
        drop(first);
        assert_eq!(tracker.library(a).unwrap().phase, ScanPhase::Planning);
        assert_eq!(tracker.libraries().len(), 2);
        drop(second);
        assert!(tracker.libraries().is_empty());
    }
}
