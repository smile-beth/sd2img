macro_rules! failed {
    ($($args:tt)*) => { $crate::output::emit(&format!($($args)*), $crate::output::Tone::Error, true) };
}
macro_rules! info {
    ($($args:tt)*) => { $crate::output::emit(&format!($($args)*), $crate::output::Tone::Info, false) };
}
macro_rules! success {
    ($($args:tt)*) => { $crate::output::emit(&format!($($args)*), $crate::output::Tone::Success, false) };
}
macro_rules! warn {
    ($($args:tt)*) => { $crate::output::emit(&format!($($args)*), $crate::output::Tone::Warning, false) };
}
macro_rules! warning_stderr {
    ($($args:tt)*) => { $crate::output::emit(&format!($($args)*), $crate::output::Tone::Warning, true) };
}

pub(crate) use crate::logging::{check_log, flush_log, open_log};
use crate::{rescue_map::RescueMap, sector::SectorSize};
use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

pub(crate) fn format_size(sectors: u64, sector: SectorSize) -> String {
    format_bytes(sectors as u128 * sector.bytes() as u128)
}
pub(crate) fn format_bytes(bytes: u128) -> String {
    let mut value = bytes as f64;
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < units.len() {
        value /= 1024.0;
        unit += 1;
    }
    message!(
        "{:.2} {} ({} bytes)",
        "{:.2} {} ({} バイト)",
        value,
        units[unit],
        bytes
    )
}

pub(crate) fn print_progress(map: &RescueMap) -> io::Result<()> {
    let (ok, ng, uncertain) = map.stats();
    let total = ok + ng + uncertain;
    let percent = |count: u64| {
        if total == 0 {
            0.0
        } else {
            count as f64 * 100.0 / total as f64
        }
    };
    info!(
        "{}",
        message!(
            "\n[Progress] Total: {} sectors / {}",
            "\n[進捗] 全体: {} セクタ / {}",
            total,
            format_size(total, map.sector_size)
        )
    );
    for (label, count) in [
        ("OK", ok),
        (tr!("Legacy bad", "旧NG"), ng),
        (tr!("Unresolved", "未確定"), uncertain),
    ] {
        info!(
            "{}",
            message!(
                "  {}: {} sectors / {} / {:.2}%",
                "  {}: {} セクタ / {} / {:.2}%",
                label,
                count,
                format_size(count, map.sector_size),
                percent(count)
            )
        );
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Info,
    Success,
    Warning,
    Error,
}

fn colored(message: &str, tone: Tone, enabled: bool) -> String {
    if !enabled {
        return message.to_owned();
    }
    let code = match tone {
        Tone::Info => "36",
        Tone::Success => "32",
        Tone::Warning => "33",
        Tone::Error => "31",
    };
    format!("\x1b[{code}m{message}\x1b[0m")
}

fn enable_console_color(stderr: bool) -> bool {
    use windows::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, CONSOLE_MODE,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    let terminal = if stderr {
        io::stderr().is_terminal()
    } else {
        io::stdout().is_terminal()
    };
    if !terminal {
        return false;
    }
    unsafe {
        let Ok(handle) = GetStdHandle(if stderr {
            STD_ERROR_HANDLE
        } else {
            STD_OUTPUT_HANDLE
        }) else {
            return false;
        };
        let mut mode = CONSOLE_MODE::default();
        GetConsoleMode(handle, &mut mode).is_ok()
            && SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
    }
}

fn colors() -> &'static (bool, bool) {
    static COLORS: OnceLock<(bool, bool)> = OnceLock::new();
    COLORS.get_or_init(|| (enable_console_color(false), enable_console_color(true)))
}

pub(crate) fn init() {
    let _ = colors();
}
pub(crate) fn terminal_color() -> bool {
    colors().0
}

pub(crate) fn emit(message: &str, tone: Tone, stderr: bool) {
    crate::logging::append(message);
    if SILENT.load(Ordering::Relaxed) {
        return;
    }
    if crate::visual::active() {
        crate::visual::event(message, tone);
    } else if !crate::logging::is_open() {
        // Before elevation / before the destination is known, errors remain visible.
        if stderr {
            eprintln!("{message}");
        } else {
            println!("{message}");
        }
    }
}

static SILENT: AtomicBool = AtomicBool::new(false);
pub(crate) fn silence_console() {
    SILENT.store(true, Ordering::Relaxed);
}

pub(crate) fn error(message: &str) {
    emit(message, Tone::Error, true);
}

pub(crate) fn final_line(message: &str, tone: Tone) {
    crate::logging::append(message);
    if tone == Tone::Error {
        eprintln!("{}", colored(message, tone, colors().1));
    } else {
        println!("{}", colored(message, tone, colors().0));
    }
}

pub(crate) fn sector_count(count: u64, sector: SectorSize) -> String {
    message!(
        "{} sectors / {}",
        "{} セクタ / {}",
        count,
        format_size(count, sector)
    )
}

fn read_line(lba: u64, count: u64, sector: SectorSize) -> String {
    message!(
        "[READ OK] LBA {}..{} | {}",
        "[読取成功] LBA {}..{} | {}",
        lba,
        lba.saturating_add(count),
        sector_count(count, sector)
    )
}

pub(crate) fn read_success(lba: u64, count: u64, sector: SectorSize) {
    crate::logging::append(&read_line(lba, count, sector));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn read_lines_include_range_sectors_scaled_size_and_exact_bytes() {
        let line = read_line(2048, 128, SectorSize::LEGACY);
        assert!(line.contains("2048..2176"));
        assert!(line.contains("128"));
        assert!(line.contains("64.00 KiB"));
        assert!(line.contains("65536"));
        assert_eq!(line.lines().count(), 1);
        assert!(format_size(2_097_152, SectorSize::LEGACY).contains("1.00 GiB"));
        assert!(format_size(2_097_152, SectorSize::LEGACY).contains("1073741824"));
        let four_k = sector_count(16, SectorSize::new(4096).unwrap());
        assert!(four_k.contains("64.00 KiB"));
        assert!(four_k.contains("65536"));
    }
    #[test]
    fn color_is_severity_specific_and_absent_when_disabled() {
        assert_eq!(colored("line", Tone::Success, false), "line");
        for (tone, code) in [
            (Tone::Success, "32"),
            (Tone::Warning, "33"),
            (Tone::Error, "31"),
            (Tone::Info, "36"),
        ] {
            assert_eq!(
                colored("line", tone, true),
                format!("\x1b[{code}mline\x1b[0m")
            );
        }
    }
}
