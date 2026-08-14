use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerSelection {
    pub source: Arc<str>,
    pub range: std::ops::Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyRun {
    pub id: u64,
    pub rect: CellRect,
    pub text: Arc<str>,
    pub soft_wrap: bool,
    pub next_run_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticEndpoint {
    pub run_id: u64,
    pub byte: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSelection {
    pub anchor: SemanticEndpoint,
    pub head: SemanticEndpoint,
    pub run_ids: Arc<[u64]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellRect {
    pub row: u16,
    pub col: u16,
    pub height: u16,
    pub width: u16,
}

impl CellRect {
    fn contains(self, row: u16, col: u16) -> bool {
        row >= self.row
            && row < self.row.saturating_add(self.height)
            && col >= self.col
            && col < self.col.saturating_add(self.width)
    }

    pub(crate) fn intersects(self, other: Self) -> bool {
        self.row < other.row.saturating_add(other.height)
            && other.row < self.row.saturating_add(self.height)
            && self.col < other.col.saturating_add(other.width)
            && other.col < self.col.saturating_add(self.width)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    MenuItem { index: usize },
    ModalItem { index: usize },
    ModalCancel,
    ComposerByte { byte: usize },
    TranscriptByte { run_id: u64, byte: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitRegion {
    pub rect: CellRect,
    pub target: HitTarget,
}

#[derive(Debug, Default)]
pub struct InteractionFrame {
    pub generation: u64,
    pub surface_session: u64,
    pub regions: Vec<HitRegion>,
    pub copy_runs: Vec<CopyRun>,
}

impl InteractionFrame {
    pub fn hit(&self, row: u16, col: u16) -> Option<HitTarget> {
        self.regions
            .iter()
            .rev()
            .find(|region| region.rect.contains(row, col))
            .map(|region| region.target)
    }
}

#[derive(Debug, Clone, Default)]
pub struct InteractionPublisher {
    inner: Arc<RwLock<PublisherState>>,
}

#[derive(Debug, Default)]
struct PublisherState {
    frame: Arc<InteractionFrame>,
    epoch: u64,
    actionable: bool,
    worker_authority: Option<(u64, u64)>,
    composer_selection: Option<ComposerSelection>,
    transcript_selection: Option<TranscriptSelection>,
}

impl InteractionPublisher {
    pub fn snapshot(&self) -> Arc<InteractionFrame> {
        read_recover(&self.inner).frame.clone()
    }

    pub fn snapshot_actionable(&self) -> Option<Arc<InteractionFrame>> {
        let state = read_recover(&self.inner);
        state.actionable.then(|| state.frame.clone())
    }

    pub fn publish(&self, surface_session: u64, regions: Vec<HitRegion>) {
        let epoch = read_recover(&self.inner).epoch;
        let _ = self.publish_if_current(epoch, surface_session, regions);
    }

    pub fn publish_if_current(
        &self,
        expected_epoch: u64,
        surface_session: u64,
        regions: Vec<HitRegion>,
    ) -> bool {
        self.publish_frame_if_current(expected_epoch, surface_session, regions, Vec::new())
    }

    pub fn publish_frame_if_current(
        &self,
        expected_epoch: u64,
        surface_session: u64,
        regions: Vec<HitRegion>,
        copy_runs: Vec<CopyRun>,
    ) -> bool {
        self.publish_frame_with_selection_if_current(
            expected_epoch,
            surface_session,
            regions,
            copy_runs,
        )
    }

    pub fn publish_frame_with_selection_if_current(
        &self,
        expected_epoch: u64,
        surface_session: u64,
        regions: Vec<HitRegion>,
        copy_runs: Vec<CopyRun>,
    ) -> bool {
        let mut state = write_recover(&self.inner);
        if state.epoch != expected_epoch
            || state
                .worker_authority
                .is_some_and(|authority| authority != (expected_epoch, surface_session))
        {
            return false;
        }
        state.transcript_selection = state
            .transcript_selection
            .as_ref()
            .and_then(|selection| reconcile_selection(selection, &copy_runs));
        let generation = state.frame.generation.saturating_add(1);
        state.frame = Arc::new(InteractionFrame {
            generation,
            surface_session,
            regions,
            copy_runs,
        });
        state.actionable = true;
        true
    }

    pub fn invalidate(&self) -> u64 {
        let mut state = write_recover(&self.inner);
        state.epoch = state.epoch.saturating_add(1);
        state.actionable = false;
        state.epoch
    }

    pub fn fail_closed(&self) {
        let _ = self.invalidate();
    }

    pub fn set_worker_authority(&self, epoch: u64, surface_session: u64) -> bool {
        let mut state = write_recover(&self.inner);
        if state.epoch != epoch {
            return false;
        }
        state.worker_authority = Some((epoch, surface_session));
        true
    }

    pub fn worker_authority(&self) -> Option<(u64, u64)> {
        read_recover(&self.inner).worker_authority
    }

    pub fn current_epoch(&self) -> u64 {
        read_recover(&self.inner).epoch
    }

    pub fn set_composer_selection(
        &self,
        source: &str,
        selection: Option<std::ops::Range<usize>>,
    ) {
        let valid = selection.filter(|range| {
            range.start < range.end
                && range.end <= source.len()
                && source.is_char_boundary(range.start)
                && source.is_char_boundary(range.end)
        });
        write_recover(&self.inner).composer_selection = valid.map(|range| ComposerSelection {
            source: Arc::from(source),
            range,
        });
    }

    pub fn composer_selection(&self) -> Option<ComposerSelection> {
        read_recover(&self.inner).composer_selection.clone()
    }

    pub fn set_transcript_selection(&self, selection: Option<TranscriptSelection>) {
        write_recover(&self.inner).transcript_selection = selection;
    }

    pub fn transcript_selection(&self) -> Option<TranscriptSelection> {
        read_recover(&self.inner).transcript_selection.clone()
    }

    #[cfg(test)]
    fn reconcile_transcript_selection(&self, runs: &[CopyRun]) -> Option<TranscriptSelection> {
        let mut state = write_recover(&self.inner);
        let selection = state
            .transcript_selection
            .as_ref()
            .and_then(|selection| reconcile_selection(selection, runs));
        state.transcript_selection = selection.clone();
        selection
    }

    pub fn projected_transcript_selection(&self, runs: &[CopyRun]) -> Option<TranscriptSelection> {
        read_recover(&self.inner)
            .transcript_selection
            .as_ref()
            .and_then(|selection| reconcile_selection(selection, runs))
    }

    #[cfg(test)]
    fn poison_for_test(&self) {
        let _guard = self
            .inner
            .write()
            .unwrap_or_else(|poison| poison.into_inner());
        panic!("poison interaction publisher for recovery test");
    }
}

fn reconcile_selection(
    selection: &TranscriptSelection,
    runs: &[CopyRun],
) -> Option<TranscriptSelection> {
    let mut selection = selection.clone();
    selection.run_ids = selection
        .run_ids
        .iter()
        .copied()
        .filter(|id| runs.iter().any(|run| run.id == *id))
        .collect::<Vec<_>>()
        .into();
    let clamp = |endpoint: SemanticEndpoint| {
        let run = runs
            .iter()
            .filter(|run| selection.run_ids.contains(&run.id))
            .min_by_key(|run| run.id.abs_diff(endpoint.run_id))?;
        let mut byte = endpoint.byte.min(run.text.len());
        while byte > 0 && !run.text.is_char_boundary(byte) {
            byte -= 1;
        }
        Some(SemanticEndpoint {
            run_id: run.id,
            byte,
        })
    };
    selection.anchor = clamp(selection.anchor)?;
    selection.head = clamp(selection.head)?;
    Some(selection)
}

fn read_recover<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poison| poison.into_inner())
}

fn write_recover<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_starts_unpainted_and_advances_only_when_published() {
        let publisher = InteractionPublisher::default();
        let initial = publisher.snapshot();
        assert_eq!(initial.generation, 0);
        assert!(initial.regions.is_empty());
        assert!(publisher.snapshot_actionable().is_none());

        publisher.publish(1, vec![HitRegion {
            rect: CellRect {
                row: 3,
                col: 2,
                height: 1,
                width: 20,
            },
            target: HitTarget::MenuItem { index: 4 },
        }]);
        let painted = publisher.snapshot();
        assert_eq!(painted.generation, 1);
        assert_eq!(painted.hit(3, 5), Some(HitTarget::MenuItem { index: 4 }));
        assert!(publisher.snapshot_actionable().is_some());

        publisher.fail_closed();
        assert!(publisher.snapshot_actionable().is_none());
        assert_eq!(publisher.snapshot().generation, 1);
    }

    #[test]
    fn composer_selection_presentation_requires_matching_utf8_boundaries() {
        let publisher = InteractionPublisher::default();
        publisher.set_composer_selection("你ab", Some("你".len().."你a".len()));
        let selection = publisher
            .composer_selection()
            .expect("non-empty valid selection");
        assert_eq!(&*selection.source, "你ab");
        assert_eq!(selection.range, "你".len().."你a".len());

        publisher.set_composer_selection("你ab", Some(1..2));
        assert!(publisher.composer_selection().is_none());
        publisher.set_composer_selection("你ab", None);
        assert!(publisher.composer_selection().is_none());
    }

    #[test]
    fn transcript_presentation_clamps_on_a_stream_frame_without_pointer_input() {
        let publisher = InteractionPublisher::default();
        publisher.set_transcript_selection(Some(TranscriptSelection {
            anchor: SemanticEndpoint { run_id: 1, byte: 4 },
            head: SemanticEndpoint { run_id: 2, byte: 3 },
            run_ids: vec![1, 2].into(),
        }));
        let runs = vec![CopyRun {
            id: 2,
            rect: CellRect {
                row: 0,
                col: 0,
                height: 1,
                width: 4,
            },
            text: Arc::from("你a"),
            soft_wrap: false,
            next_run_id: None,
        }];
        let selection = publisher
            .reconcile_transcript_selection(&runs)
            .expect("nearest initial run survives");
        assert_eq!(selection.run_ids.as_ref(), &[2]);
        assert_eq!(selection.anchor, SemanticEndpoint { run_id: 2, byte: 4 });
        assert_eq!(selection.head, SemanticEndpoint { run_id: 2, byte: 3 });

        assert!(publisher.reconcile_transcript_selection(&[]).is_none());
        assert!(publisher.transcript_selection().is_none());
    }

    #[test]
    fn stale_blocked_worker_cannot_reconcile_selection_before_epoch_publish_rejects_it() {
        fn run(id: u64) -> CopyRun {
            CopyRun {
                id,
                rect: CellRect {
                    row: id as u16,
                    col: 0,
                    height: 1,
                    width: 1,
                },
                text: Arc::from("x"),
                soft_wrap: false,
                next_run_id: None,
            }
        }

        let publisher = InteractionPublisher::default();
        publisher.set_transcript_selection(Some(TranscriptSelection {
            anchor: SemanticEndpoint { run_id: 1, byte: 0 },
            head: SemanticEndpoint { run_id: 2, byte: 1 },
            run_ids: vec![1, 2].into(),
        }));
        let stale_epoch = publisher.current_epoch();
        assert!(publisher.set_worker_authority(stale_epoch, 9));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let stale_publisher = publisher.clone();
        let stale_worker = std::thread::spawn(move || {
            release_rx.recv().expect("release stale worker");
            stale_publisher.publish_frame_with_selection_if_current(
                stale_epoch,
                9,
                Vec::new(),
                vec![run(2)],
            )
        });

        let current_epoch = publisher.invalidate();
        assert!(publisher.set_worker_authority(current_epoch, 9));
        assert!(publisher.publish_frame_with_selection_if_current(
            current_epoch,
            9,
            Vec::new(),
            vec![run(1), run(2)],
        ));
        release_tx.send(()).expect("unblock stale worker");
        assert!(!stale_worker.join().expect("stale worker joined"));
        assert_eq!(
            publisher
                .transcript_selection()
                .expect("current selection survives")
                .run_ids
                .as_ref(),
            &[1, 2],
            "stale reconciliation must not mutate presentation before rejected publish"
        );
    }

    #[test]
    fn publisher_recovers_from_a_poisoned_lock() {
        let publisher = InteractionPublisher::default();
        let poison = publisher.clone();
        let _ = std::thread::spawn(move || poison.poison_for_test()).join();

        publisher.publish(1, vec![HitRegion {
            rect: CellRect {
                row: 1,
                col: 1,
                height: 2,
                width: 2,
            },
            target: HitTarget::ModalCancel,
        }]);

        assert_eq!(publisher.snapshot().generation, 1);
    }

    #[test]
    fn later_overlay_region_wins_hit_precedence() {
        let frame = InteractionFrame {
            generation: 1,
            surface_session: 1,
            regions: vec![
                HitRegion {
                    rect: CellRect {
                        row: 4,
                        col: 0,
                        height: 3,
                        width: 20,
                    },
                    target: HitTarget::MenuItem { index: 2 },
                },
                HitRegion {
                    rect: CellRect {
                        row: 5,
                        col: 2,
                        height: 1,
                        width: 8,
                    },
                    target: HitTarget::ModalItem { index: 0 },
                },
            ],
            copy_runs: Vec::new(),
        };

        assert_eq!(frame.hit(5, 4), Some(HitTarget::ModalItem { index: 0 }));
        assert_eq!(frame.hit(4, 4), Some(HitTarget::MenuItem { index: 2 }));
    }

    #[test]
    fn stale_worker_publish_cannot_reopen_after_a_newer_invalidation() {
        let publisher = InteractionPublisher::default();
        let initial_epoch = publisher.invalidate();
        assert!(publisher.publish_if_current(initial_epoch, 1, Vec::new()));
        assert_eq!(publisher.snapshot().generation, 1);

        let stale_epoch = publisher.invalidate();
        let current_epoch = publisher.invalidate();
        assert!(!publisher.publish_if_current(stale_epoch, 1, Vec::new()));
        assert!(publisher.snapshot_actionable().is_none());
        assert_eq!(publisher.snapshot().generation, 1);

        assert!(publisher.publish_if_current(current_epoch, 2, Vec::new()));
        let frame = publisher.snapshot_actionable().unwrap();
        assert_eq!(frame.generation, 2);
        assert_eq!(frame.surface_session, 2);
    }

    #[test]
    fn stale_worker_authority_cannot_replace_the_current_epoch() {
        let publisher = InteractionPublisher::default();
        let stale_epoch = publisher.invalidate();
        assert!(publisher.set_worker_authority(stale_epoch, 1));
        let current_epoch = publisher.invalidate();
        assert!(publisher.set_worker_authority(current_epoch, 2));

        assert!(!publisher.set_worker_authority(stale_epoch, 1));

        assert_eq!(publisher.worker_authority(), Some((current_epoch, 2)));
    }
}
