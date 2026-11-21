use std::{
    fs::{File, OpenOptions},
    io::{self, BufWriter, Write},
    sync::{Mutex, OnceLock},
    time::Instant,
};
pub(crate) fn is_open() -> bool {
    logger().lock().unwrap().writer.is_some()
}

struct LogState {
    writer: Option<BufWriter<File>>,
    pending: Vec<String>,
    error: Option<String>,
    last_flush: Instant,
}

fn logger() -> &'static Mutex<LogState> {
    static LOGGER: OnceLock<Mutex<LogState>> = OnceLock::new();
    LOGGER.get_or_init(|| {
        Mutex::new(LogState {
            writer: None,
            pending: Vec::new(),
            error: None,
            last_flush: Instant::now(),
        })
    })
}

fn timestamp() -> String {
    let time = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        time.wMilliseconds
    )
}

pub(crate) fn open_log(path: &str) -> io::Result<()> {
    let _operation = crate::watchdog::operation("open recovery log");
    let mut writer = append_writer(path)?;
    writeln!(
        writer,
        "\n--- sd2img {} | {} | build={} ---",
        env!("CARGO_PKG_VERSION"),
        timestamp(),
        env!("SD2IMG_BUILD")
    )?;
    let mut state = logger().lock().unwrap();
    for line in state.pending.drain(..) {
        writeln!(writer, "{line}")?;
    }
    writer.flush()?;
    state.writer = Some(writer);
    Ok(())
}

fn append_writer(path: &str) -> io::Result<BufWriter<File>> {
    Ok(BufWriter::new(
        OpenOptions::new().create(true).append(true).open(path)?,
    ))
}

pub(crate) fn append(message: &str) {
    let _operation = crate::watchdog::operation("append recovery log");
    let mut state = logger().lock().unwrap();
    for message in message.lines().filter(|line| !line.is_empty()) {
        let line = format!("[{}] {message}", timestamp());
        if let Some(writer) = &mut state.writer {
            if let Err(error) = writeln!(writer, "{line}") {
                state.error = Some(error.to_string());
            }
        } else if state.pending.len() < 1024 {
            state.pending.push(line);
        }
    }
    if state.last_flush.elapsed().as_secs() >= 1 {
        if let Some(writer) = &mut state.writer {
            if let Err(error) = writer.flush() {
                state.error = Some(error.to_string());
            }
        }
        state.last_flush = Instant::now();
    }
}

pub(crate) fn flush_log() -> io::Result<()> {
    let _operation = crate::watchdog::operation("flush recovery log");
    let mut state = logger().lock().unwrap();
    if let Some(writer) = &mut state.writer {
        writer.flush()?;
    }
    if let Some(error) = &state.error {
        return Err(io::Error::other(message!(
            "Cannot write the rescue log: {error}",
            "救出ログを書き込めません: {error}"
        )));
    }
    Ok(())
}

pub(crate) fn check_log() -> io::Result<()> {
    let state = logger().lock().unwrap();
    match &state.error {
        Some(error) => Err(io::Error::other(message!(
            "Cannot write the rescue log: {error}",
            "救出ログを書き込めません: {error}"
        ))),
        None => Ok(()),
    }
}

pub(crate) fn take_pending() -> Vec<String> {
    std::mem::take(&mut logger().lock().unwrap().pending)
}

pub(crate) fn append_forwarded(line: &str) {
    let _operation = crate::watchdog::operation("append worker log");
    let mut state = logger().lock().unwrap();
    if let Some(writer) = &mut state.writer {
        if let Err(error) = writeln!(writer, "{line}") {
            state.error = Some(error.to_string());
        }
    } else if state.pending.len() < 1024 {
        state.pending.push(line.to_owned());
    }
}

pub(crate) fn emergency_append(message: String) {
    // A blocked filesystem/logger must not block the watchdog itself.
    let (sender, receiver) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("sd2img-emergency-log".to_owned())
        .spawn(move || {
            if let Ok(mut state) = logger().try_lock() {
                if let Some(writer) = &mut state.writer {
                    let _ = writeln!(writer, "[{}] {message}", timestamp());
                    let _ = writer.flush();
                }
            }
            let _ = sender.send(());
        });
    let _ = receiver.recv_timeout(std::time::Duration::from_millis(200));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn later_sessions_append_without_truncating_existing_logs() -> io::Result<()> {
        let path = std::env::temp_dir().join(format!("sd2img-log-{}.log", std::process::id()));
        std::fs::write(&path, "existing session\n")?;
        for line in ["first appended session", "second appended session"] {
            let mut writer = append_writer(path.to_str().unwrap())?;
            writeln!(writer, "{line}")?;
            writer.flush()?;
        }
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "existing session\nfirst appended session\nsecond appended session\n"
        );
        std::fs::remove_file(path)
    }
}
