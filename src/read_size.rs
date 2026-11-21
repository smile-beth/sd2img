//! Passive transfer-efficiency tuning; no benchmark reads are issued.
use crate::{cli::Direction, sector::SectorSize};
use std::time::Duration;

pub(crate) const INITIAL_BYTES: usize = 64 * 1024;
#[cfg(test)]
pub(crate) const DEFAULT_MAX_BYTES: usize = 1024 * 1024 * 1024;
const SAMPLE_COUNT: usize = 7;
const MIN_IMPROVEMENT: f64 = 0.05;

/// Predict transfer time from the latest successful scan read, with the
/// configured timeout as a floor. Failed reads never train this estimate.
#[derive(Clone)]
pub(crate) struct ReadTimeout {
    minimum_ms: u32,
    sample: Option<(usize, Duration)>,
}

impl ReadTimeout {
    pub(crate) fn new(minimum_ms: u32) -> Self {
        Self {
            minimum_ms,
            sample: None,
        }
    }
    pub(crate) fn success(&mut self, bytes: usize, elapsed: Duration) {
        if bytes > 0 && !elapsed.is_zero() {
            self.sample = Some((bytes, elapsed));
        }
    }
    pub(crate) fn for_bytes(&self, bytes: usize) -> u32 {
        let predicted = self.sample.map_or(0, |(sample_bytes, elapsed)| {
            let nanos = elapsed.as_nanos().saturating_mul(bytes as u128);
            let divisor = sample_bytes as u128 * 1_000_000;
            nanos.div_ceil(divisor).min((u32::MAX - 1) as u128) as u32
        });
        predicted.max(self.minimum_ms).max(1)
    }
}

#[derive(Clone)]
pub(crate) struct ReadSizing {
    sector: SectorSize,
    maximum: usize,
    target: usize,
    previous_range: Option<(u64, u64)>,
    previous_cost: Option<(usize, f64)>,
    samples: Vec<f64>,
    settled: bool,
}

impl ReadSizing {
    pub(crate) fn new(sector: SectorSize, maximum: usize) -> Self {
        Self {
            sector,
            maximum: maximum / sector.bytes() * sector.bytes(),
            target: INITIAL_BYTES,
            previous_range: None,
            previous_cost: None,
            samples: Vec::new(),
            settled: false,
        }
    }
    pub(crate) fn sectors(&self) -> u64 {
        (self.target / self.sector.bytes()) as u64
    }
    pub(crate) fn reset(&mut self) {
        self.target = INITIAL_BYTES;
        self.previous_range = None;
        self.previous_cost = None;
        self.samples.clear();
        self.settled = false;
    }
    pub(crate) fn advance(&mut self, start: u64, len: u64) {
        self.previous_range = Some((start, start + len));
    }
    pub(crate) fn prepare(&mut self, start: u64, len: u64, direction: Direction) -> (u64, u64) {
        if let Some((previous_start, previous_end)) = self.previous_range {
            let contiguous = if direction == Direction::Reverse {
                start + len == previous_start
            } else {
                start == previous_end
            };
            if !contiguous {
                self.reset();
            }
        }
        let adjusted = len.min(self.sectors());
        (
            if direction == Direction::Reverse {
                start + len - adjusted
            } else {
                start
            },
            adjusted,
        )
    }
    /// Returns the previous target when the tuning decision changes or settles.
    pub(crate) fn success(&mut self, start: u64, len: u64, elapsed: Duration) -> Option<usize> {
        self.previous_range = Some((start, start + len));
        let bytes = len as usize * self.sector.bytes();
        if self.settled || bytes != self.target || elapsed.is_zero() {
            return None;
        }
        self.samples.push(elapsed.as_secs_f64() / bytes as f64);
        if self.samples.len() < SAMPLE_COUNT {
            return None;
        }
        self.samples.sort_by(f64::total_cmp);
        let cost = self.samples[SAMPLE_COUNT / 2];
        self.samples.clear();
        let previous = self.target;
        if let Some((smaller, smaller_cost)) = self.previous_cost {
            if cost >= smaller_cost * (1.0 - MIN_IMPROVEMENT) {
                // Prefer the smaller request when efficiency is effectively equal.
                self.target = smaller;
                self.settled = true;
                return Some(previous);
            }
        }
        self.previous_cost = Some((self.target, cost));
        let next = self.target.saturating_mul(2).min(self.maximum);
        if next <= self.target {
            self.settled = true;
        } else {
            self.target = next;
        }
        Some(previous)
    }
    pub(crate) fn bytes(&self) -> usize {
        self.target
    }
    pub(crate) fn settled(&self) -> bool {
        self.settled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timeout_scales_with_successful_transfer_size_and_speed() {
        let mut timeout = ReadTimeout::new(2000);
        assert_eq!(timeout.for_bytes(256 * 1024), 2000);
        timeout.success(64 * 1024, Duration::from_secs(1));
        assert_eq!(timeout.for_bytes(128 * 1024), 2000);
        assert_eq!(timeout.for_bytes(256 * 1024), 4000);
        timeout.success(256 * 1024, Duration::from_secs(2));
        assert_eq!(timeout.for_bytes(512 * 1024), 4000);
        assert_eq!(timeout.for_bytes(512), 2000);
        timeout.success(0, Duration::from_secs(10));
        timeout.success(512, Duration::ZERO);
        assert_eq!(timeout.for_bytes(512 * 1024), 4000);
        timeout.success(1, Duration::MAX);
        assert_eq!(timeout.for_bytes(usize::MAX), u32::MAX - 1);
        timeout.success(3, Duration::from_millis(2000));
        assert_eq!(timeout.for_bytes(4), 2667);
    }
    fn sample(sizing: &mut ReadSizing, seconds_per_byte: f64, start: &mut u64) {
        for _ in 0..SAMPLE_COUNT {
            let len = sizing.sectors();
            sizing.success(
                *start,
                len,
                Duration::from_secs_f64(
                    seconds_per_byte * (len as usize * sizing.sector.bytes()) as f64,
                ),
            );
            *start += len;
        }
    }
    #[test]
    fn grows_only_for_measured_benefit_and_uses_smaller_size_at_plateau() {
        let mut sizing = ReadSizing::new(SectorSize::LEGACY, DEFAULT_MAX_BYTES);
        let mut start = 0;
        sample(&mut sizing, 1e-8, &mut start);
        assert_eq!(sizing.bytes(), 128 * 1024);
        sample(&mut sizing, 5e-9, &mut start);
        assert_eq!(sizing.bytes(), 256 * 1024);
        sample(&mut sizing, 4.9e-9, &mut start);
        assert_eq!(sizing.bytes(), 128 * 1024);
        assert!(sizing.settled());
        sizing.reset();
        assert_eq!(sizing.bytes(), INITIAL_BYTES);
        assert!(!sizing.settled());
    }
    #[test]
    fn respects_cap_sector_size_and_noncontiguous_regions() {
        let mut sizing = ReadSizing::new(SectorSize::new(4096).unwrap(), 128 * 1024);
        let mut start = 0;
        sample(&mut sizing, 1e-8, &mut start);
        sample(&mut sizing, 1e-9, &mut start);
        assert_eq!(sizing.bytes(), 128 * 1024);
        assert!(sizing.settled());
        assert_eq!(sizing.prepare(1000, 32, Direction::Forward), (1000, 16));
        let mut slow = ReadSizing::new(SectorSize::LEGACY, DEFAULT_MAX_BYTES);
        sample(&mut slow, 1e-5, &mut 0);
        assert_eq!(slow.bytes(), 128 * 1024);
        assert!(!slow.settled());
    }
    #[test]
    fn truncated_blocks_do_not_grow_and_reverse_shrink_preserves_the_end() {
        let mut sizing = ReadSizing::new(SectorSize::LEGACY, DEFAULT_MAX_BYTES);
        for _ in 0..5 {
            sizing.success(0, 1, Duration::from_millis(1));
        }
        assert_eq!(sizing.bytes(), INITIAL_BYTES);
        assert_eq!(sizing.prepare(1000, 512, Direction::Reverse), (1384, 128));
    }
}
