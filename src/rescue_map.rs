use crate::{cli::Direction, scan_order::RandomOrder, sector::SectorSize};
use std::fs::{self, File};
use std::io::{self, Write};
use std::{cell::RefCell, collections::VecDeque, time::Duration};

#[derive(Clone)]
pub(crate) struct ReferenceCandidate {
    pub(crate) lba: u64,
    pub(crate) data: Vec<u8>,
    pub(crate) elapsed: Option<Duration>,
}
impl ReferenceCandidate {
    pub(crate) fn timeout_ms(&self, fallback: u32) -> u32 {
        self.elapsed.map_or(fallback.max(1), |elapsed| {
            elapsed
                .saturating_add(Duration::from_secs(2))
                .as_nanos()
                .div_ceil(1_000_000)
                .min((u32::MAX - 1) as u128) as u32
        })
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub(crate) enum SectorStatus {
    Untried,  // ? : 未処理
    Rescued,  // + : 正常吸い出し済み
    Bad,      // - : 旧バージョンで不良と判定
    Deferred, // ~ : 大ブロック走査済み、awaiting sector scan
    Pending,  // * : 抜き差しで回復する可能性があるためawaiting retry
}

impl SectorStatus {
    pub(crate) fn as_char(self) -> char {
        match self {
            SectorStatus::Untried => '?',
            SectorStatus::Rescued => '+',
            SectorStatus::Bad => '-',
            SectorStatus::Pending => '*',
            SectorStatus::Deferred => '~',
        }
    }

    pub(crate) fn from_char(c: char) -> Option<Self> {
        match c {
            '?' => Some(SectorStatus::Untried),
            '+' => Some(SectorStatus::Rescued),
            '-' => Some(SectorStatus::Bad),
            '*' => Some(SectorStatus::Pending),
            '~' => Some(SectorStatus::Deferred),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Extent {
    pub(crate) start: u64,
    pub(crate) len: u64,
    pub(crate) status: SectorStatus,
}

/// 吸い出し状況と正常基準セクタを保持するマップ構造体
pub(crate) struct RescueMap {
    pub(crate) sector_size: SectorSize,
    pub(crate) good_lba: u64,
    pub(crate) good_data: Vec<u8>,
    pub(crate) extents: Vec<Extent>,
    // Runtime observations only: never serialized into the map or rendered.
    references: RefCell<VecDeque<ReferenceCandidate>>,
}

impl RescueMap {
    pub(crate) fn reference(&self) -> ReferenceCandidate {
        self.references
            .borrow()
            .iter()
            .rev()
            .min_by_key(|candidate| candidate.elapsed)
            .cloned()
            .unwrap_or_else(|| ReferenceCandidate {
                lba: self.good_lba,
                data: self.good_data.clone(),
                elapsed: None,
            })
    }
    pub(crate) fn reference_success(&self, lba: u64, data: &[u8], elapsed: Duration) {
        if data.len() != self.sector_size.bytes() {
            return;
        }
        let mut candidates = self.references.borrow_mut();
        candidates.retain(|candidate| candidate.lba != lba);
        candidates.push_back(ReferenceCandidate {
            lba,
            data: data.to_vec(),
            elapsed: Some(elapsed),
        });
        while candidates.len() > 30 {
            candidates.pop_front();
        }
    }
    pub(crate) fn reference_completed(
        &self,
        lba: u64,
        data: &[u8],
        elapsed: Duration,
    ) -> io::Result<()> {
        let expected = self
            .references
            .borrow()
            .iter()
            .find(|candidate| candidate.lba == lba)
            .map(|candidate| candidate.data.clone())
            .or_else(|| (lba == self.good_lba).then(|| self.good_data.clone()));
        if let Some(expected) = expected {
            if expected != data {
                return Err(io::Error::other(tr!(
                    "Reference sector mismatch; rescued image data was preserved",
                    "基準セクタが一致しません。救出済みイメージは変更していません"
                )));
            }
            self.reference_success(lba, data, elapsed);
        }
        Ok(())
    }
    pub(crate) fn remember_timed_success(&mut self, start: u64, data: &[u8], elapsed: Duration) {
        self.remember_success(start, data);
        if !data.is_empty() && data.len().is_multiple_of(self.sector_size.bytes()) {
            // A scan's whole-request duration is a conservative provisional
            // observation, replaced by the actual one-sector check on success.
            self.reference_success(self.good_lba, &self.good_data, elapsed);
        }
    }
    /// Use the final sector of the latest successfully read and written range.
    pub(crate) fn remember_success(&mut self, start: u64, data: &[u8]) {
        let size = self.sector_size.bytes();
        if data.is_empty() || !data.len().is_multiple_of(size) {
            return;
        }
        self.good_lba = start + (data.len() / size) as u64 - 1;
        self.good_data.clear();
        self.good_data.extend_from_slice(&data[data.len() - size..]);
        crate::logging::append(&message!(
            "Reference updated: LBA {}",
            "正常確認用の基準更新: LBA {}",
            self.good_lba
        ));
    }
    pub(crate) fn validate(&self, total: u64) -> io::Result<()> {
        let invalid = || {
            io::Error::new(io::ErrorKind::InvalidData, tr!(
            "Map ranges or reference sector do not match the source disk size. The image and map were not modified.",
            "マップの範囲または基準セクタが元ディスクの容量と一致しません。イメージとマップは変更していません。"))
        };
        if total == 0 || self.good_lba >= total || self.good_data.len() != self.sector_size.bytes()
        {
            return Err(invalid());
        }
        let mut next = 0;
        for extent in &self.extents {
            if extent.start != next || extent.len == 0 {
                return Err(invalid());
            }
            next = next
                .checked_add(extent.len)
                .filter(|&end| end <= total)
                .ok_or_else(invalid)?;
        }
        if next != total {
            return Err(invalid());
        }
        Ok(())
    }
    pub(crate) fn range_at(
        &self,
        status: SectorStatus,
        start: u64,
        max_len: u64,
    ) -> Option<(u64, u64)> {
        self.extents
            .iter()
            .find(|extent| {
                extent.status == status
                    && start >= extent.start
                    && start - extent.start < extent.len
            })
            .map(|extent| (start, (extent.len - (start - extent.start)).min(max_len)))
    }
    #[cfg(test)]
    pub(crate) fn new(good_lba: u64, good_data: Vec<u8>, total_sectors: u64) -> Self {
        Self::new_with_sector(good_lba, good_data, total_sectors, SectorSize::LEGACY)
    }
    pub(crate) fn new_with_sector(
        good_lba: u64,
        good_data: Vec<u8>,
        total_sectors: u64,
        sector_size: SectorSize,
    ) -> Self {
        Self {
            sector_size,
            good_lba,
            good_data,
            references: RefCell::default(),
            extents: vec![Extent {
                start: 0,
                len: total_sectors,
                status: SectorStatus::Untried,
            }],
        }
    }

    /// 区間 [start, start + len) のステータスを更新し、隣接する同ステータスを結合
    pub(crate) fn update_range(&mut self, start: u64, len: u64, status: SectorStatus) {
        if len == 0 {
            return;
        }
        let end = start + len;
        let mut next = Vec::with_capacity(self.extents.len() + 2);

        for ext in &self.extents {
            let ext_end = ext.start + ext.len;
            if ext_end <= start
                || ext.start >= end
                || (ext.status == SectorStatus::Rescued && status != SectorStatus::Rescued)
            {
                next.push(*ext);
            } else {
                if ext.start < start {
                    next.push(Extent {
                        start: ext.start,
                        len: start - ext.start,
                        status: ext.status,
                    });
                }
                let overlap_start = ext.start.max(start);
                let overlap_end = ext_end.min(end);
                next.push(Extent {
                    start: overlap_start,
                    len: overlap_end - overlap_start,
                    status,
                });
                if ext_end > end {
                    next.push(Extent {
                        start: end,
                        len: ext_end - end,
                        status: ext.status,
                    });
                }
            }
        }

        // 隣接する同じステータスの区間をマージ
        let mut merged: Vec<Extent> = Vec::with_capacity(next.len());
        for ext in next {
            if let Some(last) = merged.last_mut() {
                if last.status == ext.status && last.start + last.len == ext.start {
                    last.len += ext.len;
                    continue;
                }
            }
            merged.push(ext);
        }
        self.extents = merged;
    }

    /// 各区間を最大 max_len ごとのブロックに分け、順序に従って一つ選ぶ。
    pub(crate) fn next_range(
        &self,
        status: SectorStatus,
        max_len: u64,
        direction: Direction,
        rng: &mut RandomOrder,
    ) -> Option<(u64, u64)> {
        let matching = || {
            self.extents
                .iter()
                .filter(|e| e.status == status && e.len > 0)
        };
        match direction {
            Direction::Forward => matching().next().map(|e| (e.start, e.len.min(max_len))),
            Direction::Reverse => matching().next_back().map(|e| {
                let len = e.len.min(max_len);
                (e.start + e.len - len, len)
            }),
            Direction::Random => {
                let count: u64 = matching().map(|e| e.len.div_ceil(max_len)).sum();
                if count == 0 {
                    return None;
                }
                let mut index = rng.below(count);
                for e in matching() {
                    let blocks = e.len.div_ceil(max_len);
                    if index < blocks {
                        let offset = index * max_len;
                        return Some((e.start + offset, (e.len - offset).min(max_len)));
                    }
                    index -= blocks;
                }
                unreachable!()
            }
        }
    }

    pub(crate) fn retry_pending(&mut self) {
        let ranges: Vec<_> = self
            .extents
            .iter()
            .filter(|e| e.status == SectorStatus::Pending)
            .map(|e| (e.start, e.len))
            .collect();
        for (start, len) in ranges {
            self.update_range(start, len, SectorStatus::Deferred);
        }
    }

    pub(crate) fn stats(&self) -> (u64, u64, u64) {
        let (mut rescued, mut bad, mut untried) = (0, 0, 0);
        for ext in &self.extents {
            match ext.status {
                SectorStatus::Rescued => rescued += ext.len,
                SectorStatus::Bad => bad += ext.len,
                SectorStatus::Untried | SectorStatus::Pending | SectorStatus::Deferred => {
                    untried += ext.len
                }
            }
        }
        (rescued, bad, untried)
    }

    /// マップファイルを保存（一時ファイル経由でアトミックに上書き）
    pub(crate) fn save(&self, path: &str) -> io::Result<()> {
        let _operation = crate::watchdog::operation("save map");
        let tmp_path = format!("{}.tmp", path);
        {
            let mut f = File::create(&tmp_path)?;
            let hex: String = self
                .good_data
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect();
            writeln!(f, "SECTOR_SIZE {}", self.sector_size.bytes())?;
            writeln!(f, "GOOD {} {}", self.good_lba, hex)?;
            for ext in &self.extents {
                writeln!(f, "{} {} {}", ext.start, ext.len, ext.status.as_char())?;
            }
            f.flush()?;
            f.sync_all()?;
        }
        fs::rename(tmp_path, path)?;
        Ok(())
    }

    pub(crate) fn load(path: &str) -> io::Result<Self> {
        let _operation = crate::watchdog::operation("load map");
        let content = fs::read_to_string(path)?;
        let mut lines = content.lines();

        let first = lines.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                tr!("Map file is empty", "マップファイルが空です"),
            )
        })?;
        let (sector_size, header) = if first.starts_with("SECTOR_SIZE") {
            let parts: Vec<_> = first.split_whitespace().collect();
            if parts.len() != 2 || parts[0] != "SECTOR_SIZE" {
                return Err(io::Error::other(tr!(
                    "Invalid sector-size header",
                    "セクタサイズのヘッダーが不正です"
                )));
            }
            let size = parts[1].parse::<usize>().map_err(io::Error::other)?;
            (SectorSize::new(size)?, lines.next())
        } else {
            (SectorSize::LEGACY, Some(first))
        };
        let header = header.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                tr!("Map file is empty", "マップファイルが空です"),
            )
        })?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        if parts.len() != 3 || parts[0] != "GOOD" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                tr!("Invalid map header", "マップのヘッダーが不正です"),
            ));
        }
        let good_lba: u64 = parts[1].parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                tr!("Invalid reference LBA", "基準セクタのLBAが不正です"),
            )
        })?;
        let hex = parts[2];
        if hex.len() != sector_size.bytes() * 2 || !hex.is_ascii() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                tr!(
                    "Invalid reference sector data length",
                    "基準セクタのデータ長が不正です"
                ),
            ));
        }
        let mut good_data = Vec::with_capacity(sector_size.bytes());
        for i in 0..sector_size.bytes() {
            let byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    tr!(
                        "Invalid hexadecimal sector data",
                        "セクタデータの16進表記が不正です"
                    ),
                )
            })?;
            good_data.push(byte);
        }

        let mut extents = Vec::new();
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let p: Vec<&str> = line.split_whitespace().collect();
            if p.len() != 3 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    tr!("Malformed map extent", "マップの区間情報が不正です"),
                ));
            }
            let start: u64 = p[0].parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    tr!("Invalid extent start LBA", "区間の開始LBAが不正です"),
                )
            })?;
            let len: u64 = p[1].parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    tr!("Invalid extent sector count", "区間のセクタ数が不正です"),
                )
            })?;
            if p[2].len() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    tr!("Invalid map status", "マップの状態が不正です"),
                ));
            }
            let status_char = p[2].chars().next().unwrap_or('?');
            let status = SectorStatus::from_char(status_char).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    tr!("Invalid extent status", "区間の状態が不正です"),
                )
            })?;
            extents.push(Extent { start, len, status });
        }

        Ok(Self {
            sector_size,
            good_lba,
            good_data,
            extents,
            references: RefCell::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SECTOR_SIZE;
    #[test]
    fn rescued_ranges_never_revert_to_failure_or_untried() {
        for status in [
            SectorStatus::Untried,
            SectorStatus::Bad,
            SectorStatus::Deferred,
            SectorStatus::Pending,
        ] {
            let mut map = RescueMap::new(0, vec![0; 512], 10);
            map.update_range(3, 4, SectorStatus::Rescued);
            map.update_range(0, 10, status);
            assert_eq!(map.range_at(SectorStatus::Rescued, 3, 10), Some((3, 4)));
            assert_eq!(map.stats().0, 4);
            assert!(map
                .extents
                .iter()
                .filter(|extent| extent.status != SectorStatus::Rescued)
                .all(|extent| extent.status == status));
            assert!(map.validate(10).is_ok());
        }
    }
    #[test]
    fn reference_candidates_are_recent_fastest_and_use_latest_observation_plus_two_seconds() {
        let map = RescueMap::new(0, vec![0; 512], 100);
        assert_eq!(map.reference().timeout_ms(10000), 10000);
        for lba in 1..=30 {
            map.reference_success(lba, &vec![lba as u8; 512], Duration::from_millis(lba));
        }
        assert_eq!(map.reference().lba, 1);
        assert_eq!(map.reference().timeout_ms(10000), 2001);
        // Slow revalidation replaces the old fast measurement, not its image status.
        map.reference_success(1, &vec![1; 512], Duration::from_secs(5));
        assert_eq!(map.reference().lba, 2);
        map.reference_success(31, &vec![31; 512], Duration::from_millis(31));
        assert_eq!(map.references.borrow().len(), 30);
        assert_eq!(map.reference().lba, 3);
        assert_eq!(map.reference().timeout_ms(1), 2003);
        map.reference_success(31, &vec![31; 512], Duration::from_micros(1));
        assert_eq!(map.reference().lba, 31);
        assert_eq!(map.reference().timeout_ms(10000), 2001);
        assert!(map
            .reference_completed(31, &vec![0; 512], Duration::from_secs(1))
            .is_err());
        assert_eq!(map.reference().lba, 31);
    }
    #[test]
    fn candidate_history_is_not_serialized_into_map() -> io::Result<()> {
        let mut map = RescueMap::new(0, vec![0; 512], 100);
        map.remember_timed_success(10, &vec![7; 1024], Duration::from_millis(20));
        map.reference_success(25, &vec![8; 512], Duration::from_millis(1));
        assert_eq!(map.reference().lba, 25);
        let path =
            std::env::temp_dir().join(format!("sd2img-candidates-{}.map", std::process::id()));
        map.save(path.to_str().unwrap())?;
        let content = fs::read_to_string(&path)?;
        assert_eq!(content.lines().count(), 3);
        assert_eq!(
            content
                .lines()
                .filter(|line| line.starts_with("GOOD "))
                .count(),
            1
        );
        let loaded = RescueMap::load(path.to_str().unwrap())?;
        assert!(loaded.references.borrow().is_empty());
        assert_eq!(loaded.reference().lba, 11);
        assert_eq!(loaded.reference().timeout_ms(10000), 10000);
        fs::remove_file(path)
    }
    #[test]
    fn latest_success_replaces_reference_including_reverse_and_delayed_reads() -> io::Result<()> {
        for size in [512, 4096] {
            let mut map = RescueMap::new_with_sector(0, vec![0; size], 100, SectorSize::new(size)?);
            let mut data = vec![1; size * 2];
            data[size..].fill(2);
            map.remember_success(50, &data);
            assert_eq!(map.good_lba, 51);
            assert_eq!(map.good_data, vec![2; size]);
            // Completion order, rather than the highest LBA, determines the reference.
            map.remember_success(3, &vec![3; size]);
            assert_eq!(map.good_lba, 3);
            assert_eq!(map.good_data, vec![3; size]);
            map.remember_success(10, &[]);
            map.remember_success(10, &[0]);
            assert_eq!(map.good_lba, 3);
            let path = std::env::temp_dir().join(format!(
                "sd2img-latest-reference-{}-{size}.map",
                std::process::id()
            ));
            map.save(path.to_str().unwrap())?;
            let loaded = RescueMap::load(path.to_str().unwrap())?;
            assert_eq!(loaded.good_lba, 3);
            assert_eq!(loaded.good_data, map.good_data);
            loaded.validate(100)?;
            fs::remove_file(path)?;
        }
        Ok(())
    }
    #[test]
    fn legacy_maps_default_to_512_and_explicit_4k_maps_round_trip() -> io::Result<()> {
        let path =
            std::env::temp_dir().join(format!("sd2img-sector-map-{}.map", std::process::id()));
        fs::write(&path, format!("GOOD 0 {}\n0 3 ?\n", "ab".repeat(512)))?;
        let legacy = RescueMap::load(path.to_str().unwrap())?;
        assert_eq!(legacy.sector_size, SectorSize::LEGACY);
        legacy.validate(3)?;
        let map = RescueMap::new_with_sector(1, vec![0xcd; 4096], 3, SectorSize::new(4096)?);
        map.save(path.to_str().unwrap())?;
        assert!(fs::read_to_string(&path)?.starts_with("SECTOR_SIZE 4096\n"));
        let restored = RescueMap::load(path.to_str().unwrap())?;
        assert_eq!(restored.sector_size.bytes(), 4096);
        assert_eq!(restored.good_data, map.good_data);
        restored.validate(3)?;
        fs::write(
            &path,
            format!("SECTOR_SIZE 4096\nGOOD 0 {}\n0 3 ?\n", "ab".repeat(512)),
        )?;
        assert!(RescueMap::load(path.to_str().unwrap()).is_err());
        fs::remove_file(path)
    }
    #[test]
    fn resume_rejects_size_mismatch_gaps_overlaps_and_overflow() {
        let mut map = RescueMap::new(0, vec![0; SECTOR_SIZE], 10);
        assert!(map.validate(10).is_ok());
        assert!(map.validate(11).is_err());
        map.extents[0].start = 1;
        assert!(map.validate(10).is_err());
        map.extents[0].start = 0;
        map.extents[0].len = u64::MAX;
        map.extents.push(Extent {
            start: u64::MAX,
            len: 1,
            status: SectorStatus::Untried,
        });
        assert!(map.validate(u64::MAX).is_err());
    }
    #[test]
    fn coarse_scan_visits_every_sector_once_before_fine_scan() {
        for direction in [Direction::Forward, Direction::Reverse, Direction::Random] {
            let mut map = RescueMap::new(0, vec![0; SECTOR_SIZE], 259);
            let mut rng = RandomOrder(42);
            let mut visited = vec![false; 259];
            let mut starts = Vec::new();
            while let Some((start, len)) =
                map.next_range(SectorStatus::Untried, 128, direction, &mut rng)
            {
                starts.push(start);
                for lba in start..start + len {
                    assert!(!visited[lba as usize]);
                    visited[lba as usize] = true;
                }
                // 全ブロック失敗でも未走査を優先し、走査済みを再選択しない。
                map.update_range(start, len, SectorStatus::Deferred);
            }
            assert!(visited.iter().all(|seen| *seen));
            assert_eq!(starts.len(), 3);
            if direction == Direction::Forward {
                assert_eq!(starts, vec![0, 128, 256]);
            }
            if direction == Direction::Reverse {
                assert_eq!(starts, vec![131, 3, 0]);
            }
            let mut fine_count = 0;
            while let Some((start, len)) =
                map.next_range(SectorStatus::Deferred, 1, direction, &mut rng)
            {
                assert_eq!(len, 1);
                map.update_range(start, len, SectorStatus::Pending);
                fine_count += 1;
            }
            assert_eq!(fine_count, 259);
            assert_eq!(map.stats(), (0, 0, 259));
        }
    }
    #[test]
    fn retry_preserves_rescued_and_legacy_bad_ranges() {
        let mut map = RescueMap::new(0, vec![7; SECTOR_SIZE], 8);
        map.update_range(0, 2, SectorStatus::Rescued);
        map.update_range(2, 3, SectorStatus::Pending);
        map.update_range(5, 1, SectorStatus::Bad);
        assert_eq!(map.stats(), (2, 1, 5));
        map.retry_pending();
        assert_eq!(
            map.next_range(
                SectorStatus::Deferred,
                128,
                Direction::Forward,
                &mut RandomOrder(1)
            ),
            Some((2, 3))
        );
        assert_eq!(map.stats(), (2, 1, 5));
        map.update_range(2, 3, SectorStatus::Rescued);
        assert_eq!(map.extents[0].len, 5);
        assert_eq!(map.stats(), (5, 1, 2));
    }
    #[test]
    fn pending_survives_map_save_and_restart() -> io::Result<()> {
        let path = std::env::temp_dir().join(format!("sd2img-test-{}.map", std::process::id()));
        let path = path.to_str().unwrap();
        let mut map = RescueMap::new(0, vec![7; SECTOR_SIZE], 4);
        map.update_range(0, 2, SectorStatus::Rescued);
        map.update_range(2, 2, SectorStatus::Pending);
        map.save(path)?;
        let mut restored = RescueMap::load(path)?;
        fs::remove_file(path)?;
        assert_eq!(restored.stats(), (2, 0, 2));
        assert!(restored
            .next_range(
                SectorStatus::Deferred,
                128,
                Direction::Forward,
                &mut RandomOrder(1)
            )
            .is_none());
        restored.retry_pending();
        assert_eq!(
            restored.next_range(
                SectorStatus::Deferred,
                128,
                Direction::Forward,
                &mut RandomOrder(1)
            ),
            Some((2, 2))
        );
        Ok(())
    }
}
