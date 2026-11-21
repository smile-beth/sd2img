//! Health checks share the same bounded OVERLAPPED request pool as scanning.
use crate::{
    control::{interruptible_sleep, stopping},
    device::{self, AlignedBuffer, PhysicalDrive, ReadResult},
    rescue_map::RescueMap,
};
use std::{
    io,
    time::{Duration, Instant},
};

pub(crate) fn healthy(drive: &PhysicalDrive, map: &RescueMap, timeout_ms: u32) -> io::Result<bool> {
    let observation_ms = crate::watchdog::operation_timeout()
        .as_millis()
        .min((u32::MAX - 1) as u128) as u32;
    check(
        drive,
        map,
        timeout_ms.max(observation_ms),
        "health-reference",
    )
}
pub(crate) fn recovery_matches(
    drive: &PhysicalDrive,
    map: &RescueMap,
    timeout_ms: u32,
) -> io::Result<bool> {
    let observation_ms = crate::watchdog::operation_timeout()
        .as_millis()
        .min((u32::MAX - 1) as u128) as u32;
    check(
        drive,
        map,
        timeout_ms.max(observation_ms),
        "recovery-reference",
    )
}
pub(crate) fn reference_matches(drive: &PhysicalDrive, map: &RescueMap) -> io::Result<bool> {
    check(
        drive,
        map,
        crate::watchdog::operation_timeout()
            .as_millis()
            .min((u32::MAX - 1) as u128) as u32,
        "resume-reference",
    )
}
fn check(
    drive: &PhysicalDrive,
    map: &RescueMap,
    timeout_ms: u32,
    purpose: &'static str,
) -> io::Result<bool> {
    let reference = map.reference();
    let timeout_ms = reference.timeout_ms(timeout_ms);
    info!(
        "[reference selected] LBA={} last_success_ms={} timeout_ms={}",
        reference.lba,
        reference.elapsed.map_or_else(
            || "unknown".to_owned(),
            |elapsed| format!("{:.3}", elapsed.as_secs_f64() * 1000.0)
        ),
        timeout_ms
    );
    let started = Instant::now();
    if !device::has_reference_capacity(map.sector_size.bytes())
        && !device::overlaps_pending(reference.lba, 1)
    {
        // No new probe can be issued. Do not treat capacity pressure as a
        // negative device response; the scanner must wait for completions.
        info!(
            "[reference deferred] LBA={} reason=read-pool-full",
            reference.lba
        );
    }
    let mut buffer = AlignedBuffer::new(map.sector_size.bytes())?;
    // Reuse a pending reference request instead of issuing duplicates. A fresh
    // check must never be satisfied by a different region's successful read.
    let issued = device::overlaps_pending(reference.lba, 1);
    let remaining = Duration::from_millis(timeout_ms as u64).saturating_sub(if issued {
        device::pending_elapsed(reference.lba).unwrap_or_default()
    } else {
        Duration::ZERO
    });
    while !stopping() {
        if issued {
            if let Some(elapsed) = device::completed_reference_elapsed(
                drive.generation(),
                reference.lba,
                &reference.data,
            )? {
                map.reference_success(reference.lba, &reference.data, elapsed);
                return Ok(true);
            }
        }
        if !issued && device::has_reference_capacity(map.sector_size.bytes()) {
            match drive.read_sectors_timeout_for(
                reference.lba,
                1,
                &mut buffer,
                timeout_ms.max(1),
                purpose,
            ) {
                ReadResult::Success => {
                    if buffer.as_slice(map.sector_size.bytes()) != reference.data {
                        return Err(io::Error::other(tr!("Reference sector mismatch; refusing to mix different cards", "基準セクタが一致しません。異なるカードのデータ混在を防ぐため停止します")));
                    }
                    map.reference_success(
                        reference.lba,
                        &reference.data,
                        drive.last_read_elapsed(),
                    );
                    return Ok(true);
                }
                _ => return Ok(false),
            }
        }
        crate::visual::probing(device::pending_ranges());
        if started.elapsed() >= remaining {
            return Ok(false);
        }
        interruptible_sleep(Duration::from_millis(100));
    }
    Ok(false)
}
