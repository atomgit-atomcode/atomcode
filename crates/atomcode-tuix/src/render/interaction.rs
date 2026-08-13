use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    MenuItem { index: usize },
    ModalItem { index: usize },
    ModalCancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitRegion {
    pub rect: CellRect,
    pub target: HitTarget,
}

#[derive(Debug, Default)]
pub struct InteractionFrame {
    pub generation: u64,
    pub regions: Vec<HitRegion>,
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
    inner: Arc<RwLock<Arc<InteractionFrame>>>,
    actionable: Arc<AtomicBool>,
}

impl InteractionPublisher {
    pub fn snapshot(&self) -> Arc<InteractionFrame> {
        read_recover(&self.inner).clone()
    }

    pub fn snapshot_actionable(&self) -> Option<Arc<InteractionFrame>> {
        // Retained frame flushing and input dispatch are serialized by the
        // current TUI event loop. The second load only closes the small clone
        // window if a writer failed concurrently; this is not intended as a
        // cross-thread transaction boundary for event execution.
        if !self.actionable.load(Ordering::Acquire) {
            return None;
        }
        let frame = self.snapshot();
        self.actionable.load(Ordering::Acquire).then_some(frame)
    }

    pub fn publish(&self, regions: Vec<HitRegion>) {
        let mut slot = write_recover(&self.inner);
        let generation = slot.generation.saturating_add(1);
        *slot = Arc::new(InteractionFrame {
            generation,
            regions,
        });
        self.actionable.store(true, Ordering::Release);
    }

    pub fn fail_closed(&self) {
        self.actionable.store(false, Ordering::Release);
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

        publisher.publish(vec![HitRegion {
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
    fn publisher_recovers_from_a_poisoned_lock() {
        let publisher = InteractionPublisher::default();
        let poison = publisher.clone();
        let _ = std::thread::spawn(move || poison.poison_for_test()).join();

        publisher.publish(vec![HitRegion {
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
        };

        assert_eq!(frame.hit(5, 4), Some(HitTarget::ModalItem { index: 0 }));
        assert_eq!(frame.hit(4, 4), Some(HitTarget::MenuItem { index: 2 }));
    }
}
