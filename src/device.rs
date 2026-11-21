use crate::{
    control::{stopping, STOP_REQUESTED},
    rescue_map::RescueMap,
    sector::SectorSize,
    volumes,
    windows_native::auto_device_path,
};
use std::cell::{Cell, RefCell};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::{io, slice};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GENERIC_READ, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{IOCTL_STORAGE_GET_DEVICE_NUMBER, STORAGE_DEVICE_NUMBER};
use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0,
};

#[derive(PartialEq, Eq, Clone, Copy)]
pub(crate) enum ReadResult {
    Interrupted,
    Success,
    Timeout,
    DeviceError,
    Stalled,
    Busy,
}

/// FILE_FLAG_NO_BUFFERING 用のページアライメント済みバッファ
pub(crate) struct AlignedBuffer {
    ptr: *mut u8,
    capacity: usize,
}

impl AlignedBuffer {
    pub(crate) fn new(capacity: usize) -> io::Result<Self> {
        let _operation = crate::watchdog::operation("allocate read buffer");
        let ptr = unsafe { VirtualAlloc(None, capacity, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        if ptr.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: ptr as *mut u8,
            capacity,
        })
    }

    pub(crate) fn ensure_capacity(&mut self, capacity: usize) -> io::Result<()> {
        let _operation = crate::watchdog::operation("resize read buffer");
        if capacity > self.capacity {
            *self = Self::new(capacity)?;
        }
        Ok(())
    }
    /// Caller must already have consumed or rejected the completed read.
    pub(crate) fn trim_capacity(&mut self, capacity: usize) -> io::Result<()> {
        let _operation = crate::watchdog::operation("trim read buffer");
        if self.capacity > capacity {
            *self = Self::new(capacity)?;
        }
        Ok(())
    }
    pub(crate) fn as_slice(&self, len: usize) -> &[u8] {
        assert!(len <= self.capacity);
        unsafe { slice::from_raw_parts(self.ptr, len) }
    }
}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        // 中断時の未完了I/Oの資源はプロセス終了まで保持する。
        if stopping() || self.ptr.is_null() {
            return;
        }
        unsafe {
            let _ = VirtualFree(self.ptr as *mut _, 0, MEM_RELEASE);
        }
    }
}

static PARALLEL_LIMIT: AtomicUsize = AtomicUsize::new(16);
pub(crate) const PENDING_MEMORY_LIMIT: usize = 2 * 1024 * 1024 * 1024;
pub(crate) fn configure_parallel_limit(limit: usize) {
    PARALLEL_LIMIT.store(limit, Ordering::Relaxed);
}
fn pending_count() -> usize {
    reap_retired();
    RETIRED.with(|reads| reads.borrow().len())
}
pub(crate) fn has_capacity(bytes: usize) -> bool {
    capacity(bytes, false)
}
pub(crate) fn has_reference_capacity(bytes: usize) -> bool {
    capacity(bytes, true)
}
fn capacity(bytes: usize, reference: bool) -> bool {
    reap_retired();
    RETIRED.with(|reads| {
        let reads = reads.borrow();
        let scan_count = reads
            .iter()
            .filter(|read| !read.trace.purpose.ends_with("reference"))
            .count();
        let reference_count = reads.len() - scan_count;
        let slots = if reference {
            reference_count == 0
        } else {
            scan_count < PARALLEL_LIMIT.load(Ordering::Relaxed)
        };
        let memory_limit = if reference {
            PENDING_MEMORY_LIMIT
        } else {
            PENDING_MEMORY_LIMIT - 65536
        };
        slots
            && reads
                .iter()
                .map(|read| read.allocation)
                .sum::<usize>()
                .saturating_add(bytes)
                <= memory_limit
    })
}
fn duplicate_read_handle(handle: HANDLE) -> io::Result<volumes::OwnedHandle> {
    use windows::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut result = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &mut result,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    }
    .map_err(volumes::win_error)?;
    Ok(volumes::OwnedHandle(result))
}

pub(crate) fn pending_reads() -> usize {
    pending_count()
}
pub(crate) struct CompletedRead {
    generation: usize,
    pub(crate) lba: u64,
    pub(crate) data: Vec<u8>,
    pub(crate) elapsed: Duration,
    pub(crate) purpose: &'static str,
}
thread_local! { static READ_PURPOSE: Cell<&'static str> = const { Cell::new("scan") }; }
#[derive(Clone, Copy)]
struct ReadTrace {
    id: usize,
    purpose: &'static str,
    timeout_ms: u32,
}
impl ReadTrace {
    fn new(lba: u64, bytes: usize, timeout_ms: u32) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        let trace = Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            purpose: READ_PURPOSE.with(Cell::get),
            timeout_ms,
        };
        crate::logging::append(&format!(
            "[I/O start] id={} purpose={} LBA={} bytes={} timeout_ms={} pending={}",
            trace.id,
            trace.purpose,
            lba,
            bytes,
            timeout_ms,
            pending_count()
        ));
        trace
    }
    fn event(self, status: &str, elapsed: Duration, pending: usize) {
        crate::logging::append(&format!(
            "[I/O {status}] id={} purpose={} elapsed_ms={:.3} timeout_ms={} pending={pending}",
            self.id,
            self.purpose,
            elapsed.as_secs_f64() * 1000.0,
            self.timeout_ms
        ));
    }
}
pub(crate) fn take_completed() -> Vec<CompletedRead> {
    reap_retired();
    COMPLETED.with(|reads| std::mem::take(&mut *reads.borrow_mut()))
}
pub(crate) fn overlaps_pending(lba: u64, sectors: u64) -> bool {
    RETIRED.with(|reads| {
        reads
            .borrow()
            .iter()
            .any(|r| lba < r.lba + (r.bytes / r.sector.bytes()) as u64 && r.lba < lba + sectors)
    })
}
pub(crate) fn pending_elapsed(lba: u64) -> Option<Duration> {
    RETIRED.with(|reads| {
        reads
            .borrow()
            .iter()
            .find(|read| read.lba == lba)
            .map(|read| read.started.elapsed())
    })
}
pub(crate) fn completed_reference_elapsed(
    generation: usize,
    lba: u64,
    expected: &[u8],
) -> io::Result<Option<Duration>> {
    reap_retired();
    COMPLETED.with(|reads| {
        if let Some(read) = reads
            .borrow()
            .iter()
            .find(|r| r.generation == generation && r.lba == lba && r.data.len() == expected.len())
        {
            if read.data != expected {
                return Err(io::Error::other(tr!(
                    "Reference sector mismatch",
                    "基準セクタが一致しません"
                )));
            }
            return Ok(Some(read.elapsed));
        }
        Ok(None)
    })
}
#[cfg(test)]
fn completed_reference(generation: usize, lba: u64, expected: &[u8]) -> io::Result<bool> {
    Ok(completed_reference_elapsed(generation, lba, expected)?.is_some())
}
pub(crate) fn pending_ranges() -> Vec<u64> {
    RETIRED.with(|reads| reads.borrow().iter().map(|r| r.lba).collect())
}
struct RetiredRead {
    trace: ReadTrace,
    generation: usize,
    lba: u64,
    bytes: usize,
    allocation: usize,
    sector: SectorSize,
    started: Instant,
    handle: HANDLE,
    event: HANDLE,
    overlapped: Option<Box<OVERLAPPED>>,
    buffer: *mut u8,
}
impl RetiredRead {
    fn completed(&self) -> bool {
        unsafe { WaitForSingleObject(self.event, 0) == WAIT_OBJECT_0 }
    }
}
impl Drop for RetiredRead {
    fn drop(&mut self) {
        if self.completed() {
            unsafe {
                let _ = CloseHandle(self.event);
                let _operation = crate::watchdog::operation("close source handle");
                let _ = CloseHandle(self.handle);
                let _ = VirtualFree(self.buffer.cast(), 0, MEM_RELEASE);
            }
        } else if let Some(overlapped) = self.overlapped.take() {
            // Process teardown is the final owner if the kernel never acknowledges cancellation.
            let _ = Box::into_raw(overlapped);
        }
    }
}
pub(crate) fn io_aborted() -> bool {
    false
}
thread_local! { static RETIRED: RefCell<Vec<RetiredRead>> = const { RefCell::new(Vec::new()) }; }
thread_local! { static COMPLETED: RefCell<Vec<CompletedRead>> = const { RefCell::new(Vec::new()) }; }
fn reap_retired() {
    RETIRED.with(|reads| {
        let mut remaining = reads.borrow().len();
        reads.borrow_mut().retain(|read| {
            if !read.completed() {
                return true;
            }
            let mut bytes = 0;
            let ok = unsafe {
                GetOverlappedResult(
                    read.handle,
                    read.overlapped.as_ref().unwrap().as_ref(),
                    &mut bytes,
                    false,
                )
            }
            .is_ok()
                && bytes as usize == read.bytes;
            let elapsed = read.started.elapsed();
            crate::visual::record_completion(elapsed, ok);
            remaining = remaining.saturating_sub(1);
            read.trace.event(
                if ok { "late-ok" } else { "late-error" },
                elapsed,
                remaining,
            );
            if ok {
                if read.trace.purpose == "scan" {
                    crate::visual::record_transfer(read.bytes, elapsed, true);
                }
                let data = unsafe { slice::from_raw_parts(read.buffer, read.bytes) }.to_vec();
                crate::output::read_success(
                    read.lba,
                    (read.bytes / read.sector.bytes()) as u64,
                    read.sector,
                );
                COMPLETED.with(|completed| {
                    completed.borrow_mut().push(CompletedRead {
                        generation: read.generation,
                        lba: read.lba,
                        data,
                        elapsed,
                        purpose: read.trace.purpose,
                    })
                });
            }
            false
        });
    });
}

/// 物理ドライブのラッパー
pub(crate) struct PhysicalDrive {
    generation: usize,
    handle: HANDLE,
    sector: SectorSize,
    elapsed: Cell<Duration>,
    total_bytes: u64,
}

impl PhysicalDrive {
    pub(crate) fn from_handle(handle: HANDLE, sector_size: SectorSize) -> Self {
        static NEXT_GENERATION: AtomicUsize = AtomicUsize::new(1);
        Self {
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            handle,
            sector: sector_size,
            elapsed: Cell::new(Duration::ZERO),
            total_bytes: 0,
        }
    }
    pub(crate) fn generation(&self) -> usize {
        self.generation
    }
    pub(crate) fn closed() -> Self {
        Self::from_handle(HANDLE::default(), SectorSize::LEGACY)
    }
    pub(crate) fn sector_size(&self) -> SectorSize {
        self.sector
    }
    pub(crate) fn last_read_elapsed(&self) -> Duration {
        self.elapsed.get()
    }

    pub(crate) fn can_reuse(&self) -> bool {
        !self.handle.is_invalid()
    }
    pub(crate) fn open(device_path: &str) -> io::Result<Self> {
        let (sector_size, bytes) = crate::geometry::inspect(device_path)?;
        let mut drive = Self::open_with_sector(device_path, sector_size)?;
        drive.total_bytes = bytes;
        Ok(drive)
    }
    fn open_with_sector(device_path: &str, sector_size: SectorSize) -> io::Result<Self> {
        let _operation = crate::watchdog::operation("open source disk");
        let wide: Vec<u16> = Path::new(device_path)
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(|error| {
            let error = volumes::win_error(error);
            io::Error::new(
                error.kind(),
                message!(
                    "CreateFileW(data read) failed for {device_path}: {error}",
                    "CreateFileW（データ読み取り）に失敗しました（対象: {device_path}）: {error}"
                ),
            )
        })?;

        Ok(Self::from_handle(handle, sector_size))
    }

    pub(crate) fn total_sectors(&self) -> io::Result<u64> {
        if self.total_bytes == 0 {
            return Err(io::Error::other(tr!(
                "Disk geometry is unavailable",
                "ディスク構成を取得できません"
            )));
        }
        Ok(self.total_bytes / self.sector.bytes() as u64)
    }

    /// 指定LBAから sector_count セクタ分をタイムアウト付きで読み込む
    pub(crate) fn read_sectors_timeout(
        &self,
        lba: u64,
        sector_count: u64,
        buf: &mut AlignedBuffer,
        timeout_ms: u32,
    ) -> ReadResult {
        self.read_with_cancel(
            lba,
            sector_count,
            buf,
            timeout_ms,
            |handle, overlapped| unsafe {
                let _ = CancelIoEx(handle, Some(overlapped));
            },
        )
    }

    pub(crate) fn read_sectors_timeout_for(
        &self,
        lba: u64,
        count: u64,
        buf: &mut AlignedBuffer,
        timeout_ms: u32,
        purpose: &'static str,
    ) -> ReadResult {
        struct Restore(&'static str);
        impl Drop for Restore {
            fn drop(&mut self) {
                READ_PURPOSE.with(|value| value.set(self.0));
            }
        }
        let _restore = Restore(READ_PURPOSE.with(|value| value.replace(purpose)));
        self.read_sectors_timeout(lba, count, buf, timeout_ms)
    }

    fn read_with_cancel(
        &self,
        lba: u64,
        sector_count: u64,
        buf: &mut AlignedBuffer,
        timeout_ms: u32,
        cancel: impl Fn(HANDLE, *const OVERLAPPED),
    ) -> ReadResult {
        if stopping() {
            return ReadResult::Interrupted;
        }
        if !capacity(
            buf.capacity,
            READ_PURPOSE.with(|value| value.get().ends_with("reference")),
        ) {
            return ReadResult::Busy;
        }
        let request_handle = match duplicate_read_handle(self.handle) {
            Ok(handle) => handle,
            Err(_) => return ReadResult::DeviceError,
        };
        let byte_len = (sector_count as usize) * self.sector.bytes();
        assert!(byte_len <= buf.capacity);
        let event = match unsafe { CreateEventW(None, true, false, None) } {
            Ok(ev) => ev,
            Err(_) => return ReadResult::DeviceError,
        };
        let offset = lba * self.sector.bytes() as u64;
        // 中断時にもカーネルが参照できるよう、OVERLAPPEDをヒープに置く。
        let mut ov = Box::new(OVERLAPPED {
            Anonymous: OVERLAPPED_0 {
                Anonymous: OVERLAPPED_0_0 {
                    Offset: offset as u32,
                    OffsetHigh: (offset >> 32) as u32,
                },
            },
            hEvent: event,
            ..Default::default()
        });
        let slice = unsafe { slice::from_raw_parts_mut(buf.ptr, byte_len) };
        crate::visual::begin_read(lba, sector_count, timeout_ms);
        let operation = crate::watchdog::timed(
            "source read / cancellation",
            Duration::from_millis(timeout_ms as u64) + crate::watchdog::operation_timeout(),
        );
        let trace = ReadTrace::new(lba, byte_len, timeout_ms);
        let io_started = Instant::now();
        let read = unsafe { ReadFile(request_handle.0, Some(slice), None, Some(&mut *ov)) };
        let pending = match read {
            Ok(_) => true,
            Err(error) => error.code() == windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0),
        };
        let result = if pending {
            let started = Instant::now();

            loop {
                if stopping() {
                    crate::visual::cancelling();
                    cancel(request_handle.0, &*ov);
                    // 完了待ちで停止を妨げない。イベント、OVERLAPPED、バッファ、
                    // ドライブは解放せず、マップ保存後のプロセス終了時にOSへ返す。
                    let _ = Box::into_raw(ov);
                    let _ = request_handle.into_raw();
                    crate::visual::end_read();
                    trace.event("interrupted", io_started.elapsed(), pending_count());
                    return ReadResult::Interrupted;
                }
                if started.elapsed() >= Duration::from_millis(timeout_ms as u64)
                    && unsafe { WaitForSingleObject(event, 0) } != WAIT_OBJECT_0
                {
                    let allocation = buf.capacity;
                    let ptr = std::mem::replace(&mut buf.ptr, std::ptr::null_mut());
                    buf.capacity = 0;
                    RETIRED.with(|retired| {
                        retired.borrow_mut().push(RetiredRead {
                            trace,
                            generation: self.generation,
                            lba,
                            bytes: byte_len,
                            allocation,
                            sector: self.sector,
                            started: io_started,
                            handle: request_handle.into_raw(),
                            event,
                            overlapped: Some(ov),
                            buffer: ptr,
                        })
                    });
                    reap_retired();
                    let elapsed = io_started.elapsed();
                    self.elapsed.set(elapsed);
                    trace.event("pending", elapsed, pending_count());
                    crate::visual::end_read();
                    crate::visual::probing(pending_ranges());
                    return ReadResult::Stalled;
                }
                let wait = unsafe { WaitForSingleObject(event, 50) };
                if wait == WAIT_OBJECT_0 {
                    let mut bytes_read = 0;
                    let completed = unsafe {
                        GetOverlappedResult(request_handle.0, &*ov, &mut bytes_read, false)
                    };
                    break if completed.is_ok() && bytes_read as usize == byte_len {
                        ReadResult::Success
                    } else {
                        ReadResult::DeviceError
                    };
                }
                if wait != WAIT_TIMEOUT {
                    // 未完了の可能性があるので終了を要求し、上の保持処理へ進む。
                    STOP_REQUESTED.store(true, Ordering::SeqCst);
                }
            }
        } else {
            ReadResult::DeviceError
        };

        unsafe {
            let _ = CloseHandle(event);
        }
        let elapsed = io_started.elapsed();
        self.elapsed.set(elapsed);
        trace.event(
            if result == ReadResult::Success {
                "ok"
            } else {
                "error"
            },
            elapsed,
            pending_count(),
        );
        drop(operation);
        crate::visual::read_finished(elapsed, result == ReadResult::Success);
        if result == ReadResult::Success {
            if trace.purpose == "scan" {
                crate::visual::record_transfer(byte_len, elapsed, false);
            }
            crate::output::read_success(lba, sector_count, self.sector);
        }
        result
    }
}

impl Drop for PhysicalDrive {
    fn drop(&mut self) {
        // 中断時の未完了I/Oの資源はプロセス終了まで保持する。
        if stopping() || self.handle.is_invalid() {
            return;
        }
        unsafe {
            let _operation = crate::watchdog::operation("close source handle");
            let _ = CloseHandle(self.handle);
        }
    }
}

/// ドライブレター（"E", "E:", "E:\"）または番号（"2"）から "\\.\PhysicalDriveX" を解決する
pub(crate) fn resolve_device_path(input: &str, verbose: bool) -> io::Result<String> {
    if input.trim().eq_ignore_ascii_case("auto") {
        return auto_device_path(verbose);
    }
    let trimmed = input.trim().trim_end_matches('\\').trim_end_matches(':');

    if let Ok(num) = trimmed.parse::<u32>() {
        return Ok(format!(r"\\.\PhysicalDrive{}", num));
    }

    let mut chars = trimmed.chars();
    if let (Some(letter), None) = (chars.next(), chars.next()) {
        if letter.is_ascii_alphabetic() {
            let letter_upper = letter.to_ascii_uppercase();
            let vol_path = format!(r"\\.\{}:", letter_upper);
            let _operation = crate::watchdog::operation("resolve drive letter");
            let wide: Vec<u16> = Path::new(&vol_path)
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    0x0000_0080, // FILE_READ_ATTRIBUTES
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    Default::default(),
                    None,
                )
            }
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    message!("Cannot open drive {}:. If it is unavailable or has a RAW filesystem, specify its physical disk number (e.g. -d 2).", "ドライブ {}: を開けません。存在しない場合やRAWの場合は物理ディスク番号（例: -d 2）を指定してください。",
                        letter_upper
                    ),
                )
            })?;

            let mut sdn = STORAGE_DEVICE_NUMBER::default();
            let mut bytes_returned = 0u32;
            let res = unsafe {
                DeviceIoControl(
                    handle,
                    IOCTL_STORAGE_GET_DEVICE_NUMBER,
                    None,
                    0,
                    Some(&mut sdn as *mut _ as *mut _),
                    std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                    Some(&mut bytes_returned),
                    None,
                )
            };
            unsafe {
                let _ = CloseHandle(handle);
            }

            res.map_err(|_| io::Error::last_os_error())?;
            if verbose {
                info!(
                    "{}",
                    message!(
                        "Resolved drive {}: to physical disk {}",
                        "ドライブ {}: を物理ディスク {} に解決しました",
                        letter_upper,
                        sdn.DeviceNumber
                    )
                );
            }
            return Ok(format!(r"\\.\PhysicalDrive{}", sdn.DeviceNumber));
        }
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        message!("Invalid drive selection: '{}' (use a drive letter such as E:, a disk number such as 2, or auto)", "ドライブ指定が不正です: '{}'（E:、2、autoなどを指定してください）", input),
    ))
}

fn open_then_protect<T>(
    open: impl FnOnce() -> io::Result<T>,
    protect: impl FnOnce() -> io::Result<()>,
) -> io::Result<T> {
    let source = open()?;
    protect()?;
    Ok(source)
}

pub(crate) fn open_protected(
    path: &str,
    locks: &mut volumes::VolumeLocks,
) -> io::Result<PhysicalDrive> {
    // A fresh handle must be opened before volume locks can deny new opens.
    // No ReadFile is issued until every volume has been protected.
    locks.release();
    let number = volumes::disk_number(path)?;
    open_then_protect(|| PhysicalDrive::open(path), || locks.ensure(number))
}

pub(crate) enum Reconnection {
    Ready(PhysicalDrive, String),
    Retained,
    Waiting,
    Stalled,
}

/// 新しいハンドルで容量と基準セクタを照合する。入力待ちは行わない。
pub(crate) fn try_reconnect(
    raw_drive_arg: &str,
    fallback_device_path: &str,
    existing: &PhysicalDrive,
    map: &RescueMap,
    buf: &mut AlignedBuffer,
    timeout_ms: u32,
    volume_locks: &mut volumes::VolumeLocks,
) -> io::Result<Reconnection> {
    if stopping() {
        return Ok(Reconnection::Waiting);
    }
    crate::visual::reconnecting();
    let total_sectors = map
        .extents
        .last()
        .map_or(0, |extent| extent.start + extent.len);
    buf.ensure_capacity(map.sector_size.bytes())?;
    if existing.can_reuse()
        && existing.sector_size() == map.sector_size
        && existing.total_sectors().ok() == Some(total_sectors)
    {
        if let Err(error) = volumes::disk_number(fallback_device_path)
            .and_then(|number| volume_locks.ensure(number))
        {
            warn!(
                "{}",
                message!(
                    "Volume protection failed; waiting to retry: {error}",
                    "ボリュームの保護に失敗しました。再試行を待ちます: {error}"
                )
            );
            return Ok(Reconnection::Waiting);
        }
        if crate::probes::recovery_matches(existing, map, timeout_ms)? {
            return Ok(Reconnection::Retained);
        }
        if overlaps_pending(map.reference().lba, 1) {
            return Ok(Reconnection::Stalled);
        }
        if stopping() {
            return Ok(Reconnection::Waiting);
        }
    }
    volume_locks.release();
    // Once resolved, use the physical path even while its volumes are locked.
    // Fall back to the original selector only if that path is no longer usable.
    let (candidate, reopened) = match PhysicalDrive::open(fallback_device_path) {
        Ok(drive) if drive.total_sectors().ok() == Some(total_sectors) => {
            (fallback_device_path.to_owned(), drive)
        }
        _ => {
            let path = match resolve_device_path(raw_drive_arg, false) {
                Ok(path) => path,
                Err(_) => return Ok(Reconnection::Waiting),
            };
            let drive = match PhysicalDrive::open(&path) {
                Ok(drive) => drive,
                Err(_) => return Ok(Reconnection::Waiting),
            };
            (path, drive)
        }
    };
    if reopened.sector_size() != map.sector_size
        || reopened.total_sectors().ok() != Some(total_sectors)
    {
        return Ok(Reconnection::Waiting);
    }
    if let Err(error) =
        volumes::disk_number(&candidate).and_then(|number| volume_locks.ensure(number))
    {
        warn!(
            "{}",
            message!(
                "Volume protection failed; waiting to retry: {error}",
                "ボリュームの保護に失敗しました。再試行を待ちます: {error}"
            )
        );
        return Ok(Reconnection::Waiting);
    }
    if stopping() {
        return Ok(Reconnection::Waiting);
    }
    buf.ensure_capacity(map.sector_size.bytes())?;
    if crate::probes::recovery_matches(&reopened, map, timeout_ms)? {
        Ok(Reconnection::Ready(reopened, candidate))
    } else if overlaps_pending(map.reference().lba, 1) {
        Ok(Reconnection::Stalled)
    } else {
        Ok(Reconnection::Waiting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{console_handler, interruptible_sleep};
    use crate::{CHUNK_BYTES, SECTOR_SIZE};
    use std::fs;
    use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    static IO_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    #[test]
    fn source_is_opened_before_locking_and_lock_failure_drops_it_without_reading() {
        let events = RefCell::new(Vec::new());
        struct Source<'a>(&'a RefCell<Vec<&'static str>>);
        impl Drop for Source<'_> {
            fn drop(&mut self) {
                self.0.borrow_mut().push("close");
            }
        }
        let result = open_then_protect(
            || {
                events.borrow_mut().push("open");
                Ok(Source(&events))
            },
            || {
                events.borrow_mut().push("lock");
                Err(io::Error::other("volume busy"))
            },
        );
        assert!(result.is_err());
        assert_eq!(*events.borrow(), ["open", "lock", "close"]);
        events.borrow_mut().clear();
        let result: io::Result<()> = open_then_protect(
            || {
                events.borrow_mut().push("open");
                Err(io::Error::other("denied"))
            },
            || {
                events.borrow_mut().push("lock");
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(*events.borrow(), ["open"]);
    }
    #[test]
    fn pending_read_keeps_memory_until_completion_without_requesting_cancel() -> io::Result<()> {
        use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_FREE};
        let _guard = IO_TEST_LOCK.lock().unwrap();
        let (drive, server) = pipe_pair("ignored-cancellation")?;
        let mut buffer = AlignedBuffer::new(4096)?;
        let ptr = buffer.ptr;
        let started = Instant::now();
        // Scheduling delay must not cancel a real pending request.
        let result = drive.read_with_cancel(0, 1, &mut buffer, 1, |_, _| {
            panic!("Scheduling threshold must not cancel")
        });
        assert!(result == ReadResult::Stalled);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(buffer.capacity, 0);
        assert!(buffer.ptr.is_null());
        let mut info = MEMORY_BASIC_INFORMATION::default();
        unsafe {
            VirtualQuery(
                Some(ptr.cast()),
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            );
        }
        assert_eq!(info.State, MEM_COMMIT);
        drop(server); // Device removal/reset is modeled by disconnecting the pipe.
        let until = Instant::now() + Duration::from_secs(1);
        while RETIRED.with(|reads| reads.borrow().iter().any(|read| !read.completed()))
            && Instant::now() < until
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        reap_retired();
        assert!(RETIRED.with(|reads| reads.borrow().is_empty()));
        unsafe {
            VirtualQuery(
                Some(ptr.cast()),
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            );
        }
        assert_eq!(info.State, MEM_FREE);
        Ok(())
    }
    #[test]
    fn four_k_reads_use_four_k_offsets_and_grow_buffers_on_demand() -> io::Result<()> {
        let _guard = IO_TEST_LOCK.lock().unwrap();
        let path = std::env::temp_dir().join(format!("sd2img-4k-{}.bin", std::process::id()));
        let mut data = vec![0x12; 4096];
        data.extend(vec![0x34; 4096]);
        fs::write(&path, &data)?;
        let result = (|| -> io::Result<()> {
            let drive =
                PhysicalDrive::open_with_sector(path.to_str().unwrap(), SectorSize::new(4096)?)?;
            let mut buffer = AlignedBuffer::new(4096)?;
            assert!(drive.read_sectors_timeout(1, 1, &mut buffer, 2000) == ReadResult::Success);
            assert_eq!(buffer.as_slice(4096), &data[4096..]);
            buffer.ensure_capacity(8192)?;
            assert!(drive.read_sectors_timeout(0, 2, &mut buffer, 2000) == ReadResult::Success);
            assert_eq!(buffer.as_slice(8192), data);
            Ok(())
        })();
        fs::remove_file(path)?;
        result
    }

    #[test]
    fn delayed_success_is_collected_and_pending_reads_obey_the_limit() -> io::Result<()> {
        use windows::Win32::Storage::FileSystem::WriteFile;
        let _guard = IO_TEST_LOCK.lock().unwrap();
        configure_parallel_limit(2);
        let (first, server1) = pipe_pair("delayed-first")?;
        let (second, server2) = pipe_pair("delayed-second")?;
        let mut buffer = AlignedBuffer::new(512)?;
        assert!(
            first.read_with_cancel(7, 1, &mut buffer, 1, |_, _| panic!("unexpected cancel"))
                == ReadResult::Stalled
        );
        buffer.ensure_capacity(512)?;
        assert!(
            second.read_with_cancel(9, 1, &mut buffer, 1, |_, _| panic!("unexpected cancel"))
                == ReadResult::Stalled
        );
        buffer.ensure_capacity(512)?;
        assert!(first.read_sectors_timeout(11, 1, &mut buffer, 1) == ReadResult::Busy);
        assert_eq!(pending_reads(), 2);
        // A full scan pool leaves exactly one independent reference slot.
        let (probe, server3) = pipe_pair("reserved-reference")?;
        assert!(has_reference_capacity(512));
        assert!(
            probe.read_sectors_timeout_for(0, 1, &mut buffer, 1, "health-reference")
                == ReadResult::Stalled
        );
        assert_eq!(pending_reads(), 3);
        assert!(!has_capacity(512));
        assert!(!has_reference_capacity(512));
        buffer.ensure_capacity(512)?;
        assert!(
            probe.read_sectors_timeout_for(1, 1, &mut buffer, 1, "health-reference")
                == ReadResult::Busy
        );
        drop(server3);
        for _ in 0..100 {
            if pending_reads() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(pending_reads(), 2);
        let data = vec![0x5a; 512];
        unsafe { WriteFile(server1.0, Some(&data), None, None) }.map_err(volumes::win_error)?;
        for _ in 0..100 {
            if pending_reads() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(completed_reference(first.generation(), 7, &data)?);
        assert!(!completed_reference(second.generation(), 7, &data)?);
        assert!(completed_reference(first.generation(), 7, &vec![0; 512]).is_err());
        let completed = take_completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].lba, 7);
        assert_eq!(completed[0].data, data);
        assert!(has_capacity(512));
        assert!(!has_capacity(PENDING_MEMORY_LIMIT));
        drop(server2);
        for _ in 0..100 {
            if pending_reads() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(pending_reads(), 0);
        configure_parallel_limit(16);
        Ok(())
    }

    #[test]
    fn health_verification_requires_the_saved_reference_bytes() -> io::Result<()> {
        let _guard = IO_TEST_LOCK.lock().unwrap();
        let path = std::env::temp_dir().join(format!("sd2img-health-{}.bin", std::process::id()));
        let data = vec![0x35; 4096];
        fs::write(&path, &data)?;
        let result = (|| -> io::Result<()> {
            let sector = SectorSize::new(4096)?;
            let drive = PhysicalDrive::open_with_sector(path.to_str().unwrap(), sector)?;
            let map = RescueMap::new_with_sector(0, data, 1, sector);
            assert!(crate::probes::reference_matches(&drive, &map)?);
            // A previous success must not satisfy a fresh health check.
            fs::write(&path, vec![0; 4096])?;
            assert!(crate::probes::healthy(&drive, &map, 100).is_err());
            assert!(crate::probes::reference_matches(&drive, &map).is_err());
            take_completed();
            Ok(())
        })();
        fs::remove_file(path)?;
        result
    }

    #[test]
    fn retired_buffers_are_retained_until_completion_is_signalled() -> io::Result<()> {
        use windows::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_FREE};
        use windows::Win32::System::Threading::SetEvent;
        let _guard = IO_TEST_LOCK.lock().unwrap();
        let buffer = AlignedBuffer::new(4096)?;
        let ptr = buffer.ptr;
        std::mem::forget(buffer);
        let event = unsafe { CreateEventW(None, true, false, None) }.map_err(volumes::win_error)?;
        let handle =
            unsafe { CreateEventW(None, true, false, None) }.map_err(volumes::win_error)?;
        let read = RetiredRead {
            trace: ReadTrace {
                id: 0,
                purpose: "test",
                timeout_ms: 0,
            },
            generation: 0,
            lba: 0,
            bytes: 4096,
            allocation: 4096,
            sector: SectorSize::new(512)?,
            started: Instant::now(),
            handle,
            event,
            overlapped: Some(Box::default()),
            buffer: ptr,
        };
        assert!(!read.completed());
        let mut info = MEMORY_BASIC_INFORMATION::default();
        unsafe {
            VirtualQuery(
                Some(ptr.cast()),
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            );
        }
        assert_eq!(info.State, MEM_COMMIT);
        unsafe { SetEvent(event) }.map_err(volumes::win_error)?;
        assert!(read.completed());
        drop(read);
        unsafe {
            VirtualQuery(
                Some(ptr.cast()),
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            );
        }
        assert_eq!(info.State, MEM_FREE);
        Ok(())
    }

    fn pipe_pair(suffix: &str) -> io::Result<(PhysicalDrive, crate::volumes::OwnedHandle)> {
        use windows::Win32::Storage::FileSystem::PIPE_ACCESS_OUTBOUND;
        use windows::Win32::System::Pipes::{CreateNamedPipeW, PIPE_TYPE_BYTE, PIPE_WAIT};
        let name = crate::volumes::wide(&format!(
            r"\\.\pipe\sd2img-test-{}-{suffix}",
            std::process::id()
        ));
        let server = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_OUTBOUND,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                1,
                512,
                512,
                0,
                None,
            )
        };
        if server.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        let server = crate::volumes::OwnedHandle(server);
        let client = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(crate::volumes::win_error)?;
        Ok((
            PhysicalDrive::from_handle(client, SectorSize::LEGACY),
            server,
        ))
    }

    #[test]
    fn ctrl_c_interrupts_an_actual_pending_windows_read_without_freeing_its_buffer(
    ) -> io::Result<()> {
        let _guard = IO_TEST_LOCK.lock().unwrap();
        let (drive, server) = pipe_pair("pending")?;
        let mut buffer = AlignedBuffer::new(SECTOR_SIZE)?;
        let signal = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            unsafe {
                let _ = console_handler(CTRL_C_EVENT);
            }
        });
        let started = Instant::now();
        let result = drive.read_sectors_timeout(0, 1, &mut buffer, 5_000);
        signal.join().unwrap();
        // Interrupted I/O deliberately keeps its buffer, OVERLAPPED, event and
        // source handle alive until process exit. Preserve that contract here.
        std::mem::forget(buffer);
        std::mem::forget(drive);
        drop(server);
        STOP_REQUESTED.store(false, Ordering::SeqCst);
        assert!(result == ReadResult::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(2));
        Ok(())
    }

    #[test]
    fn disappearing_read_endpoint_is_reported_as_a_device_error() -> io::Result<()> {
        let _guard = IO_TEST_LOCK.lock().unwrap();
        let (drive, server) = pipe_pair("disconnect")?;
        drop(server);
        let mut buffer = AlignedBuffer::new(SECTOR_SIZE)?;
        assert!(drive.read_sectors_timeout(0, 1, &mut buffer, 1_000) == ReadResult::DeviceError);
        Ok(())
    }
    #[test]
    fn ctrl_c_stops_waits_and_prevents_new_reads() -> io::Result<()> {
        let _guard = IO_TEST_LOCK.lock().unwrap();
        let mut buf = AlignedBuffer::new(SECTOR_SIZE)?;
        let drive = PhysicalDrive::closed();
        assert_eq!(unsafe { console_handler(CTRL_C_EVENT) }.0, 1);
        // 2回目も既定の強制終了へ渡さず、保存処理を続けられる。
        assert_eq!(unsafe { console_handler(CTRL_BREAK_EVENT) }.0, 1);
        let start = Instant::now();
        interruptible_sleep(Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(drive.read_sectors_timeout(0, 1, &mut buf, 10_000) == ReadResult::Interrupted);
        // テストでは未発行なので通常解放できる。
        STOP_REQUESTED.store(false, Ordering::SeqCst);
        std::mem::forget(drive); // 無効なテスト用ハンドルをCloseHandleしない。
        Ok(())
    }
    #[test]
    fn overlapped_read_returns_actual_bytes_and_detects_short_read() -> io::Result<()> {
        let _guard = IO_TEST_LOCK.lock().unwrap();
        // 実カードに触れず、通常ファイルでWindowsの非同期読み取り経路を通す。
        let path = std::env::temp_dir().join(format!("sd2img-read-{}.bin", std::process::id()));
        let expected: Vec<u8> = (0..SECTOR_SIZE).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &expected)?;
        let result = (|| -> io::Result<()> {
            let drive =
                PhysicalDrive::open_with_sector(path.to_str().unwrap(), SectorSize::LEGACY)?;
            let mut buf = AlignedBuffer::new(CHUNK_BYTES)?;
            assert!(drive.read_sectors_timeout(0, 1, &mut buf, 2_000) == ReadResult::Success);
            assert_eq!(buf.as_slice(SECTOR_SIZE), expected);
            assert!(drive.read_sectors_timeout(1, 1, &mut buf, 2_000) == ReadResult::DeviceError);
            Ok(())
        })();
        fs::remove_file(path)?;
        result
    }
}
