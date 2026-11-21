//! Device-specific recovery. One hidden helper performs PnP APIs; only the scanner issues data reads.
use crate::{
    control::stopping,
    rescue_map::RescueMap,
    volumes::{self, wide, win_error},
    windows_native,
};
use std::{
    io::{self, Read, Write},
    mem::size_of,
    os::windows::process::CommandExt,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

const WORKER_FLAG: &str = "--sd2img-reset-worker";
#[derive(Default)]
struct Backoff {
    failures: u32,
    until: Option<Instant>,
}
impl Backoff {
    fn failed(&mut self, now: Instant) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let delay = Duration::from_secs(
            (10u64.saturating_mul(1u64 << self.failures.min(6).saturating_sub(1))).min(300),
        );
        self.until = Some(now + delay);
        delay
    }
    fn remaining(&self, now: Instant) -> Duration {
        self.until
            .map_or(Duration::ZERO, |until| until.saturating_duration_since(now))
    }
}
thread_local! {
    static BACKOFF: std::cell::RefCell<[Backoff; 2]> = std::cell::RefCell::new([Backoff::default(), Backoff::default()]);
    static RETRY_AT: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}
fn record_failure(method: Method) {
    let delay = BACKOFF.with(|states| states.borrow_mut()[method as usize].failed(Instant::now()));
    warn!(
        "{}",
        message!(
            "{} failed; retry cooldown {:.0}s",
            "{} が失敗。再試行の待ち時間 {:.0}秒",
            method.label(),
            delay.as_secs_f64()
        )
    );
    let at = Instant::now() + delay;
    RETRY_AT.with(|value| value.set(Some(value.get().map_or(at, |old| old.min(at)))));
}
struct PendingReset {
    child: ResetChild,
    output: std::thread::JoinHandle<io::Result<Vec<u8>>>,
    errors: std::thread::JoinHandle<io::Result<Vec<u8>>>,
    method: Method,
}
thread_local! { static PENDING: std::cell::RefCell<Option<PendingReset>> = const { std::cell::RefCell::new(None) }; }
thread_local! {
    static FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static NEXT_METHOD: std::cell::Cell<Option<Method>> = const { std::cell::Cell::new(None) };
}
pub(crate) fn take_failed() -> bool {
    let due = RETRY_AT.with(|value| {
        if value.get().is_some_and(|at| Instant::now() >= at) {
            value.set(None);
            true
        } else {
            false
        }
    });
    FAILED.with(|flag| flag.replace(false)) || due
}
pub(crate) fn reads_restored() {
    NEXT_METHOD.with(|next| next.set(None));
    FAILED.with(|flag| flag.set(false));
    BACKOFF.with(|states| {
        for state in states.borrow_mut().iter_mut() {
            if state.until.is_some_and(|until| {
                Instant::now().saturating_duration_since(until) >= Duration::from_secs(60)
            }) {
                *state = Backoff::default();
            }
        }
    });
}
fn is_pending() -> bool {
    PENDING.with(|pending| pending.borrow().is_some())
}
/// Do not release a usable source handle just to discover that all reset
/// methods are cooling down. Reference/reconnection checks continue normally.
pub(crate) fn can_attempt() -> bool {
    poll_pending();
    if is_pending() {
        return false;
    }
    let now = Instant::now();
    let remaining = BACKOFF.with(|states| {
        states
            .borrow()
            .iter()
            .map(|state| state.remaining(now))
            .min()
            .unwrap()
    });
    if remaining.is_zero() {
        return true;
    }
    RETRY_AT.with(|value| {
        value.set(Some(
            value
                .get()
                .map_or(now + remaining, |old| old.min(now + remaining)),
        ))
    });
    info!(
        "{}",
        message!(
            "Reset cooldown {:.1}s; keep the source handle and continue reference checks",
            "リセット再試行まで {:.1}秒。元ハンドルを保持して正常確認を継続",
            remaining.as_secs_f64()
        )
    );
    false
}
pub(crate) fn poll_pending() {
    let finished = PENDING.with(|pending| {
        pending
            .borrow_mut()
            .as_mut()
            .is_some_and(|job| job.child.try_wait().ok().flatten().is_some())
    });
    if !finished {
        return;
    }
    let mut job = PENDING.with(|pending| pending.borrow_mut().take().unwrap());
    let status = job.child.wait();
    let output = job
        .output
        .join()
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let errors = job
        .errors
        .join()
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    if let Ok(report) = decode_report(&output) {
        for line in report.logs {
            crate::logging::append_forwarded(&line);
        }
    }
    if !status.as_ref().is_ok_and(|s| s.success()) {
        record_failure(job.method);
        FAILED.with(|flag| flag.set(true));
        if job.method == Method::ConfigurationManager {
            NEXT_METHOD.with(|next| next.set(Some(Method::SetupApi)));
        }
    }
    info!(
        "{}",
        message!(
            "{} reset completion received: {} {}",
            "{} のリセット完了通知を受信: {} {}",
            job.method.label(),
            status.is_ok_and(|s| s.success()),
            String::from_utf8_lossy(&errors).trim()
        )
    );
}
pub(crate) fn drain_pending() {
    if is_pending() {
        warn!(
            "{}",
            tr!(
                "Image and map saved. Waiting for the outstanding reset notification.",
                "イメージとマップを保存しました。未完了リセットの通知を待っています。"
            )
        );
    }
    while is_pending() {
        poll_pending();
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct ResetChild(Option<Child>);
impl std::ops::Deref for ResetChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        self.0.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for ResetChild {
    fn deref_mut(&mut self) -> &mut Child {
        self.0.as_mut().unwrap()
    }
}
impl Drop for ResetChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    ConfigurationManager,
    SetupApi,
}

impl Method {
    fn argument(self) -> &'static str {
        match self {
            Self::ConfigurationManager => "cm",
            Self::SetupApi => "setup",
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ConfigurationManager => "Configuration Manager API",
            Self::SetupApi => "SetupAPI",
        }
    }
}

fn sequence(mut attempt: impl FnMut(Method) -> bool) -> bool {
    for method in [Method::ConfigurationManager, Method::SetupApi] {
        if attempt(method) {
            return true;
        }
    }
    false
}

/// Logs each API attempt and failure, including verification failures. No second
/// helper starts until the first has exited, so reset calls are never parallel.
struct Verification<'a> {
    total: u64,
    sector: crate::sector::SectorSize,
    reference: Option<(u64, &'a [u8])>,
    reference_elapsed: Option<Duration>,
}

pub(crate) fn attempt(instance: Option<&str>, total: u64, map: &RescueMap) -> Option<String> {
    let reference = map.reference();
    attempt_verified(
        instance,
        Verification {
            total,
            sector: map.sector_size,
            reference: Some((reference.lba, &reference.data)),
            reference_elapsed: reference.elapsed,
        },
    )
}
pub(crate) fn bootstrap(
    instance: Option<&str>,
    total: u64,
    sector: crate::sector::SectorSize,
) -> Option<String> {
    attempt_verified(
        instance,
        Verification {
            total,
            sector,
            reference: None,
            reference_elapsed: None,
        },
    )
}
fn attempt_verified(instance: Option<&str>, verification: Verification<'_>) -> Option<String> {
    poll_pending();
    if is_pending() {
        return instance.and_then(|id| windows_native::disk_for_instance(id).ok());
    }
    let mut restored = None;
    let next = NEXT_METHOD.with(|value| value.take());
    sequence(|method| {
        if next == Some(Method::SetupApi) && method == Method::ConfigurationManager {
            return false;
        }
        if stopping() {
            return false;
        }
        let remaining =
            BACKOFF.with(|states| states.borrow()[method as usize].remaining(Instant::now()));
        if !remaining.is_zero() {
            let at = Instant::now() + remaining;
            RETRY_AT.with(|value| value.set(Some(value.get().map_or(at, |old| old.min(at)))));
            info!(
                "{}",
                message!(
                    "{} cooling down for {:.1}s; continue reference checks",
                    "{} は再試行まで {:.1}秒。正常セクタの確認を継続",
                    method.label(),
                    remaining.as_secs_f64()
                )
            );
            return false;
        }
        warn!(
            "{}",
            message!(
                "Attempting reset via {}.",
                "{} でリセットを試みます。",
                method.label()
            )
        );
        crate::visual::resetting(method.label());
        let _ = crate::output::flush_log();
        let result = instance
            .ok_or_else(|| {
                io::Error::other(tr!(
            "Cannot safely identify the source device instance; no other device will be reset",
            "対象デバイスを安全に特定できないため、他のデバイスへのリセットは行いません"))
            })
            .and_then(|instance| run_worker(method, instance, &verification))
            .and_then(|path| {
                if is_pending() {
                    return Ok(path);
                }
                let mut locks = volumes::VolumeLocks::default();
                let drive = crate::device::open_protected(&path, &mut locks)?;
                if drive.sector_size() != verification.sector
                    || drive.total_sectors()? != verification.total
                {
                    return Err(io::Error::other(tr!(
                        "Device geometry mismatch after reset",
                        "リセット後のディスク構成が一致しません"
                    )));
                }
                if let Some((lba, data)) = verification.reference {
                    let map = RescueMap::new_with_sector(
                        lba,
                        data.to_vec(),
                        verification.total,
                        verification.sector,
                    );
                    if let Some(elapsed) = verification.reference_elapsed {
                        map.reference_success(lba, data, elapsed);
                    }
                    if !crate::probes::recovery_matches(&drive, &map, 1)? {
                        return Err(io::Error::other(tr!(
                            "Reference sector did not respond after reset",
                            "リセット後も基準セクタが応答しません"
                        )));
                    }
                }
                Ok(path)
            });
        if is_pending() {
            restored = result.ok();
            return true;
        }
        match result {
            Ok(path) => {
                BACKOFF.with(|states| states.borrow_mut()[method as usize] = Backoff::default());
                let detail = if verification.reference.is_some() {
                    tr!(
                        "capacity and reference sector verified",
                        "容量と基準セクタの一致を確認"
                    )
                } else {
                    tr!("device identity, capacity and sector size verified; no reference sector saved yet", "デバイス識別・容量・セクタサイズを確認（基準セクタは未保存）")
                };
                success!(
                    "{}",
                    message!(
                        "{} reset succeeded: {}.",
                        "{} のリセットが成功しました: {}。",
                        method.label(),
                        detail
                    )
                );
                restored = Some(path);
                true
            }
            Err(error) => {
                record_failure(method);
                failed!(
                    "{}",
                    message!(
                        "{} reset did not restore the device: {}",
                        "{} ではデバイスが復旧しませんでした: {}",
                        method.label(),
                        error
                    )
                );
                false
            }
        }
    });
    restored
}

fn run_worker(
    method: Method,
    instance: &str,
    verification: &Verification<'_>,
) -> io::Result<String> {
    let _operation = crate::watchdog::operation("reset helper / teardown");
    let mut child = ResetChild(Some(
        Command::new(std::env::current_exe()?)
            .args([WORKER_FLAG, method.argument(), instance])
            .creation_flags(CREATE_NO_WINDOW.0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    ));
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Missing helper output"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("Missing helper errors"))?;
    let output_reader = collect_pipe(stdout)?;
    let error_reader = collect_pipe(stderr)?;
    let mut input = child.stdin.take().ok_or_else(|| {
        io::Error::other(tr!(
            "Reset helper input unavailable",
            "リセット処理の入力を開けません"
        ))
    })?;
    // Reference bytes stay in a private pipe, never on the command line or in logs.
    let (lba, data) = verification.reference.unwrap_or((u64::MAX, &[]));
    let header = format!(
        "{} {} {}\n",
        verification.total,
        lba,
        verification.sector.bytes()
    );
    if let Err(error) = input
        .write_all(header.as_bytes())
        .and_then(|_| input.write_all(data))
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    drop(input);
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() >= crate::watchdog::operation_timeout() {
            warn!("{}", message!("{} reset is still pending. Keeping the request and checking for readable regions; no second reset will be issued.", "{} のリセット応答を待っています。要求を保持して読める領域を確認し、リセットは重ねません。", method.label()));
            PENDING.with(|pending| {
                *pending.borrow_mut() = Some(PendingReset {
                    child,
                    output: output_reader,
                    errors: error_reader,
                    method,
                })
            });
            return windows_native::disk_for_instance(instance);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let status = child.wait()?;
    let stdout = output_reader
        .join()
        .map_err(|_| io::Error::other("Helper output reader failed"))??;
    let stderr = error_reader
        .join()
        .map_err(|_| io::Error::other("Helper error reader failed"))??;
    let report = decode_report(&stdout)?;
    for stats in report.stats {
        crate::visual::merge_statistics(stats);
    }
    for line in report.logs {
        crate::logging::append_forwarded(&line);
    }
    if !status.success() {
        return Err(io::Error::other(
            String::from_utf8_lossy(&stderr).trim().to_owned(),
        ));
    }
    report.path.ok_or_else(|| {
        io::Error::other(tr!(
            "Reset helper did not return a device",
            "リセット処理からデバイスが返されませんでした"
        ))
    })
}

fn collect_pipe(
    reader: impl Read + Send + 'static,
) -> io::Result<std::thread::JoinHandle<io::Result<Vec<u8>>>> {
    std::thread::Builder::new()
        .name("sd2img-helper-output".to_owned())
        .spawn(move || {
            let mut bytes = Vec::new();
            reader.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 4 * 1024 * 1024 {
                return Err(io::Error::other("Reset helper output limit exceeded"));
            }
            Ok(bytes)
        })
}
struct WorkerReport {
    path: Option<String>,
    stats: Vec<crate::visual::ReadStatistics>,
    logs: Vec<String>,
}
fn decode_report(bytes: &[u8]) -> io::Result<WorkerReport> {
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    let mut report = WorkerReport {
        path: None,
        stats: Vec::new(),
        logs: Vec::new(),
    };
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("DEVICE ") {
            volumes::disk_number(path)?;
            report.path = Some(path.to_owned());
        } else if let Some(log) = line.strip_prefix("LOG ") {
            report.logs.push(log.to_owned());
        } else if let Some(stats) = line.strip_prefix("STATS ") {
            let fields: Vec<_> = stats.split_whitespace().collect();
            if fields.len() != 4 {
                return Err(io::Error::other("Invalid helper statistics"));
            }
            let sample = crate::visual::ReadStatistics {
                ok_count: fields[0].parse().map_err(io::Error::other)?,
                ok_seconds: fields[1].parse().map_err(io::Error::other)?,
                ng_count: fields[2].parse().map_err(io::Error::other)?,
                ng_seconds: fields[3].parse().map_err(io::Error::other)?,
            };
            if !sample.ok_seconds.is_finite()
                || !sample.ng_seconds.is_finite()
                || sample.ok_seconds < 0.0
                || sample.ng_seconds < 0.0
            {
                return Err(io::Error::other("Invalid helper timing"));
            }
            report.stats.push(sample);
        } else if !line.is_empty() {
            return Err(io::Error::other("Invalid reset helper response"));
        }
    }
    Ok(report)
}

fn cm_reset(instance: &str) -> io::Result<()> {
    let id = wide(instance);
    let mut devinst = 0;
    let locate =
        unsafe { CM_Locate_DevNodeW(&mut devinst, PCWSTR(id.as_ptr()), CM_LOCATE_DEVNODE_NORMAL) };
    if locate != CR_SUCCESS {
        return Err(io::Error::other(format!(
            "CM_Locate_DevNodeW: {}",
            locate.0
        )));
    }
    let mut parent = 0;
    let parent_result = unsafe { CM_Get_Parent(&mut parent, devinst, 0) };
    if parent_result != CR_SUCCESS {
        return Err(io::Error::other(format!(
            "CM_Get_Parent: {}",
            parent_result.0
        )));
    }
    let mut veto = PNP_VETO_TYPE::default();
    let mut veto_name = [0u16; 512];
    // Do not set CM_REMOVE_NO_RESTART or disable the device persistently:
    // cancellation must not leave the card administratively disabled.
    let remove = unsafe {
        CM_Query_And_Remove_SubTreeW(
            devinst,
            Some(&mut veto),
            Some(&mut veto_name),
            CM_REMOVE_UI_NOT_OK,
        )
    };
    if remove != CR_SUCCESS {
        return Err(io::Error::other(format!(
            "CM_Query_And_Remove_SubTreeW: {} (veto {})",
            remove.0, veto.0
        )));
    }
    let ready = unsafe { CM_Setup_DevNode(devinst, CM_SETUP_DEVNODE_READY) };
    // Enumerating the parent rediscovers the target; it does not reset the USB
    // controller or other sibling disks. Always do this even if ready failed.
    let enumerate = unsafe { CM_Reenumerate_DevNode(parent, CM_REENUMERATE_SYNCHRONOUS) };
    if ready != CR_SUCCESS || enumerate != CR_SUCCESS {
        return Err(io::Error::other(format!(
            "CM_Setup_DevNode: {}; CM_Reenumerate_DevNode: {}",
            ready.0, enumerate.0
        )));
    }
    Ok(())
}

struct DeviceList(HDEVINFO);
impl Drop for DeviceList {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

fn setup_reset(instance: &str) -> io::Result<()> {
    let list = DeviceList(unsafe { SetupDiCreateDeviceInfoList(None, None) }.map_err(win_error)?);
    let id = wide(instance);
    let mut device = SP_DEVINFO_DATA {
        cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
        ..Default::default()
    };
    unsafe { SetupDiOpenDeviceInfoW(list.0, PCWSTR(id.as_ptr()), None, 0, Some(&mut device)) }
        .map_err(win_error)?;
    let mut settings = SP_DEVINSTALL_PARAMS_W {
        cbSize: size_of::<SP_DEVINSTALL_PARAMS_W>() as u32,
        ..Default::default()
    };
    unsafe {
        SetupDiGetDeviceInstallParamsW(list.0, Some(&device), &mut settings).map_err(win_error)?;
        settings.Flags |= DI_QUIETINSTALL;
        SetupDiSetDeviceInstallParamsW(list.0, Some(&device), &settings).map_err(win_error)?;
    }
    // Ensure the target is enabled before requesting a stop/start property change.
    for change in [DICS_ENABLE, DICS_PROPCHANGE] {
        let parameters = SP_PROPCHANGE_PARAMS {
            ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                cbSize: size_of::<SP_CLASSINSTALL_HEADER>() as u32,
                InstallFunction: DIF_PROPERTYCHANGE,
            },
            StateChange: change,
            Scope: if change == DICS_ENABLE {
                DICS_FLAG_GLOBAL
            } else {
                DICS_FLAG_CONFIGSPECIFIC
            },
            HwProfile: 0,
        };
        unsafe {
            SetupDiSetClassInstallParamsW(
                list.0,
                Some(&device),
                Some(&parameters.ClassInstallHeader),
                size_of::<SP_PROPCHANGE_PARAMS>() as u32,
            )
            .map_err(win_error)?;
            SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, list.0, Some(&device))
                .map_err(win_error)?;
        }
    }
    unsafe { SetupDiGetDeviceInstallParamsW(list.0, Some(&device), &mut settings) }
        .map_err(win_error)?;
    if settings.Flags.contains(DI_NEEDRESTART) || settings.Flags.contains(DI_NEEDREBOOT) {
        return Err(io::Error::other(tr!(
            "Windows requires a restart to complete the device reset",
            "デバイスのリセット完了にはWindowsの再起動が必要です"
        )));
    }
    Ok(())
}

fn worker(method: Method, instance: &str) -> io::Result<String> {
    let mut input = Vec::new();
    io::stdin().take(65536 + 128).read_to_end(&mut input)?;
    let split = input
        .iter()
        .position(|&byte| byte == b'\n')
        .ok_or_else(|| {
            io::Error::other(tr!(
                "Invalid reset verification input",
                "リセット検証の入力が不正です"
            ))
        })?;
    let header = std::str::from_utf8(&input[..split]).map_err(io::Error::other)?;
    let mut fields = header.split_whitespace();
    let total: u64 = fields
        .next()
        .ok_or_else(|| {
            io::Error::other(tr!("Missing disk size", "ディスク容量が指定されていません"))
        })?
        .parse()
        .map_err(io::Error::other)?;
    let reference: u64 = fields
        .next()
        .ok_or_else(|| {
            io::Error::other(tr!("Missing reference LBA", "基準LBAが指定されていません"))
        })?
        .parse()
        .map_err(io::Error::other)?;
    let sector_size = crate::sector::SectorSize::new(
        fields
            .next()
            .ok_or_else(|| {
                io::Error::other(tr!("Missing sector size", "セクタサイズがありません"))
            })?
            .parse()
            .map_err(io::Error::other)?,
    )?;
    let data = &input[split + 1..];
    let bootstrap = reference == u64::MAX && data.is_empty();
    if (!bootstrap && (data.len() != sector_size.bytes() || reference >= total)) || total == 0 {
        return Err(io::Error::other(tr!(
            "Invalid reference sector",
            "基準セクタが不正です"
        )));
    }
    match method {
        Method::ConfigurationManager => cm_reset(instance)?,
        Method::SetupApi => setup_reset(instance)?,
    }
    // Only the scanner issues data reads, enforcing one global outstanding-read limit.
    // Identity, capacity, sector size and reference bytes are checked on reconnect.
    windows_native::disk_for_instance(instance)
}

/// Private subprocess entry point; deliberately absent from the public CLI.
pub(crate) fn worker_entry() -> Option<i32> {
    let arguments: Vec<_> = std::env::args_os().collect();
    if arguments.get(1).and_then(|value| value.to_str()) != Some(WORKER_FLAG) {
        return None;
    }
    crate::output::silence_console();
    let method = match arguments.get(2).and_then(|value| value.to_str()) {
        Some("cm") => Method::ConfigurationManager,
        Some("setup") => Method::SetupApi,
        _ => return Some(2),
    };
    let Some(instance) = arguments.get(3).and_then(|value| value.to_str()) else {
        return Some(2);
    };
    let result = worker(method, instance);
    let stats = crate::visual::statistics();
    println!(
        "STATS {} {:.9} {} {:.9}",
        stats.ok_count, stats.ok_seconds, stats.ng_count, stats.ng_seconds
    );
    for line in crate::logging::take_pending() {
        println!("LOG {line}");
    }
    match result {
        Ok(path) => {
            println!("DEVICE {path}");
            Some(0)
        }
        Err(error) => {
            eprintln!("{error}");
            Some(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_reset_failures_back_off_and_cap_at_five_minutes() {
        let mut state = Backoff::default();
        let now = Instant::now();
        for seconds in [10, 20, 40, 80, 160, 300, 300] {
            assert_eq!(state.failed(now), Duration::from_secs(seconds));
            assert_eq!(state.remaining(now), Duration::from_secs(seconds));
            assert_eq!(
                state.remaining(now + Duration::from_secs(seconds)),
                Duration::ZERO
            );
        }
    }
    #[test]
    fn cooldowns_are_per_method_and_expiry_requests_a_single_retry() {
        let now = Instant::now();
        BACKOFF.with(|states| {
            let mut states = states.borrow_mut();
            states[0].failed(now);
            states[1] = Backoff::default();
        });
        assert!(can_attempt()); // SetupAPI remains available after CM fails.
        BACKOFF.with(|states| {
            states.borrow_mut()[1].failed(now);
        });
        assert!(!can_attempt());
        RETRY_AT.with(|value| value.set(Some(now - Duration::from_secs(1))));
        assert!(take_failed());
        assert!(!take_failed());
        BACKOFF.with(|states| *states.borrow_mut() = [Backoff::default(), Backoff::default()]);
        assert!(can_attempt());
    }
    #[test]
    #[ignore = "Synthetic reset helper; invoked only by the completion-notification test"]
    fn synthetic_reset_failure() {
        std::thread::sleep(Duration::from_millis(150));
        std::process::exit(7);
    }

    #[test]
    fn pending_reset_is_retained_until_notification_then_allows_setup_fallback() -> io::Result<()> {
        let mut child = ResetChild(Some(
            Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "reset::tests::synthetic_reset_failure",
                    "--ignored",
                ])
                .creation_flags(CREATE_NO_WINDOW.0)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        ));
        let output = collect_pipe(child.stdout.take().unwrap())?;
        let errors = collect_pipe(child.stderr.take().unwrap())?;
        PENDING.with(|pending| {
            *pending.borrow_mut() = Some(PendingReset {
                child,
                output,
                errors,
                method: Method::ConfigurationManager,
            })
        });
        poll_pending();
        assert!(is_pending());
        let deadline = Instant::now() + Duration::from_secs(10);
        while is_pending() && Instant::now() < deadline {
            poll_pending();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!is_pending());
        assert!(take_failed());
        assert_eq!(NEXT_METHOD.with(|next| next.take()), Some(Method::SetupApi));
        Ok(())
    }

    #[test]
    fn worker_report_preserves_both_timing_groups_logs_and_device_identity() {
        let report = decode_report(
            b"STATS 3 0.375 2 5.500\nLOG [timestamp] read succeeded\nDEVICE \\.\\PhysicalDrive2\n",
        );
        // Use the exact Windows path spelling rather than accepting arbitrary text.
        assert!(report.is_err());
        let wire = format!(
            "STATS 3 0.375 2 5.500\nLOG [timestamp] read succeeded\nDEVICE {}\n",
            r"\\.\PhysicalDrive2"
        );
        let report = decode_report(wire.as_bytes()).unwrap();
        assert_eq!(report.stats[0].ok_count, 3);
        assert_eq!(report.stats[0].ok_seconds, 0.375);
        assert_eq!(report.stats[0].ng_count, 2);
        assert_eq!(report.stats[0].ng_seconds, 5.5);
        assert_eq!(report.logs, ["[timestamp] read succeeded"]);
        assert_eq!(report.path.as_deref(), Some(r"\\.\PhysicalDrive2"));
        assert!(decode_report(b"STATS 1 NaN 0 0\n").is_err());
    }
    #[test]
    fn cm_precedes_setup_and_success_stops_fallback() {
        let mut attempts = Vec::new();
        assert!(sequence(|method| {
            attempts.push(method);
            true
        }));
        assert_eq!(attempts, [Method::ConfigurationManager]);
        attempts.clear();
        assert!(sequence(|method| {
            attempts.push(method);
            method == Method::SetupApi
        }));
        assert_eq!(attempts, [Method::ConfigurationManager, Method::SetupApi]);
        attempts.clear();
        assert!(!sequence(|method| {
            attempts.push(method);
            false
        }));
        assert_eq!(attempts, [Method::ConfigurationManager, Method::SetupApi]);
    }
}
