use crate::{
    cli::Args,
    control::{console_handler, interruptible_sleep, stopping},
    device::{
        resolve_device_path, try_reconnect, AlignedBuffer, PhysicalDrive, ReadResult, Reconnection,
    },
    output::{format_size, print_progress, sector_count},
    read_size::INITIAL_BYTES,
    rescue_map::{RescueMap, SectorStatus},
    scan_order::Scheduler,
    volumes,
};
use std::io::{self, Seek, SeekFrom, Write};
use std::time::{Duration, Instant};
use windows::Win32::System::Console::SetConsoleCtrlHandler;

pub(crate) fn run(args: Args) -> io::Result<()> {
    if !args.timeout.is_finite() || args.timeout <= 0.0 || args.timeout * 1000.0 >= u32::MAX as f64
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "timeout must be a positive finite number of seconds within the supported range",
                "timeoutは対応範囲内の正の有限秒数を指定してください"
            ),
        ));
    }
    unsafe { SetConsoleCtrlHandler(Some(console_handler), true) }.map_err(io::Error::other)?;

    let mut device_path = resolve_device_path(&args.drive, true)?;
    let output_path = &args.out;
    crate::visual::disk(&device_path, output_path, 0, args.direction.label());
    let map_path = format!("{}.map", output_path);
    let timeout_ms = (args.timeout * 1000.0) as u32;

    info!(
        "{}",
        message!("Source device: {}", "対象デバイス: {}", device_path)
    );
    info!(
        "{}",
        message!(
            "Output image: {} (map: {})",
            "出力イメージ: {}（マップ: {}）",
            output_path,
            map_path
        )
    );
    info!(
        "{}",
        message!(
            "Initial/minimum read timeout: {} ms (auto-scaled)",
            "読取タイムアウト初期値・下限: {} ms（自動調整）",
            timeout_ms
        )
    );

    let source_number = volumes::disk_number(&device_path)?;
    volumes::check_destination(output_path, source_number)?;
    volumes::check_destination(&map_path, source_number)?;
    volumes::check_destination(&format!("{map_path}.tmp"), source_number)?;
    let log_path = format!("{output_path}.log");
    volumes::check_destination(&log_path, source_number)?;
    crate::output::open_log(&log_path)?;
    info!("build={} settings={args:?}", env!("SD2IMG_BUILD"));
    info!(
        "{}",
        message!(
            "Appending rescue events to: {}",
            "救出イベントを追記します: {}",
            log_path
        )
    );
    info!("{}", tr!("Opening the source handle before protecting volumes; scan reads start only after locking.", "ロック前に元ディスクのハンドルを開きます。読み取りはロック完了後に開始します。"));
    let mut volume_locks = volumes::VolumeLocks::default();
    let mut drive =
        crate::device::open_protected(&device_path, &mut volume_locks).map_err(|error| {
            io::Error::new(
                error.kind(),
                message!(
                    "Cannot prepare source device {device_path}: {error}",
                    "元デバイス {device_path} の準備に失敗しました: {error}"
                ),
            )
        })?;
    if stopping() {
        return Ok(());
    }
    let total_sectors = drive.total_sectors()?;
    let sector_size = drive.sector_size();
    let total_bytes = total_sectors * sector_size.bytes() as u64;
    crate::visual::settings(sector_size);
    crate::visual::operation_settings(args.max_parallel_reads as usize);
    let reset_instance = crate::windows_native::device_instance_for_disk(source_number).ok();
    crate::visual::disk(
        &device_path,
        output_path,
        total_sectors,
        args.direction.label(),
    );
    info!(
        "{}",
        message!(
            "Source size: {}",
            "ディスク容量: {}",
            sector_count(total_sectors, sector_size)
        )
    );

    let mut read_buf = AlignedBuffer::new(INITIAL_BYTES)?;

    // 1. 既存のマップファイルがあれば途中再開、なければ新規作成
    let mut map = if crate::image::exists(&map_path)? {
        let mut loaded = RescueMap::load(&map_path)?;
        if loaded.sector_size != sector_size {
            return Err(io::Error::other(message!("Map sector size {} differs from device sector size {}. Refusing to reinterpret LBAs.",
                "マップのセクタサイズ {} とデバイスのセクタサイズ {} が一致しません。LBAを再解釈せず停止します。", loaded.sector_size.bytes(), sector_size.bytes())));
        }
        loaded.validate(total_sectors)?;
        if loaded.stats().0 > 0 {
            drop(crate::image::open(output_path, total_bytes, true)?);
        }
        let mut recovery_requested = false;
        loop {
            if stopping() {
                return Ok(());
            }
            if crate::probes::reference_matches(&drive, &loaded)? {
                break;
            }
            if !recovery_requested {
                if let Some(path) = reset_source(
                    &mut drive,
                    &mut volume_locks,
                    reset_instance.as_deref(),
                    total_sectors,
                    &loaded,
                ) {
                    device_path = path;
                }
                recovery_requested = true;
            }
            crate::reset::poll_pending();
            match try_reconnect(
                &args.drive,
                &device_path,
                &drive,
                &loaded,
                &mut read_buf,
                timeout_ms,
                &mut volume_locks,
            )? {
                Reconnection::Ready(reopened, path) => {
                    drive = reopened;
                    device_path = path;
                    break;
                }
                Reconnection::Retained => break,
                _ => interruptible_sleep(Duration::from_secs(1)),
            }
        }

        if args.retry_bad {
            let bad_ranges: Vec<(u64, u64)> = loaded
                .extents
                .iter()
                .filter(|e| e.status == SectorStatus::Bad)
                .map(|e| (e.start, e.len))
                .collect();
            for (s, l) in bad_ranges {
                loaded.update_range(s, l, SectorStatus::Untried);
            }
            info!(
                "{}",
                message!(
                    "Reset legacy bad sectors to untried.",
                    "旧NGセクタを未処理に戻しました。"
                )
            );
        }

        let (rescued, bad, untried) = loaded.stats();
        info!(
            "{}",
            message!(
                "Loaded map (resuming): recovered={} / legacy bad={} / unresolved={}",
                "マップを読み込み再開します: 救出済={} / 旧NG={} / 未確定={}",
                sector_count(rescued, sector_size),
                sector_count(bad, sector_size),
                sector_count(untried, sector_size)
            )
        );
        loaded
    } else {
        crate::image::ensure_new_destination(output_path)?;
        let mut found = None;
        let mut lba = 0;
        let mut discovery_reset = false;
        while lba < total_sectors.min(10_000) || crate::device::pending_reads() > 0 {
            if let Some(completed) = crate::device::take_completed().into_iter().next() {
                found = Some((completed.lba, completed.data));
                break;
            }
            if !drive.can_reuse() {
                if let Some(id) = reset_instance.as_deref() {
                    if let Ok(path) = crate::windows_native::disk_for_instance(id) {
                        device_path = path;
                    }
                }
                if let Ok(reopened) = crate::device::open_protected(&device_path, &mut volume_locks)
                {
                    if reopened.sector_size() == sector_size
                        && reopened.total_sectors().ok() == Some(total_sectors)
                    {
                        drive = reopened;
                    }
                }
                if !drive.can_reuse() {
                    crate::reset::poll_pending();
                    interruptible_sleep(Duration::from_secs(1));
                    continue;
                }
            }
            if !crate::device::has_capacity(sector_size.bytes()) {
                if !discovery_reset {
                    let previous = std::mem::replace(&mut drive, PhysicalDrive::closed());
                    drop(previous);
                    volume_locks.release();
                    if let Some(path) = crate::reset::bootstrap(
                        reset_instance.as_deref(),
                        total_sectors,
                        sector_size,
                    ) {
                        device_path = path;
                    }
                    discovery_reset = true;
                }
                crate::reset::poll_pending();
                interruptible_sleep(Duration::from_secs(1));
                continue;
            }
            lba %= total_sectors.min(10_000);
            check_io_abort()?;
            if stopping() {
                info!(
                    "{}",
                    message!(
                        "Stopped before creating the image.",
                        "イメージ作成前に中断しました。"
                    )
                );
                return Ok(());
            }
            read_buf.ensure_capacity(sector_size.bytes())?;
            let discovery =
                drive.read_sectors_timeout_for(lba, 1, &mut read_buf, timeout_ms, "discovery");
            if discovery == ReadResult::Success {
                info!(
                    "{}",
                    message!(
                        "Saved readable reference sector: LBA {}",
                        "読み取り可能な基準セクタを保存しました: LBA {}",
                        lba
                    )
                );
                found = Some((lba, read_buf.as_slice(sector_size.bytes()).to_vec()));
                break;
            }
            lba += 1;
        }
        if stopping() {
            info!(
                "{}",
                message!(
                    "Stopped before creating the image.",
                    "イメージ作成前に中断しました。"
                )
            );
            return Ok(());
        }
        check_io_abort()?;
        let (good_lba, good_data) = found.ok_or_else(|| {
            io::Error::other(tr!(
                "No readable reference sector found within the first 10,000 sectors.",
                "先頭10,000セクタ以内に読み取り可能な基準セクタがありません。"
            ))
        })?;
        let new_map = RescueMap::new_with_sector(good_lba, good_data, total_sectors, sector_size);
        new_map.save(&map_path)?;
        new_map
    };

    // 出力ファイルを開き、フルサイズを確保（未到達部分は自動的に0x00埋めになる）
    let mut out_file = crate::image::open(output_path, total_bytes, map.stats().0 > 0)?;

    let mut notified = false;
    let mut needs_reference = false;
    let mut last_full_check: Option<Instant> = None;
    let mut last_save = Instant::now();
    print_progress(&map)?;
    crate::visual::map(&map, true);

    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        ^ std::process::id() as u64;
    let mut scheduler = Scheduler::new(args.direction, seed);
    let mut lanes = crate::read_lanes::ReadLanes::new(
        sector_size,
        args.max_read_size
            .min(crate::device::PENDING_MEMORY_LIMIT - 65536),
        timeout_ms,
    );

    info!(
        "{}",
        message!(
            "Logical sector: {} bytes / Adaptive read cap: {}",
            "論理セクタ: {} バイト / 読取サイズ上限: {}",
            sector_size.bytes(),
            crate::output::format_bytes(args.max_read_size as u128)
        )
    );
    let mut fine_announced = false;
    info!(
        "{}",
        message!(
            "Read order: {} / Initial coarse read: {} (adaptive)",
            "読取順序: {} / 大ブロック読取の初期値: {}（自動調整）",
            args.direction.label(),
            format_size((INITIAL_BYTES / sector_size.bytes()) as u64, sector_size)
        )
    );
    let scan_result = (|| -> io::Result<()> {
        loop {
            if stopping() {
                break;
            }
            crate::output::check_log()?;
            crate::reset::poll_pending();
            if crate::reset::take_failed() {
                notified = false;
            }
            for completed in crate::device::take_completed() {
                if completed.purpose.ends_with("reference") {
                    // Confirmation samples do not overwrite already rescued data.
                    map.reference_completed(completed.lba, &completed.data, completed.elapsed)?;
                    continue;
                }
                let sectors = (completed.data.len() / sector_size.bytes()) as u64;
                if completed.lba + sectors > total_sectors {
                    return Err(io::Error::other("Completed read exceeds disk size"));
                }
                out_file.seek(SeekFrom::Start(completed.lba * sector_size.bytes() as u64))?;
                out_file.write_all(&completed.data)?;
                if completed.purpose == "scan" {
                    map.remember_timed_success(completed.lba, &completed.data, completed.elapsed);
                } else {
                    map.reference_success(completed.lba, &completed.data, completed.elapsed);
                }
                map.update_range(completed.lba, sectors, SectorStatus::Rescued);
                info!(
                    "[late read saved] LBA={} sectors={} continuation=none",
                    completed.lba, sectors
                );
                if completed.purpose == "scan" {
                    notified = false;
                    crate::reset::reads_restored();
                }
            }
            crate::visual::probing(crate::device::pending_ranges());
            if !crate::device::has_capacity(sector_size.bytes()) {
                if last_save.elapsed() >= Duration::from_secs(3) {
                    crate::image::checkpoint(&out_file, &map, &map_path)?;
                    last_save = Instant::now();
                }
                if !notified
                    && last_full_check
                        .is_none_or(|last| last.elapsed() >= crate::watchdog::operation_timeout())
                {
                    info!(
                        "[scan pool full] pending={} limit={} checking reference",
                        crate::device::pending_reads(),
                        args.max_parallel_reads
                    );
                    last_full_check = Some(Instant::now());
                    if !crate::probes::healthy(&drive, &map, timeout_ms)? {
                        crate::image::checkpoint(&out_file, &map, &map_path)?;
                        if let Some(path) = reset_source(
                            &mut drive,
                            &mut volume_locks,
                            reset_instance.as_deref(),
                            total_sectors,
                            &map,
                        ) {
                            device_path = path;
                        }
                        needs_reference = true;
                        notified = true;
                    }
                }
                interruptible_sleep(Duration::from_millis(200));
                continue;
            }
            last_full_check = None;
            if needs_reference {
                match try_reconnect(
                    &args.drive,
                    &device_path,
                    &drive,
                    &map,
                    &mut read_buf,
                    timeout_ms,
                    &mut volume_locks,
                )? {
                    Reconnection::Ready(reopened, path) => {
                        drive = reopened;
                        device_path = path;
                        needs_reference = false;
                    }
                    Reconnection::Retained => needs_reference = false,
                    _ => {
                        if !notified {
                            crate::image::checkpoint(&out_file, &map, &map_path)?;
                            if let Some(path) = reset_source(
                                &mut drive,
                                &mut volume_locks,
                                reset_instance.as_deref(),
                                total_sectors,
                                &map,
                            ) {
                                device_path = path;
                            }
                            notified = true;
                        }
                        interruptible_sleep(Duration::from_secs(1));
                        continue;
                    }
                }
            }
            lanes.current.load(crate::device::pending_reads() > 0);
            let coarse =
                scheduler.next(&map, SectorStatus::Untried, lanes.current.sizing.sectors());
            let task = coarse.or_else(|| scheduler.next(&map, SectorStatus::Deferred, 1));
            let Some((start, len)) = task else {
                if !map
                    .extents
                    .iter()
                    .any(|e| e.status == SectorStatus::Pending)
                {
                    break;
                }
                crate::image::checkpoint(&out_file, &map, &map_path)?;
                if !notified && !crate::probes::healthy(&drive, &map, timeout_ms)? {
                    if let Some(path) = reset_source(
                        &mut drive,
                        &mut volume_locks,
                        reset_instance.as_deref(),
                        total_sectors,
                        &map,
                    ) {
                        device_path = path;
                    } else if !stopping() {
                        warn!("{}", tr!("Some sectors are awaiting retry. Reconnecting the same card may help. Retrying automatically.",
                        "再試行待ちのセクタがあります。同じカードを抜き差しすると改善する場合があります。自動で再試行します。"));
                    }
                    notified = true;
                }
                crate::visual::map(&map, true);
                crate::visual::reconnecting();
                interruptible_sleep(Duration::from_secs(1));
                match try_reconnect(
                    &args.drive,
                    &device_path,
                    &drive,
                    &map,
                    &mut read_buf,
                    timeout_ms,
                    &mut volume_locks,
                )? {
                    Reconnection::Ready(reopened, path) => {
                        drive = reopened;
                        device_path = path;
                        map.retry_pending();
                    }
                    Reconnection::Retained => {
                        map.retry_pending();
                    }
                    Reconnection::Stalled => {}
                    Reconnection::Waiting => {}
                }
                continue;
            };
            if coarse.is_none() && !fine_announced {
                read_buf.trim_capacity(INITIAL_BYTES)?;
                info!(
                    "{}",
                    message!(
                        "\nCoarse scan finished. Starting {}-byte sector reads.",
                        "\n大ブロック走査が終わりました。{}バイト単位の走査を開始します。",
                        sector_size.bytes()
                    )
                );
                fine_announced = true;
                crate::image::checkpoint(&out_file, &map, &map_path)?;
            }
            let (start, len) = if coarse.is_some() {
                lanes
                    .current
                    .sizing
                    .prepare(start, len, scheduler.reading_direction())
            } else {
                (start, len)
            };
            if crate::device::overlaps_pending(start, len) {
                map.update_range(start, len, SectorStatus::Pending);
                scheduler.completed(start, len, false);
                continue;
            }
            read_buf.ensure_capacity(len as usize * sector_size.bytes())?;
            let bytes = len as usize * sector_size.bytes();
            let request_timeout_ms = lanes.current.timeout.for_bytes(bytes);
            info!(
                "[scan dispatch] lane={} LBA={} bytes={} timeout_ms={} pending={}",
                lanes.current.id,
                start,
                bytes,
                request_timeout_ms,
                crate::device::pending_reads()
            );
            let result = drive.read_sectors_timeout(start, len, &mut read_buf, request_timeout_ms);
            check_io_abort()?;
            if stopping() || result == ReadResult::Interrupted {
                break;
            }
            if result == ReadResult::Success {
                lanes.succeeded(start, len);
                lanes
                    .current
                    .timeout
                    .success(bytes, drive.last_read_elapsed());
                {
                    let image_seek = crate::watchdog::operation("seek image output");
                    out_file.seek(SeekFrom::Start(start * sector_size.bytes() as u64))?;
                    drop(image_seek);
                    let mut written = 0usize;
                    for chunk in read_buf
                        .as_slice(len as usize * sector_size.bytes())
                        .chunks(1024 * 1024)
                    {
                        if stopping() {
                            break;
                        }
                        let image_write = crate::watchdog::operation("write image chunk");
                        out_file.write_all(chunk)?;
                        drop(image_write);
                        let chunk_lba = start + (written / sector_size.bytes()) as u64;
                        map.remember_success(chunk_lba, chunk);
                        map.update_range(
                            chunk_lba,
                            (chunk.len() / sector_size.bytes()) as u64,
                            SectorStatus::Rescued,
                        );
                        written += chunk.len();
                        crate::visual::map(&map, false);
                    }
                    if written != len as usize * sector_size.bytes() {
                        break;
                    }
                    map.reference_success(map.good_lba, &map.good_data, drive.last_read_elapsed());
                }
                if coarse.is_some() {
                    if let Some(previous) =
                        lanes
                            .current
                            .sizing
                            .success(start, len, drive.last_read_elapsed())
                    {
                        if lanes.current.sizing.bytes() < previous {
                            read_buf.trim_capacity(lanes.current.sizing.bytes())?;
                        }
                        info!(
                            "{}",
                            message!(
                                "Read-size tuning: {} -> {} ({})",
                                "読取サイズ調整: {} → {}（{}）",
                                crate::output::format_bytes(previous as u128),
                                crate::output::format_bytes(lanes.current.sizing.bytes() as u128),
                                if lanes.current.sizing.settled() {
                                    tr!("efficiency plateau or cap", "効率の頭打ち・上限")
                                } else {
                                    tr!("measuring larger requests", "拡大して測定を継続")
                                }
                            )
                        );
                    }
                }
                scheduler.completed(start, len, true);
                notified = false;
                crate::reset::reads_restored();
            } else {
                if result == ReadResult::Busy {
                    interruptible_sleep(Duration::from_millis(100));
                    continue;
                }
                lanes.failed();
                read_buf.trim_capacity(INITIAL_BYTES)?;
                scheduler.completed(start, len, false);
                map.update_range(
                    start,
                    len,
                    if coarse.is_some() {
                        SectorStatus::Deferred
                    } else {
                        SectorStatus::Pending
                    },
                );
                let label = if result == ReadResult::Stalled {
                    tr!(
                        "Read kept pending; continue exploring until the scan limit is reached",
                        "未完了の読み取りを保持・走査上限までは別領域を探索"
                    )
                } else if result == ReadResult::Timeout {
                    tr!("Timeout", "タイムアウト")
                } else {
                    tr!("Error", "エラー")
                };
                failed!(
                    "{}",
                    message!(
                        "[Scan: {}] LBA {}..{} | {} ({})",
                        "[走査: {}] LBA {}..{} | {}（{}）",
                        label,
                        start,
                        start + len,
                        sector_count(len, sector_size),
                        if coarse.is_some() {
                            tr!("awaiting sector scan", "セクタ単位の走査待ち")
                        } else {
                            tr!("awaiting retry", "再試行待ち")
                        }
                    )
                );
                if result != ReadResult::Stalled {
                    interruptible_sleep(Duration::from_millis(100));
                }
            }
            crate::visual::map(&map, false);
            if last_save.elapsed() >= Duration::from_secs(3) {
                crate::image::checkpoint(&out_file, &map, &map_path)?;
                last_save = Instant::now();
            }
        }
        check_io_abort()?;
        Ok(())
    })();
    crate::image::checkpoint(&out_file, &map, &map_path)?;
    scan_result?;
    crate::visual::map(&map, true);
    if stopping() {
        info!(
            "{}",
            message!(
                "\nStopped. Saved the image and map. Use the same output path to resume.",
                "\n中断しました。イメージとマップを保存しました。同じ出力先で再開できます。"
            )
        );
        return Ok(());
    }
    print_progress(&map)?;
    crate::visual::map(&map, true);
    let (rescued, bad, _) = map.stats();
    success!(
        "{}",
        message!(
            "\nFinished: {} (recovered sectors: {}, legacy bad sectors: {})",
            "\n完了: {}（救出済: {}、旧NG: {}）",
            output_path,
            sector_count(rescued, sector_size),
            sector_count(bad, sector_size)
        )
    );
    Ok(())
}

fn reset_source(
    drive: &mut PhysicalDrive,
    locks: &mut volumes::VolumeLocks,
    instance: Option<&str>,
    total: u64,
    map: &RescueMap,
) -> Option<String> {
    if !crate::reset::can_attempt() {
        return None;
    }
    thread_local! { static PREVIOUS: std::cell::Cell<Option<(Instant, u64)>> = const { std::cell::Cell::new(None) }; }
    let rescued = map.stats().0;
    let now = Instant::now();
    let progress = PREVIOUS.with(|previous| previous.replace(Some((now, rescued))));
    if let Some((since, sectors)) = progress {
        info!(
            "[recovery progress] rescued_since_previous={} bytes={} elapsed_s={:.3}",
            rescued.saturating_sub(sectors),
            rescued
                .saturating_sub(sectors)
                .saturating_mul(map.sector_size.bytes() as u64),
            now.duration_since(since).as_secs_f64()
        );
    }
    warn!("{}", message!("Recovery requested: reference check unavailable; reference_LBA={} pending={} rescued_total={}", "復旧判断: 正常確認が成立しない状態。基準LBA={} 未完了={} 救出済合計={}", map.reference().lba, crate::device::pending_reads(), rescued));
    // Reads have completed before this point. Release every source handle before
    // PnP stop/start; leave only an invalid placeholder until reconnect succeeds.
    let previous = std::mem::replace(drive, PhysicalDrive::closed());
    drop(previous);
    locks.release();
    crate::reset::attempt(instance, total, map)
}

fn check_io_abort() -> io::Result<()> {
    if crate::device::io_aborted() {
        Err(io::Error::other(tr!(
            "Unacknowledged reads remain after recovery; stopped without issuing further reads",
            "復旧後も完了通知のない読み取りが残るため、これ以上の読み取りを行わず停止しました"
        )))
    } else {
        Ok(())
    }
}
