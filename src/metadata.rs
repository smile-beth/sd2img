//! Best-effort metadata scheduling. Hints never change recovered bytes or map states.
use crate::{
    rescue_map::{RescueMap, SectorStatus},
    sector::SectorSize,
};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
};

#[cfg(test)]
const HEAD_SECTORS: u64 = 32 * 1024 * 1024 / 512;

#[derive(Clone, Copy)]
struct GptTable {
    start: u64,
    entries: u32,
    entry_size: u32,
}

pub(crate) struct MetadataPriority {
    total: u64,
    sector_size: SectorSize,
    ranges: Vec<(u64, u64)>, // half-open sector ranges, sorted and merged
    partitions: Vec<(u64, u64)>,
    tables: Vec<GptTable>,
}

fn u16le(data: &[u8], offset: usize) -> u64 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) as u64
}
fn u32le(data: &[u8], offset: usize) -> u64 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as u64
}
fn u64le(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

impl MetadataPriority {
    pub(crate) fn contiguous(
        &self,
        map: &RescueMap,
        status: SectorStatus,
        start: u64,
        max_len: u64,
    ) -> Option<(u64, u64)> {
        let &(_, end) = self
            .ranges
            .iter()
            .find(|&&(first, end)| first <= start && start < end)?;
        map.range_at(status, start, max_len.min(end - start))
    }

    pub(crate) fn random_next(
        &self,
        map: &RescueMap,
        status: SectorStatus,
        max_len: u64,
        rng: &mut crate::scan_order::RandomOrder,
    ) -> Option<(u64, u64)> {
        let ranges: Vec<_> = self
            .ranges
            .iter()
            .flat_map(|&(first, end)| {
                map.extents.iter().filter_map(move |extent| {
                    let start = first.max(extent.start);
                    let end = end.min(extent.start.saturating_add(extent.len));
                    (extent.status == status && start < end).then_some((start, end))
                })
            })
            .collect();
        let count: u64 = ranges
            .iter()
            .map(|&(start, end)| (end - start).div_ceil(max_len))
            .sum();
        if count == 0 {
            return None;
        }
        let mut index = rng.below(count);
        for (start, end) in ranges {
            let blocks = (end - start).div_ceil(max_len);
            if index < blocks {
                let start = start + index * max_len;
                return Some((start, (end - start).min(max_len)));
            }
            index -= blocks;
        }
        None
    }
    pub(crate) fn new(total: u64, sector_size: SectorSize) -> Self {
        let mut result = Self {
            total,
            sector_size,
            ranges: Vec::new(),
            partitions: vec![(0, total)],
            tables: Vec::new(),
        };
        result.add(
            0,
            (32 * 1024 * 1024 / sector_size.bytes() as u64).min(total),
        );
        if total > 1 {
            result.add(total - 1, 1);
        }
        result
    }

    fn add(&mut self, start: u64, len: u64) {
        let Some(end) = start.checked_add(len).filter(|&end| end <= self.total) else {
            return;
        };
        if len == 0 {
            return;
        }
        self.ranges.push((start, end));
        self.ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for &(start, end) in &self.ranges {
            if let Some(last) = merged.last_mut().filter(|last| start <= last.1) {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        self.ranges = merged;
    }

    fn partition(&mut self, start: u64, len: u64) {
        if start == 0
            || len == 0
            || start.checked_add(len).is_none_or(|end| end > self.total)
            || self.partitions.iter().any(|&(old, _)| old == start)
        {
            return;
        }
        self.partitions.push((start, len));
        self.add(
            start,
            (32 * 1024 * 1024 / self.sector_size.bytes() as u64).min(len),
        );
    }

    pub(crate) fn next(
        &self,
        map: &RescueMap,
        status: SectorStatus,
        max_len: u64,
    ) -> Option<(u64, u64)> {
        for &(start, end) in &self.ranges {
            for extent in &map.extents {
                if extent.status != status {
                    continue;
                }
                let first = start.max(extent.start);
                let last = end.min(extent.start.saturating_add(extent.len));
                if first < last {
                    return Some((first, (last - first).min(max_len)));
                }
            }
        }
        None
    }

    /// Inspect only successful reads. No additional source-disk I/O is issued.
    pub(crate) fn observe(&mut self, start: u64, bytes: &[u8]) {
        for (index, sector) in bytes.chunks_exact(self.sector_size.bytes()).enumerate() {
            let Some(lba) = start
                .checked_add(index as u64)
                .filter(|&lba| lba < self.total)
            else {
                break;
            };
            self.observe_sector(lba, sector);
        }
    }

    fn observe_sector(&mut self, lba: u64, data: &[u8]) {
        if lba == 0 && data[510..512] == [0x55, 0xaa] {
            for entry in data[446..510].as_chunks::<16>().0 {
                // Extended partitions and the protective MBR are not FAT volumes.
                if ![0, 0x05, 0x0f, 0x85, 0xee].contains(&entry[4]) {
                    self.partition(u32le(entry, 8), u32le(entry, 12));
                }
            }
        }
        if data[..8] == *b"EFI PART"
            && u64le(data, 24) == lba
            && (92..=self.sector_size.bytes() as u64).contains(&u32le(data, 12))
        {
            let table = GptTable {
                start: u64le(data, 72),
                entries: u32le(data, 80) as u32,
                entry_size: u32le(data, 84) as u32,
            };
            // Bound work and support entries contained in one logical sector.
            if table.start > 0
                && (1..=4096).contains(&table.entries)
                && (table.entry_size >= 128
                    && table.entry_size.is_power_of_two()
                    && table.entry_size as usize <= self.sector_size.bytes())
                && table.entries as u64 * table.entry_size as u64 <= 4 * 1024 * 1024
                && !self.tables.iter().any(|old| old.start == table.start)
            {
                let len = (table.entries as u64 * table.entry_size as u64)
                    .div_ceil(self.sector_size.bytes() as u64);
                if table
                    .start
                    .checked_add(len)
                    .is_some_and(|end| end <= self.total)
                {
                    self.tables.push(table);
                    self.add(table.start, len);
                }
            }
        }
        for table in self.tables.clone() {
            if lba < table.start {
                continue;
            }
            let first_entry = (lba - table.start).saturating_mul(self.sector_size.bytes() as u64)
                / table.entry_size as u64;
            if first_entry >= table.entries as u64 {
                continue;
            }
            for (index, entry) in data.chunks_exact(table.entry_size as usize).enumerate() {
                if first_entry + index as u64 >= table.entries as u64 {
                    break;
                }
                if entry[..16].iter().all(|&byte| byte == 0) {
                    continue;
                }
                let start = u64le(entry, 32);
                let end = u64le(entry, 40);
                if let Some(len) = end.checked_sub(start).and_then(|len| len.checked_add(1)) {
                    self.partition(start, len);
                }
            }
        }
        let Some((_, length)) = self
            .partitions
            .iter()
            .find(|&&(start, _)| start == lba)
            .copied()
        else {
            return;
        };
        if data[510..512] != [0x55, 0xaa] {
            return;
        }
        if data[3..11] == *b"EXFAT   " {
            if data[108] as u32 != self.sector_size.bytes().trailing_zeros()
                || ![1, 2].contains(&data[110])
            {
                return;
            }
            let offset = u32le(data, 80);
            let fats = u32le(data, 84) * data[110] as u64;
            let volume_len = u64le(data, 72);
            if offset >= 24
                && fats > 0
                && volume_len <= length
                && offset
                    .checked_add(fats)
                    .is_some_and(|end| end <= u32le(data, 88) && end < volume_len)
            {
                self.add(lba, offset + fats);
            }
        } else if u16le(data, 11) == self.sector_size.bytes() as u64
            && data[13].is_power_of_two()
            && [1, 2].contains(&data[16])
            && (data[54..62] == *b"FAT12   "
                || data[54..62] == *b"FAT16   "
                || data[82..90] == *b"FAT32   ")
        {
            let reserved = u16le(data, 14);
            let fat_len = match u16le(data, 22) {
                0 => u32le(data, 36),
                value => value,
            };
            let volume_len = match u16le(data, 19) {
                0 => u32le(data, 32),
                value => value,
            };
            let root_sectors = (u16le(data, 17) * 32).div_ceil(self.sector_size.bytes() as u64);
            let end = reserved + fat_len * data[16] as u64 + root_sectors;
            if reserved > 0 && fat_len > 0 && volume_len <= length && end < volume_len {
                self.add(lba, end);
            }
        }
    }

    /// Reconstruct hints from already rescued image sectors on resume, never
    /// rereading a source sector solely to rediscover metadata.
    pub(crate) fn seed_from_image(&mut self, map: &RescueMap, image: &mut File) -> io::Result<()> {
        let _operation = crate::watchdog::operation("rebuild metadata hints from image");
        let mut visited = std::collections::HashSet::new();
        loop {
            let mut sectors = vec![0, 1, self.total.saturating_sub(1)];
            sectors.extend(self.partitions.iter().map(|&(start, _)| start));
            for table in &self.tables {
                let count = (table.entries as u64 * table.entry_size as u64)
                    .div_ceil(self.sector_size.bytes() as u64);
                sectors.extend(table.start..table.start + count);
            }
            let mut changed = false;
            for lba in sectors {
                if !visited.insert(lba)
                    || !map.extents.iter().any(|extent| {
                        extent.status == SectorStatus::Rescued
                            && lba >= extent.start
                            && lba - extent.start < extent.len
                    })
                {
                    continue;
                }
                let mut sector = vec![0; self.sector_size.bytes()];
                image.seek(SeekFrom::Start(lba * self.sector_size.bytes() as u64))?;
                image.read_exact(&mut sector)?;
                self.observe(lba, &sector);
                changed = true;
            }
            if !changed {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn signature(data: &mut [u8]) {
        data[510..512].copy_from_slice(&[0x55, 0xaa]);
    }
    fn put32(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn put64(data: &mut [u8], offset: usize, value: u64) {
        data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    #[test]
    fn four_k_partition_and_fat_hints_use_logical_sector_units() {
        let sector = SectorSize::new(4096).unwrap();
        let mut hints = MetadataPriority::new(400_000, sector);
        let mut mbr = vec![0; 4096];
        signature(&mut mbr);
        mbr[450] = 0x07;
        put32(&mut mbr, 454, 20_000);
        put32(&mut mbr, 458, 300_000);
        hints.observe(0, &mbr);
        let mut boot = vec![0; 4096];
        signature(&mut boot);
        boot[3..11].copy_from_slice(b"EXFAT   ");
        put64(&mut boot, 72, 300_000);
        put32(&mut boot, 80, 256);
        put32(&mut boot, 84, 10_000);
        put32(&mut boot, 88, 10_256);
        boot[108] = 12;
        boot[110] = 1;
        hints.observe(20_000, &boot);
        assert!(hints.ranges.contains(&(0, 8192)));
        assert!(hints.ranges.contains(&(20_000, 30_256)));
    }

    #[test]
    fn mbr_and_fat32_reach_beyond_initial_head_without_rereading_good_sectors() {
        let total = 2_000_000;
        let start = 200_000;
        let mut hints = MetadataPriority::new(total, SectorSize::LEGACY);
        let mut mbr = [0; 512];
        signature(&mut mbr);
        mbr[450] = 0x0c;
        put32(&mut mbr, 454, start);
        put32(&mut mbr, 458, 1_000_000);
        hints.observe(0, &mbr);
        let mut boot = [0; 512];
        signature(&mut boot);
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 8;
        boot[14..16].copy_from_slice(&32u16.to_le_bytes());
        boot[16] = 2;
        put32(&mut boot, 32, 1_000_000);
        put32(&mut boot, 36, 40_000);
        boot[82..90].copy_from_slice(b"FAT32   ");
        hints.observe(start as u64, &boot);
        let mut map = RescueMap::new(0, mbr.to_vec(), total);
        map.update_range(0, HEAD_SECTORS, SectorStatus::Rescued);
        assert_eq!(
            hints.next(&map, SectorStatus::Untried, 128),
            Some((start as u64, 128))
        );
        map.update_range(start as u64, HEAD_SECTORS, SectorStatus::Rescued);
        assert_eq!(
            hints.next(&map, SectorStatus::Untried, 128),
            Some((start as u64 + HEAD_SECTORS, 128))
        );
    }

    #[test]
    fn gpt_and_exfat_discover_partition_and_all_fat_copies() {
        let mut hints = MetadataPriority::new(4_000_000, SectorSize::LEGACY);
        let mut header = [0; 512];
        header[..8].copy_from_slice(b"EFI PART");
        put32(&mut header, 12, 92);
        put64(&mut header, 24, 1);
        put64(&mut header, 72, 2);
        put32(&mut header, 80, 128);
        put32(&mut header, 84, 128);
        hints.observe(1, &header);
        let mut table = [0; 512];
        table[0] = 1;
        put64(&mut table, 32, 500_000);
        put64(&mut table, 40, 3_499_999);
        hints.observe(2, &table);
        let mut boot = [0; 512];
        signature(&mut boot);
        boot[3..11].copy_from_slice(b"EXFAT   ");
        put64(&mut boot, 72, 3_000_000);
        put32(&mut boot, 80, 4096);
        put32(&mut boot, 84, 80_000);
        put32(&mut boot, 88, 164_096);
        boot[108] = 9;
        boot[110] = 2;
        hints.observe(500_000, &boot);
        assert!(hints
            .ranges
            .iter()
            .any(|&(start, end)| start == 500_000 && end == 664_096));
    }

    #[test]
    fn corrupt_bounds_do_not_add_out_of_disk_ranges_and_failures_are_skipped() {
        let mut hints = MetadataPriority::new(1000, SectorSize::LEGACY);
        let mut mbr = [0; 512];
        signature(&mut mbr);
        mbr[450] = 0x0c;
        put32(&mut mbr, 454, u32::MAX);
        put32(&mut mbr, 458, u32::MAX);
        hints.observe(0, &mbr);
        assert_eq!(hints.partitions, [(0, 1000)]);
        let mut map = RescueMap::new(0, mbr.to_vec(), 1000);
        map.update_range(0, 128, SectorStatus::Deferred);
        assert_eq!(
            hints.next(&map, SectorStatus::Untried, 128),
            Some((128, 128))
        );
        map.update_range(128, 872, SectorStatus::Rescued);
        assert_eq!(hints.next(&map, SectorStatus::Untried, 128), None);
        assert_eq!(hints.next(&map, SectorStatus::Deferred, 1), Some((0, 1)));
    }
}
