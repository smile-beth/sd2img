/// 走査順の選択用。暗号用途ではない。
pub(crate) struct RandomOrder(pub(crate) u64);
impl RandomOrder {
    pub(crate) fn below(&mut self, upper: u64) -> u64 {
        loop {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            let limit = u64::MAX - u64::MAX % upper;
            if self.0 < limit {
                return self.0 % upper;
            }
        }
    }
}

use crate::{
    cli::Direction,
    rescue_map::{RescueMap, SectorStatus},
};
use std::collections::VecDeque;

const MAX_FRONTIERS: usize = 1024;
#[derive(Clone, Copy)]
struct Frontier {
    lba: u64,
    direction: Direction,
    status: SectorStatus,
}
impl Frontier {
    fn range(self, map: &RescueMap, max_len: u64) -> Option<(u64, u64)> {
        if max_len == 0 {
            return None;
        }
        if self.direction == Direction::Reverse {
            let extent = map.extents.iter().find(|extent| {
                extent.status == self.status
                    && self.lba >= extent.start
                    && self.lba - extent.start < extent.len
            })?;
            let len = (self.lba - extent.start + 1).min(max_len);
            Some((self.lba + 1 - len, len))
        } else {
            map.range_at(self.status, self.lba, max_len)
        }
    }
}

pub(crate) struct Scheduler {
    direction: Direction,
    rng: RandomOrder,
    contiguous: Option<Frontier>,
    frontiers: VecDeque<Frontier>,
    reading_direction: Direction,
    status: SectorStatus,
    continuing: bool,
}
impl Scheduler {
    pub(crate) fn new(direction: Direction, seed: u64) -> Self {
        Self {
            direction,
            rng: RandomOrder(seed.max(1)),
            contiguous: None,
            frontiers: VecDeque::new(),
            reading_direction: if direction == Direction::Reverse {
                Direction::Reverse
            } else {
                Direction::Forward
            },
            status: SectorStatus::Untried,
            continuing: false,
        }
    }
    pub(crate) fn reading_direction(&self) -> Direction {
        self.reading_direction
    }
    pub(crate) fn next(
        &mut self,
        map: &RescueMap,
        status: SectorStatus,
        max_len: u64,
    ) -> Option<(u64, u64)> {
        if self.direction != Direction::Random {
            let range = map.next_range(status, max_len, self.direction, &mut self.rng)?;
            self.status = status;
            self.reading_direction = self.direction;
            return Some(range);
        }
        if let Some((frontier, range)) = self
            .contiguous
            .filter(|frontier| frontier.status == status)
            .and_then(|frontier| frontier.range(map, max_len).map(|range| (frontier, range)))
        {
            self.reading_direction = frontier.direction;
            self.status = status;
            self.continuing = true;
            return Some(range);
        }
        // Only fresh searches prune/select candidates; ordinary sequential reads
        // do not scan the candidate queue or the whole map to choose a target.
        self.frontiers
            .retain(|frontier| frontier.range(map, 1).is_some());
        let eligible: Vec<_> = self
            .frontiers
            .iter()
            .enumerate()
            .filter(|(_, frontier)| frontier.status == status)
            .map(|(index, _)| index)
            .collect();
        let selected = if !eligible.is_empty() {
            let index = eligible[self.rng.below(eligible.len() as u64) as usize];
            let frontier = self.frontiers.remove(index).unwrap();
            frontier
                .range(map, max_len)
                .map(|range| (range, frontier.direction, "fast-neighbor"))
        } else {
            map.next_range(status, max_len, Direction::Random, &mut self.rng)
                .map(|range| (range, Direction::Forward, "random"))
        };
        let (range, direction, origin) = selected?;
        self.status = status;
        self.reading_direction = direction;
        self.continuing = false;
        crate::logging::append(&format!(
            "[scan target] origin={origin} LBA={} sectors={} direction={direction:?} candidates={}",
            range.0,
            range.1,
            self.frontiers.len()
        ));
        Some(range)
    }
    fn add(&mut self, lba: u64, direction: Direction) {
        if !self
            .frontiers
            .iter()
            .any(|old| old.lba == lba && old.direction == direction && old.status == self.status)
        {
            self.frontiers.push_back(Frontier {
                lba,
                direction,
                status: self.status,
            });
        }
        while self.frontiers.len() > MAX_FRONTIERS {
            self.frontiers.pop_front();
        }
    }
    pub(crate) fn completed(&mut self, start: u64, len: u64, success: bool) {
        if self.direction != Direction::Random {
            return;
        }
        let end = start.saturating_add(len);
        self.frontiers
            .retain(|frontier| frontier.lba < start || frontier.lba >= end);
        if !success {
            self.contiguous = None;
            self.continuing = false;
            return;
        }
        if !self.continuing {
            if let Some(before) = start.checked_sub(1) {
                self.add(before, Direction::Reverse);
            }
            self.add(end, Direction::Forward);
        }
        let next = if self.reading_direction == Direction::Reverse {
            start.checked_sub(1)
        } else {
            Some(end)
        };
        self.contiguous = next.map(|lba| Frontier {
            lba,
            direction: self.reading_direction,
            status: self.status,
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fast_run_expands_its_unexplored_opposite_side_after_a_timeout() {
        let mut map = RescueMap::new(0, vec![0; 512], 1000);
        let mut scheduler = Scheduler::new(Direction::Random, 42);
        map.update_range(0, 16, SectorStatus::Rescued);
        scheduler.completed(0, 16, true);
        assert_eq!(
            scheduler.next(&map, SectorStatus::Untried, 16),
            Some((16, 16))
        );
        map.update_range(16, 16, SectorStatus::Deferred);
        scheduler.completed(16, 16, false);
        assert!(scheduler.frontiers.is_empty());
        // A random jump starts a new fast run at 100.
        map.update_range(100, 16, SectorStatus::Rescued);
        scheduler.completed(100, 16, true);
        assert_eq!(
            scheduler.next(&map, SectorStatus::Untried, 16),
            Some((116, 16))
        );
        map.update_range(116, 16, SectorStatus::Rescued);
        scheduler.completed(116, 16, true);
        assert_eq!(
            scheduler.next(&map, SectorStatus::Untried, 16),
            Some((132, 16))
        );
        map.update_range(132, 16, SectorStatus::Deferred);
        scheduler.completed(132, 16, false);
        assert_eq!(
            scheduler.next(&map, SectorStatus::Untried, 16),
            Some((84, 16))
        );
        assert_eq!(scheduler.reading_direction(), Direction::Reverse);
        map.update_range(84, 16, SectorStatus::Rescued);
        scheduler.completed(84, 16, true);
        assert_eq!(
            scheduler.next(&map, SectorStatus::Untried, 16),
            Some((68, 16))
        );
        // A slow block on this side ends the run too; neither slow edge is a candidate.
        map.update_range(68, 16, SectorStatus::Deferred);
        scheduler.completed(68, 16, false);
        let next = scheduler.next(&map, SectorStatus::Untried, 16).unwrap();
        assert!(map
            .range_at(SectorStatus::Untried, next.0, next.1)
            .is_some());
        assert!(scheduler.frontiers.is_empty());
    }
    #[test]
    fn candidates_are_bounded_and_do_not_affect_explicit_forward_or_reverse_scans() {
        let mut scheduler = Scheduler::new(Direction::Random, 42);
        for index in 0..2000 {
            scheduler.completed(index * 100 + 50, 16, true);
        }
        assert_eq!(scheduler.frontiers.len(), MAX_FRONTIERS);
        for direction in [Direction::Forward, Direction::Reverse] {
            let map = RescueMap::new(0, vec![0; 512], 1000);
            let mut scheduler = Scheduler::new(direction, 42);
            scheduler.completed(100, 16, true);
            let expected = if direction == Direction::Forward {
                (0, 16)
            } else {
                (984, 16)
            };
            assert_eq!(
                scheduler.next(&map, SectorStatus::Untried, 16),
                Some(expected)
            );
            assert_eq!(scheduler.reading_direction(), direction);
            assert!(scheduler.frontiers.is_empty());
        }
    }
    #[test]
    fn delayed_success_does_not_restore_the_abandoned_continuation() {
        let mut map = RescueMap::new(0, vec![0; 512], 10_000);
        let mut scheduler = Scheduler::new(Direction::Random, 42);
        let mut expected_rng = RandomOrder(42);
        let first = scheduler.next(&map, SectorStatus::Untried, 128).unwrap();
        assert_eq!(
            Some(first),
            map.next_range(
                SectorStatus::Untried,
                128,
                Direction::Random,
                &mut expected_rng
            )
        );
        map.update_range(first.0, first.1, SectorStatus::Deferred);
        scheduler.completed(first.0, first.1, false);
        // Late data is saved, without telling the scheduler to resume that stream.
        map.update_range(first.0, first.1, SectorStatus::Rescued);
        let next = scheduler.next(&map, SectorStatus::Untried, 128);
        assert_eq!(
            next,
            map.next_range(
                SectorStatus::Untried,
                128,
                Direction::Random,
                &mut expected_rng
            )
        );
        assert_ne!(next.unwrap().0, first.0 + first.1);
    }
    #[test]
    fn random_continues_after_success_and_selects_the_opposite_side_after_failure() {
        let mut map = RescueMap::new(0, vec![0; 512], 10_000);
        let mut scheduler = Scheduler::new(Direction::Random, 42);
        let mut expected_rng = RandomOrder(42);
        let first = scheduler.next(&map, SectorStatus::Untried, 128).unwrap();
        assert_eq!(
            Some(first),
            map.next_range(
                SectorStatus::Untried,
                128,
                Direction::Random,
                &mut expected_rng
            )
        );
        map.update_range(first.0, first.1, SectorStatus::Rescued);
        scheduler.completed(first.0, first.1, true);
        let second = scheduler.next(&map, SectorStatus::Untried, 128).unwrap();
        assert_eq!(second.0, first.0 + first.1);
        map.update_range(second.0, second.1, SectorStatus::Deferred);
        scheduler.completed(second.0, second.1, false);
        let next = scheduler.next(&map, SectorStatus::Untried, 128).unwrap();
        assert_eq!(next.0 + next.1, first.0);
        assert_eq!(scheduler.reading_direction(), Direction::Reverse);
    }

    #[test]
    fn fast_neighbors_and_adaptive_scans_cover_every_sector_without_duplicates() {
        let total = 1003;
        let mut map = RescueMap::new(0, vec![0; 512], total);
        let mut scheduler = Scheduler::new(Direction::Random, 19);
        let mut visited = vec![false; total as usize];
        let mut reads = 0;
        while let Some((start, len)) = scheduler.next(&map, SectorStatus::Untried, 128) {
            for lba in start..start + len {
                assert!(!visited[lba as usize]);
                visited[lba as usize] = true;
            }
            let success = reads % 3 != 0;
            map.update_range(
                start,
                len,
                if success {
                    SectorStatus::Rescued
                } else {
                    SectorStatus::Deferred
                },
            );
            scheduler.completed(start, len, success);
            reads += 1;
        }
        assert!(visited.into_iter().all(|seen| seen));
        while let Some((start, len)) = scheduler.next(&map, SectorStatus::Deferred, 1) {
            map.update_range(start, len, SectorStatus::Rescued);
            scheduler.completed(start, len, true);
        }
        assert_eq!(map.stats(), (total, 0, 0));
    }
}
