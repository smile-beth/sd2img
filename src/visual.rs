//! Console-only renderer. This thread never opens or reads a storage device.
use crate::{
    output::{self, Tone},
    rescue_map::{Extent, RescueMap, SectorStatus},
    sector::SectorSize,
};
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, VecDeque},
    io::{self, IsTerminal, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::Win32::System::Console::{
    GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
    CONSOLE_SCREEN_BUFFER_INFO, ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
};

const HEADER_ROWS: usize = 13;
const LOG_ROWS: usize = 5;

#[derive(Clone, Copy)]
enum Phase {
    Preparing,
    Scanning,
    Reading,
    Cancelling,
    Reconnecting,
    Resetting(&'static str),
    Finished,
}

#[derive(Clone)]
struct Reading {
    start: u64,
    count: u64,
    since: Instant,
    timeout_ms: u32,
}

#[derive(Default)]
struct MedianHeaps {
    lower: BinaryHeap<u64>,
    upper: BinaryHeap<Reverse<u64>>,
}
#[derive(Clone, Default)]
struct MedianSamples {
    // Share storage with read-only display snapshots, rather than copying the
    // complete history every frame. The cached value stays snapshot-consistent.
    heaps: Arc<Mutex<MedianHeaps>>,
    value: Option<f64>,
}
impl MedianSamples {
    fn add(&mut self, value: f64) {
        if !value.is_finite() || value < 0.0 {
            return;
        }
        // Nonnegative finite IEEE-754 values have monotonically ordered bits.
        let key = if value == 0.0 { 0 } else { value.to_bits() };
        let mut heaps = self.heaps.lock().unwrap();
        if heaps.lower.peek().is_none_or(|middle| key <= *middle) {
            heaps.lower.push(key);
        } else {
            heaps.upper.push(Reverse(key));
        }
        if heaps.lower.len() > heaps.upper.len() + 1 {
            let middle = heaps.lower.pop().unwrap();
            heaps.upper.push(Reverse(middle));
        } else if heaps.upper.len() > heaps.lower.len() {
            let Reverse(middle) = heaps.upper.pop().unwrap();
            heaps.lower.push(middle);
        }
        let low = f64::from_bits(*heaps.lower.peek().unwrap());
        self.value = Some(if heaps.lower.len() == heaps.upper.len() {
            low * 0.5 + f64::from_bits(heaps.upper.peek().unwrap().0) * 0.5
        } else {
            low
        });
    }
    fn median(&self) -> Option<f64> {
        self.value
    }
}

#[derive(Clone, Default)]
struct Latency {
    count: u64,
    seconds: f64,
    samples: MedianSamples,
}

#[derive(Clone, Default)]
struct TransferRate {
    bytes: u128,
    seconds: f64,
    samples: MedianSamples,
}
impl TransferRate {
    fn add(&mut self, bytes: usize, elapsed: Duration) {
        if elapsed.is_zero() {
            return;
        }
        self.bytes = self.bytes.saturating_add(bytes as u128);
        self.seconds += elapsed.as_secs_f64();
        self.samples.add(bytes as f64 / elapsed.as_secs_f64());
    }
    fn label(&self) -> String {
        if self.seconds <= 0.0 {
            return "--".to_owned();
        }
        Self::format_speed(self.bytes as f64 / self.seconds)
    }
    fn summary(&self) -> String {
        format!(
            "{} / {}",
            self.label(),
            self.samples
                .median()
                .map_or_else(|| "--".to_owned(), Self::format_speed)
        )
    }
    fn format_speed(speed: f64) -> String {
        let (value, unit) = if speed >= 1_000_000.0 {
            (speed / 1_000_000.0, "MB/s")
        } else if speed >= 1_000.0 {
            (speed / 1_000.0, "KB/s")
        } else {
            (speed, "B/s")
        };
        format!("{value:.2} {unit}")
    }
}
pub(crate) fn record_transfer(bytes: usize, elapsed: Duration, delayed: bool) {
    update(|state| {
        if delayed {
            state.late_transfer.add(bytes, elapsed);
        } else {
            state.normal_transfer.add(bytes, elapsed);
        }
    });
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ReadStatistics {
    pub(crate) ok_count: u64,
    pub(crate) ok_seconds: f64,
    pub(crate) ng_count: u64,
    pub(crate) ng_seconds: f64,
}
fn statistics_state() -> &'static Mutex<ReadStatistics> {
    static METRICS: OnceLock<Mutex<ReadStatistics>> = OnceLock::new();
    METRICS.get_or_init(|| Mutex::new(ReadStatistics::default()))
}
pub(crate) fn statistics() -> ReadStatistics {
    *statistics_state().lock().unwrap()
}
pub(crate) fn merge_statistics(sample: ReadStatistics) {
    {
        let mut stats = statistics_state().lock().unwrap();
        stats.ok_count = stats.ok_count.saturating_add(sample.ok_count);
        stats.ok_seconds += sample.ok_seconds;
        stats.ng_count = stats.ng_count.saturating_add(sample.ng_count);
        stats.ng_seconds += sample.ng_seconds;
    }
    update(|state| {
        state.ok_wait.count = state.ok_wait.count.saturating_add(sample.ok_count);
        state.ok_wait.seconds += sample.ok_seconds;
        state.ng_wait.count = state.ng_wait.count.saturating_add(sample.ng_count);
        state.ng_wait.seconds += sample.ng_seconds;
        // Aggregate helper reports cannot reconstruct individual observations.
        if sample.ok_count == 1 {
            state.ok_wait.samples.add(sample.ok_seconds);
        }
        if sample.ng_count == 1 {
            state.ng_wait.samples.add(sample.ng_seconds);
        }
    });
}
pub(crate) fn record_completion(elapsed: Duration, ok: bool) {
    let mut stats = ReadStatistics::default();
    if ok {
        stats.ok_count = 1;
        stats.ok_seconds = elapsed.as_secs_f64();
    } else {
        stats.ng_count = 1;
        stats.ng_seconds = elapsed.as_secs_f64();
    }
    merge_statistics(stats);
}

impl Latency {
    #[cfg(test)]
    fn add(&mut self, duration: Duration) {
        self.count = self.count.saturating_add(1);
        self.seconds += duration.as_secs_f64();
        self.samples.add(duration.as_secs_f64());
    }
    fn label(&self) -> String {
        if self.count == 0 {
            "--/--s (0)".to_owned()
        } else {
            message!(
                "{:.3}/{}s ({})",
                "{:.3}/{}s ({}回)",
                self.seconds / self.count as f64,
                self.samples
                    .median()
                    .map_or_else(|| "--".to_owned(), |median| format!("{median:.3}")),
                self.count
            )
        }
    }
}

#[derive(Clone)]
struct State {
    device: String,
    image: String,
    order: String,
    total: u64,
    sector_size: SectorSize,
    ok_wait: Latency,
    ng_wait: Latency,
    normal_transfer: TransferRate,
    late_transfer: TransferRate,
    extents: Vec<Extent>,
    phase: Phase,
    reading: Option<Reading>,
    started: Instant,
    last_map: Instant,
    logs: VecDeque<(Tone, String)>,
    result: Option<String>,
    pending_operation: Option<&'static str>,
    probe_ranges: Vec<u64>,
    parallel_limit: usize,
}

impl State {
    fn new() -> Self {
        Self {
            device: String::new(),
            image: String::new(),
            order: String::new(),
            total: 0,
            sector_size: SectorSize::LEGACY,
            ok_wait: Latency::default(),
            ng_wait: Latency::default(),
            normal_transfer: TransferRate::default(),
            late_transfer: TransferRate::default(),
            extents: Vec::new(),
            phase: Phase::Preparing,
            reading: None,
            started: Instant::now(),
            last_map: Instant::now() - Duration::from_secs(1),
            logs: VecDeque::new(),
            result: None,
            pending_operation: None,
            probe_ranges: Vec::new(),
            parallel_limit: 16,
        }
    }
}

static STATE: OnceLock<Arc<Mutex<State>>> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

fn update(f: impl FnOnce(&mut State)) {
    if let Some(state) = STATE.get() {
        f(&mut state.lock().unwrap());
    }
}

pub(crate) fn disk(device: &str, image: &str, total: u64, order: &str) {
    update(|state| {
        state.device = device.to_owned();
        state.image = image.to_owned();
        state.total = total;
        state.order = order.to_owned();
    });
}

pub(crate) fn pending_operation(name: Option<&'static str>) {
    update(|state| state.pending_operation = name);
}
pub(crate) fn operation_settings(parallel: usize) {
    update(|state| {
        state.parallel_limit = parallel;
    });
}
pub(crate) fn probing(lbas: Vec<u64>) {
    update(|state| {
        state.probe_ranges = lbas;
        state.reading = None;
    });
}

pub(crate) fn settings(sector_size: SectorSize) {
    update(|state| {
        state.sector_size = sector_size;
    });
}
pub(crate) fn read_finished(elapsed: Duration, success: bool) {
    record_completion(elapsed, success);
    update(|state| {
        state.phase = Phase::Scanning;
        state.reading = None;
    });
}

pub(crate) fn map(map: &RescueMap, force: bool) {
    update(|state| {
        if force || state.last_map.elapsed() >= Duration::from_millis(100) {
            state.extents = map.extents.clone();
            state.last_map = Instant::now();
        }
    });
}

pub(crate) fn begin_read(start: u64, count: u64, timeout_ms: u32) {
    update(|state| {
        state.phase = Phase::Reading;
        state.reading = Some(Reading {
            start,
            count,
            since: Instant::now(),
            timeout_ms,
        });
    });
}
pub(crate) fn cancelling() {
    update(|state| state.phase = Phase::Cancelling);
}
pub(crate) fn end_read() {
    update(|state| {
        state.phase = Phase::Scanning;
        state.reading = None;
    });
}
pub(crate) fn reconnecting() {
    update(|state| {
        state.phase = Phase::Reconnecting;
        state.reading = None;
    });
}
pub(crate) fn resetting(method: &'static str) {
    update(|state| {
        state.phase = Phase::Resetting(method);
        state.reading = None;
    });
}
pub(crate) fn event(text: &str, tone: Tone) {
    if tone == Tone::Info {
        return;
    }
    update(|state| {
        for line in text.lines().filter(|line| !line.is_empty()) {
            state.logs.push_back((tone, line.to_owned()));
            if state.logs.len() > 100 {
                state.logs.pop_front();
            }
        }
    });
}
pub(crate) fn finished(result: &str) {
    update(|state| {
        state.phase = Phase::Finished;
        state.reading = None;
        state.result = Some(result.to_owned());
    });
}

pub(crate) struct Screen {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    old_mode: Option<CONSOLE_MODE>,
}

impl Screen {
    pub(crate) fn start() -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut screen = Self {
            stop: stop.clone(),
            worker: None,
            old_mode: None,
        };
        // Redirected output has no terminal to draw into; detailed events still go to .log.
        if !io::stdout().is_terminal() {
            return Ok(screen);
        }
        let handle =
            unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }.map_err(crate::volumes::win_error)?;
        let mut old_mode = CONSOLE_MODE::default();
        unsafe {
            GetConsoleMode(handle, &mut old_mode).map_err(crate::volumes::win_error)?;
            SetConsoleMode(handle, old_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING)
                .map_err(crate::volumes::win_error)?;
        }
        screen.old_mode = Some(old_mode);
        let state = Arc::new(Mutex::new(State::new()));
        STATE.set(state.clone()).map_err(|_| {
            io::Error::other(tr!("Display already initialized", "画面は初期化済みです"))
        })?;
        ACTIVE.store(true, Ordering::Relaxed);
        write!(io::stdout(), "\x1b[?1049h\x1b[?25l\x1b[2J")?;
        io::stdout().flush()?;
        screen.worker = Some(
            thread::Builder::new()
                .name("sd2img-display".to_owned())
                .spawn(move || {
                    let mut tick = 0usize;
                    loop {
                        if !ACTIVE.load(Ordering::Relaxed) {
                            break;
                        }
                        let snapshot = state.lock().unwrap().clone();
                        let (width, height) = dimensions();
                        let frame =
                            render(&snapshot, width, height, tick, output::terminal_color());
                        let mut stdout = io::stdout().lock();
                        if write!(stdout, "\x1b[H{frame}\x1b[J")
                            .and_then(|_| stdout.flush())
                            .is_err()
                        {
                            break;
                        }
                        drop(stdout);
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        tick = tick.wrapping_add(1);
                        thread::sleep(Duration::from_millis(125));
                    }
                })?,
        );
        Ok(screen)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(mode) = self.old_mode.take() {
            let _ = write!(io::stdout(), "\x1b[0m\x1b[?25h\x1b[?1049l");
            let _ = io::stdout().flush();
            if let Ok(handle) = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) } {
                unsafe {
                    let _ = SetConsoleMode(handle, mode);
                }
            }
        }
        ACTIVE.store(false, Ordering::Relaxed);
    }
}

fn dimensions() -> (usize, usize) {
    let mut info = CONSOLE_SCREEN_BUFFER_INFO::default();
    if let Ok(handle) = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) } {
        if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) }.is_ok() {
            return (
                (info.srWindow.Right - info.srWindow.Left + 1).max(1) as usize,
                (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as usize,
            );
        }
    }
    (80, 25)
}

fn char_width(c: char) -> usize {
    if c.is_control() {
        0
    } else if matches!(c as u32,
        0x1100..=0x115f | 0x2329..=0x232a | 0x2e80..=0xa4cf | 0xac00..=0xd7a3 |
        0xf900..=0xfaff | 0xfe10..=0xfe19 | 0xfe30..=0xfe6f | 0xff01..=0xff60 |
        0xffe0..=0xffe6 | 0x1f300..=0x1faff | 0x20000..=0x3fffd)
    {
        2
    } else {
        1
    }
}

fn clip(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for c in text.chars().filter(|c| !c.is_control()) {
        let size = char_width(c);
        if used + size > width {
            break;
        }
        result.push(c);
        used += size;
    }
    result
}

fn wrapped(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for c in text.chars().filter(|c| !c.is_control()) {
        let size = char_width(c);
        if used + size > width && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        if size <= width {
            line.push(c);
            used += size;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn paint(text: &str, color: u8, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{color}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

// Clip plain segments before adding ANSI escapes so colored fields still fit
// narrow terminals and Japanese full-width characters.
fn paint_parts(parts: &[(&str, u8)], width: usize, color: bool) -> String {
    let mut remaining = width;
    let mut line = String::new();
    for (text, tone) in parts {
        let text = clip(text, remaining);
        remaining = remaining.saturating_sub(text.chars().map(char_width).sum());
        line.push_str(&paint(&text, *tone, color));
        if remaining == 0 {
            break;
        }
    }
    format!("{line}\x1b[K")
}

fn phase_color(state: &State) -> u8 {
    if state.pending_operation.is_some() {
        return 93;
    }
    match state.phase {
        Phase::Cancelling => 91,
        Phase::Resetting(_) | Phase::Reconnecting => 95,
        Phase::Finished => match state.result.as_deref() {
            Some(text) if text == tr!("Stopped with an error", "エラーで停止") => 91,
            Some(text) if text == tr!("Stopped", "中断") => 93,
            _ => 92,
        },
        Phase::Reading
            if state.reading.as_ref().is_some_and(|read| {
                read.since.elapsed() >= Duration::from_millis(read.timeout_ms as u64)
            }) =>
        {
            93
        }
        Phase::Scanning if !state.probe_ranges.is_empty() => 93,
        Phase::Preparing => 97,
        _ => 96,
    }
}

fn phase_label(state: &State, tick: usize) -> String {
    let spinner = ["|", "/", "-", "\\"][tick % 4];
    if let Some(name) = state.pending_operation {
        return message!(
            "{spinner} Pending: {name}",
            "{spinner} 完了通知待ち: {name}"
        );
    }
    if matches!(state.phase, Phase::Scanning)
        && state.reading.is_none()
        && !state.probe_ranges.is_empty()
    {
        return message!(
            "{spinner} Awaiting completion of {} pending reads (scan limit {})",
            "{spinner} 未完了の読み取り{}件の応答待ち（走査上限{}件）",
            state.probe_ranges.len(),
            state.parallel_limit
        );
    }
    let label = match state.phase {
        Phase::Preparing => tr!("Preparing / locking volumes", "準備・ボリュームのロック中"),
        Phase::Scanning => tr!("Scanning", "走査中"),
        Phase::Reading => {
            if state.reading.as_ref().is_some_and(|read| {
                read.since.elapsed() >= Duration::from_millis(read.timeout_ms as u64)
            }) {
                tr!("Waiting for device response", "デバイスの応答待ち")
            } else {
                tr!("Reading", "読み取り中")
            }
        }
        Phase::Cancelling => tr!(
            "Timed out / waiting for I/O cancellation",
            "停止要求・I/O取消処理"
        ),
        Phase::Reconnecting => tr!(
            "Waiting for reconnection / volume protection",
            "再接続・ボリューム保護の確認待ち"
        ),
        Phase::Resetting(method) => {
            return message!(
                "{spinner} Resetting via {method}",
                "{spinner} {method} でリセット中"
            )
        }
        Phase::Finished => return state.result.clone().unwrap_or_default(),
    };
    format!("{spinner} {label}")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cell {
    Untried,
    Good,
    Failed,
    AllFailed,
    Mixed,
    Active,
    Waiting,
}

fn cells(state: &State, capacity: usize) -> (u64, Vec<Cell>) {
    if state.total == 0 || capacity == 0 {
        return (0, Vec::new());
    }
    let block = state.total.div_ceil(capacity as u64);
    let count = state.total.div_ceil(block) as usize;
    let mut result = Vec::with_capacity(count);
    let mut cursor = 0;
    for index in 0..count {
        let start = index as u64 * block;
        let end = start.saturating_add(block).min(state.total);
        while cursor < state.extents.len()
            && state.extents[cursor]
                .start
                .saturating_add(state.extents[cursor].len)
                <= start
        {
            cursor += 1;
        }
        let mut good = false;
        let mut failed = false;
        let mut unknown = false;
        for extent in &state.extents[cursor..] {
            if extent.start >= end {
                break;
            }
            match extent.status {
                SectorStatus::Rescued => good = true,
                SectorStatus::Bad | SectorStatus::Deferred | SectorStatus::Pending => failed = true,
                SectorStatus::Untried => unknown = true,
            }
        }
        let reading = state
            .reading
            .as_ref()
            .is_some_and(|read| start < read.start.saturating_add(read.count) && read.start < end);
        let reading_timed_out = reading
            && state.reading.as_ref().is_some_and(|read| {
                read.since.elapsed() >= Duration::from_millis(read.timeout_ms as u64)
            });
        let pending = state
            .probe_ranges
            .iter()
            .any(|&lba| start <= lba && lba < end);
        // A coarse cell can contain an old pending request and a new read.
        // Keep the current read visible until its own timeout is reached.
        result.push(if reading_timed_out {
            Cell::Waiting
        } else if reading {
            Cell::Active
        } else if pending {
            Cell::Waiting
        } else if failed && !good && !unknown {
            Cell::AllFailed
        } else if failed {
            Cell::Failed
        } else if good && unknown {
            Cell::Mixed
        } else if good {
            Cell::Good
        } else {
            Cell::Untried
        });
    }
    (block, result)
}

fn render(state: &State, width: usize, height: usize, tick: usize, color: bool) -> String {
    // Keep the last terminal column unused to avoid automatic line wrapping.
    let width = width.saturating_sub(1).max(1);
    let height = height.max(1);
    if width < 44 || height < HEADER_ROWS + LOG_ROWS + 2 {
        let lines = [
            "sd2img".to_owned(),
            phase_label(state, tick),
            tr!(
                "Resize the terminal to at least 45 columns and 20 rows.",
                "端末を45列・20行以上に広げてください。"
            )
            .to_owned(),
        ];
        return lines
            .iter()
            .take(height)
            .map(|line| format!("{}\x1b[K", clip(line, width)))
            .collect::<Vec<_>>()
            .join("\r\n");
    }
    let grid_rows = height - HEADER_ROWS - LOG_ROWS - 1;
    let (block, map) = cells(state, width * grid_rows);
    let mut ok = 0;
    let mut ng = 0;
    let mut untried = state.total;
    for extent in &state.extents {
        match extent.status {
            SectorStatus::Rescued => {
                ok += extent.len;
                untried = untried.saturating_sub(extent.len);
            }
            SectorStatus::Bad | SectorStatus::Deferred | SectorStatus::Pending => {
                ng += extent.len;
                untried = untried.saturating_sub(extent.len);
            }
            SectorStatus::Untried => {}
        }
    }
    let percent = if state.total == 0 {
        0.0
    } else {
        ok as f64 * 100.0 / state.total as f64
    };
    let current = match &state.reading {
        Some(read) => message!(
            "LBA {}..{} | {} | {:.1}s/{:.1}s",
            "LBA {}..{} | {} | {:.1}s/{:.1}s",
            read.start,
            read.start.saturating_add(read.count),
            output::sector_count(read.count, state.sector_size),
            read.since.elapsed().as_secs_f64(),
            read.timeout_ms as f64 / 1000.0
        ),
        None => "-".to_owned(),
    };
    let header = [
        format!(
            "sd2img {} | {} | {:.0}s",
            env!("CARGO_PKG_VERSION"),
            phase_label(state, tick),
            state.started.elapsed().as_secs_f64()
        ),
        message!(
            "Device: {} | Order: {}",
            "対象: {} | 順序: {}",
            state.device,
            state.order
        ),
        message!("Image: {}", "イメージ: {}", state.image),
        message!(
            "Disk: {}",
            "容量: {}",
            output::sector_count(state.total, state.sector_size)
        ),
        format!(
            "OK: {} | {percent:.2}%",
            output::sector_count(ok, state.sector_size)
        ),
        message!(
            "NG/retry: {}",
            "NG/再試行: {}",
            output::sector_count(ng, state.sector_size)
        ),
        message!(
            "Untried: {}",
            "未処理: {}",
            output::sector_count(untried, state.sector_size)
        ),
        message!("Current: {}", "現在: {}", current),
        message!(
            "Response mean/median OK: {} | NG: {}",
            "応答時間 平均/中央値 OK: {} | NG: {}",
            state.ok_wait.label(),
            state.ng_wait.label()
        ),
        message!(
            "Transfer mean/median Normal: {} | Delayed: {}",
            "転送速度 平均/中央値 通常: {} | 待機後: {}",
            state.normal_transfer.summary(),
            state.late_transfer.summary()
        ),
        message!(
            "Cell: {}",
            "1マス: {}",
            output::sector_count(block, state.sector_size)
        ),
        tr!(
            ". untried # all OK X all retry x some retry : OK+untried R read W wait",
            ". 未処理 # 全OK X 全再試行 x 一部再試行 : OK+未処理 R 読取 W 待ち"
        )
        .to_owned(),
        "-".repeat(width),
    ];
    let mut lines: Vec<String> = header
        .iter()
        .enumerate()
        .map(|(index, line)| match index {
            0 => paint_parts(
                &[
                    (&format!("sd2img {} | ", env!("CARGO_PKG_VERSION")), 97),
                    (&phase_label(state, tick), phase_color(state)),
                    (
                        &format!(" | {:.0}s", state.started.elapsed().as_secs_f64()),
                        90,
                    ),
                ],
                width,
                color,
            ),
            8 => paint_parts(
                &[
                    (tr!("Response mean/median ", "応答時間 平均/中央値 "), 97),
                    (&format!("OK: {}", state.ok_wait.label()), 92),
                    (" | ", 90),
                    (&format!("NG: {}", state.ng_wait.label()), 91),
                ],
                width,
                color,
            ),
            9 => paint_parts(
                &[
                    (tr!("Transfer mean/median ", "転送速度 平均/中央値 "), 97),
                    (
                        &message!("Normal: {}", "通常: {}", state.normal_transfer.summary()),
                        92,
                    ),
                    (" | ", 90),
                    (
                        &message!("Delayed: {}", "待機後: {}", state.late_transfer.summary()),
                        93,
                    ),
                ],
                width,
                color,
            ),
            11 => paint_parts(
                &[
                    (tr!(". untried ", ". 未処理 "), 90),
                    (tr!("# all OK ", "# 全OK "), 92),
                    (tr!("X all retry ", "X 全再試行 "), 91),
                    (tr!("x some retry ", "x 一部再試行 "), 31),
                    (tr!(": OK+untried ", ": OK+未処理 "), 94),
                    (tr!("R read ", "R 読取 "), 96),
                    (tr!("W wait", "W 待ち"), 93),
                ],
                width,
                color,
            ),
            _ => {
                let tone = match index {
                    1 | 2 => 97,
                    4 => 92,
                    5 => 91,
                    6 | 12 => 90,
                    7 => phase_color(state),
                    10 => 94,
                    _ => 37,
                };
                paint_parts(&[(line, tone)], width, color)
            }
        })
        .collect();
    for row in 0..grid_rows {
        let mut line = String::new();
        let mut last_color = 0;
        for cell in map.iter().skip(row * width).take(width) {
            let (symbol, code) = match cell {
                Cell::Untried => ('.', 90),
                Cell::Good => ('#', 32),
                Cell::Failed => ('x', 31),
                Cell::AllFailed => ('X', 91),
                Cell::Mixed => (':', 34),
                Cell::Active => ('R', 36),
                Cell::Waiting => (
                    'W',
                    if matches!(state.phase, Phase::Cancelling) {
                        31
                    } else {
                        33
                    },
                ),
            };
            if color && code != last_color {
                line.push_str(&format!("\x1b[{code}m"));
                last_color = code;
            }
            line.push(symbol);
        }
        lines.push(format!("{line}\x1b[0m\x1b[K"));
    }
    lines.push(format!("{}\x1b[K", "-".repeat(width)));
    let mut logs: Vec<(Tone, String)> = state
        .logs
        .iter()
        .flat_map(|(tone, line)| wrapped(line, width).into_iter().map(|line| (*tone, line)))
        .collect();
    if logs.len() > LOG_ROWS {
        logs.drain(..logs.len() - LOG_ROWS);
    }
    for index in 0..LOG_ROWS {
        lines.push(match logs.get(index) {
            Some((tone, line)) => format!(
                "{}\x1b[K",
                paint(
                    line,
                    match tone {
                        Tone::Error => 31,
                        Tone::Success => 32,
                        _ => 33,
                    },
                    color
                )
            ),
            None => "\x1b[K".to_owned(),
        });
    }
    lines.join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn median_uses_the_full_history_and_keeps_display_snapshots_consistent() {
        let mut samples = MedianSamples::default();
        assert_eq!(samples.median(), None);
        for value in (0..200).rev() {
            samples.add(value as f64);
        }
        assert_eq!(samples.median(), Some(99.5));
        let snapshot = samples.clone();
        samples.add(10_000.0);
        assert_eq!(samples.median(), Some(100.0));
        assert_eq!(snapshot.median(), Some(99.5));
        samples.add(f64::NAN);
        samples.add(f64::INFINITY);
        samples.add(-1.0);
        assert_eq!(samples.median(), Some(100.0));
    }
    #[test]
    fn latency_and_speed_show_different_mean_and_median_for_outliers() {
        let mut latency = Latency::default();
        for millis in [100, 200, 30000] {
            latency.add(Duration::from_millis(millis));
        }
        assert!(latency.label().contains("10.100/0.200s"));
        let mut rate = TransferRate::default();
        rate.add(1000, Duration::from_secs(1));
        rate.add(9000, Duration::from_secs(3));
        rate.add(1_000_000, Duration::from_secs(1));
        assert_eq!(rate.summary(), "202.00 KB/s / 3.00 KB/s");
    }
    #[test]
    fn current_read_stays_r_until_its_timeout_even_with_pending_io_in_the_same_cell() {
        let mut state = State::new();
        state.total = 1_000_000;
        state.phase = Phase::Reading;
        state.probe_ranges = vec![50];
        state.reading = Some(Reading {
            start: 100,
            count: 128,
            since: Instant::now() - Duration::from_millis(2900),
            timeout_ms: 7100,
        });
        assert_eq!(cells(&state, 1).1, vec![Cell::Active]);
        assert!(phase_label(&state, 0).contains(tr!("Reading", "読み取り中")));
        assert_eq!(phase_color(&state), 96);
        state.reading.as_mut().unwrap().since = Instant::now() - Duration::from_millis(7200);
        assert_eq!(cells(&state, 1).1, vec![Cell::Waiting]);
        assert!(phase_label(&state, 0)
            .contains(tr!("Waiting for device response", "デバイスの応答待ち")));
        assert_eq!(phase_color(&state), 93);
    }
    #[test]
    fn all_failed_and_partly_failed_cells_are_distinct_from_all_good_and_mixed() {
        for status in [
            SectorStatus::Bad,
            SectorStatus::Deferred,
            SectorStatus::Pending,
        ] {
            let mut map = RescueMap::new(0, vec![0; 512], 40);
            map.update_range(0, 10, SectorStatus::Rescued);
            map.update_range(10, 10, status);
            map.update_range(20, 1, status);
            map.update_range(21, 9, SectorStatus::Rescued);
            map.update_range(30, 5, SectorStatus::Rescued);
            let mut state = State::new();
            state.total = 40;
            state.extents = map.extents.clone();
            assert_eq!(
                cells(&state, 4).1,
                vec![Cell::Good, Cell::AllFailed, Cell::Failed, Cell::Mixed]
            );
            // An untried remainder also makes failure partial.
            map.update_range(21, 9, SectorStatus::Untried);
            state.extents = map.extents;
            assert_eq!(cells(&state, 4).1[2], Cell::Failed);
            state.probe_ranges = vec![10];
            assert_eq!(cells(&state, 4).1[1], Cell::Waiting);
        }
    }
    #[test]
    fn success_and_failure_means_are_separate_and_use_three_decimal_places() {
        let mut state = State::new();
        state.ok_wait.add(Duration::from_millis(100));
        state.ok_wait.add(Duration::from_millis(200));
        state.ng_wait.add(Duration::from_secs(2));
        state.ng_wait.add(Duration::from_secs(4));
        assert!(state.ok_wait.label().contains("0.150"));
        assert!(state.ng_wait.label().contains("3.000"));
        assert_eq!(state.ok_wait.count, 2);
        assert_eq!(state.ng_wait.count, 2);
        state.sector_size = SectorSize::new(4096).unwrap();
        state
            .normal_transfer
            .add(1_000_000, Duration::from_millis(500));
        state.late_transfer.add(1024, Duration::from_secs(2));
        let frame = strip_ansi(&render(&state, 100, 30, 0, false));
        assert!(frame.contains("2.00 MB/s"));
        assert!(frame.contains("512.00 B/s"));
        assert_eq!(
            frame
                .lines()
                .filter(|line| line.contains("MB/s") || line.contains("B/s"))
                .count(),
            1
        );
        assert!(!frame.contains(tr!("Read cap", "読取上限")));
        assert!(!frame.contains(tr!("Timeout read/operation", "読取/操作タイムアウト")));
    }
    #[test]
    fn transfer_rates_use_total_bytes_over_total_time_and_decimal_units() {
        let mut rate = TransferRate::default();
        assert_eq!(rate.label(), "--");
        rate.add(1_000_000, Duration::ZERO);
        assert_eq!(rate.label(), "--");
        rate.add(1000, Duration::from_secs(1));
        rate.add(9000, Duration::from_secs(3));
        assert_eq!(rate.label(), "2.50 KB/s");
        for (bytes, label) in [
            (999, "999.00 B/s"),
            (1000, "1.00 KB/s"),
            (1_000_000, "1.00 MB/s"),
        ] {
            let mut rate = TransferRate::default();
            rate.add(bytes, Duration::from_secs(1));
            assert_eq!(rate.label(), label);
        }
    }
    fn strip_ansi(text: &str) -> String {
        let mut result = String::new();
        let mut escape = false;
        for c in text.chars() {
            if c == '\x1b' {
                escape = true;
                continue;
            }
            if escape {
                if c.is_ascii_alphabetic() {
                    escape = false;
                }
                continue;
            }
            result.push(c);
        }
        result
    }
    #[test]
    fn map_covers_disk_tail_and_active_and_failed_states_take_priority() {
        let mut state = State::new();
        state.total = 101;
        state.extents = vec![
            Extent {
                start: 0,
                len: 50,
                status: SectorStatus::Rescued,
            },
            Extent {
                start: 50,
                len: 1,
                status: SectorStatus::Pending,
            },
            Extent {
                start: 51,
                len: 50,
                status: SectorStatus::Untried,
            },
        ];
        let (block, map) = cells(&state, 10);
        assert_eq!(block, 11);
        assert_eq!(map.len(), 10);
        assert_eq!(map[4], Cell::Failed);
        assert_eq!(map[9], Cell::Untried);
        state.reading = Some(Reading {
            start: 99,
            count: 2,
            since: Instant::now(),
            timeout_ms: 2000,
        });
        assert_eq!(cells(&state, 10).1[9], Cell::Active);
        // An older request awaiting completion stays distinct from a new read.
        state.probe_ranges = vec![0];
        let map = cells(&state, 10).1;
        assert_eq!(map[0], Cell::Waiting);
        assert_eq!(map[9], Cell::Active);
        state.reading.as_mut().unwrap().since = Instant::now() - Duration::from_secs(3);
        assert_eq!(cells(&state, 10).1[9], Cell::Waiting);
    }
    #[test]
    fn frames_fit_resizes_and_keep_exactly_five_log_rows() {
        let mut state = State::new();
        state.total = 1_000_003;
        state.extents = vec![Extent {
            start: 0,
            len: state.total,
            status: SectorStatus::Untried,
        }];
        state.device = "日本語のディスク名".to_owned();
        for index in 0..10 {
            state
                .logs
                .push_back((Tone::Warning, format!("notice {index}")));
        }
        for (width, height) in [(80, 25), (120, 40), (45, 21)] {
            let frame = strip_ansi(&render(&state, width, height, 0, false));
            assert_eq!(frame.lines().count(), height);
            for line in frame.lines() {
                assert!(line.chars().map(char_width).sum::<usize>() < width);
            }
            let logs: Vec<_> = frame.lines().rev().take(5).collect();
            assert!(logs[0].contains("notice 9"));
            assert!(logs[4].contains("notice 5"));
        }
        assert!(render(&state, 20, 5, 0, false).lines().count() <= 5);
    }
    #[test]
    fn animations_advance_while_waiting_without_device_io() {
        let mut state = State::new();
        state.phase = Phase::Reading;
        state.reading = Some(Reading {
            start: 0,
            count: 1,
            since: Instant::now() - Duration::from_secs(3),
            timeout_ms: 2000,
        });
        assert_ne!(phase_label(&state, 0), phase_label(&state, 1));
        assert!(phase_label(&state, 0)
            .contains(tr!("Waiting for device response", "デバイスの応答待ち")));
        state.phase = Phase::Cancelling;
        assert!(phase_label(&state, 0).contains(tr!("I/O cancellation", "I/O取消")));
    }
}
