//! Independent tuning for logical sequential streams, not OS worker threads.
use crate::{
    read_size::{ReadSizing, ReadTimeout},
    sector::SectorSize,
};
#[derive(Clone)]
pub(crate) struct Lane {
    pub(crate) id: u64,
    pub(crate) sizing: ReadSizing,
    pub(crate) timeout: ReadTimeout,
    alternate_sizing: ReadSizing,
    alternate_timeout: ReadTimeout,
    loaded: bool,
}

impl Lane {
    fn new(id: u64, sector: SectorSize, maximum: usize, timeout: u32) -> Self {
        Self {
            id,
            sizing: ReadSizing::new(sector, maximum),
            timeout: ReadTimeout::new(timeout),
            alternate_sizing: ReadSizing::new(sector, maximum),
            alternate_timeout: ReadTimeout::new(timeout),
            loaded: false,
        }
    }
    pub(crate) fn load(&mut self, loaded: bool) {
        if self.loaded != loaded {
            std::mem::swap(&mut self.sizing, &mut self.alternate_sizing);
            std::mem::swap(&mut self.timeout, &mut self.alternate_timeout);
            self.loaded = loaded;
        }
    }
}
pub(crate) struct ReadLanes {
    pub(crate) current: Lane,
    next_id: u64,
    sector: SectorSize,
    maximum: usize,
    minimum_ms: u32,
}
impl ReadLanes {
    pub(crate) fn new(sector: SectorSize, maximum: usize, minimum_ms: u32) -> Self {
        Self {
            current: Lane::new(1, sector, maximum, minimum_ms),
            next_id: 2,
            sector,
            maximum,
            minimum_ms,
        }
    }
    pub(crate) fn succeeded(&mut self, start: u64, len: u64) {
        self.current.alternate_sizing.advance(start, len);
    }
    pub(crate) fn failed(&mut self) {
        self.current = Lane::new(self.next_id, self.sector, self.maximum, self.minimum_ms);
        self.next_id += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Direction;
    use std::time::Duration;
    const BLOCK: u64 = 128;
    #[test]
    fn normal_and_loaded_profiles_remain_separate_and_survive_switches() {
        let mut lanes = ReadLanes::new(SectorSize::LEGACY, 1024 * 1024, 2000);
        for index in 0..7 {
            lanes
                .current
                .sizing
                .success(index * BLOCK, BLOCK, Duration::from_millis(1));
            lanes.succeeded(index * BLOCK, BLOCK);
        }
        assert_eq!(lanes.current.sizing.bytes(), 131072);
        lanes.current.load(true);
        assert_eq!(lanes.current.sizing.bytes(), 65536);
        for index in 7..14 {
            let start = index * BLOCK;
            assert_eq!(
                lanes
                    .current
                    .sizing
                    .prepare(start, BLOCK, Direction::Forward),
                (start, BLOCK)
            );
            lanes
                .current
                .sizing
                .success(start, BLOCK, Duration::from_millis(10));
            lanes.succeeded(start, BLOCK);
        }
        lanes.current.load(false);
        assert_eq!(
            lanes
                .current
                .sizing
                .prepare(14 * BLOCK, 256, Direction::Forward),
            (14 * BLOCK, 256)
        );
    }
}
