#[macro_use]
mod i18n;
mod geometry;
mod image;
mod read_lanes;
mod read_size;
mod sector;
mod watchdog;
#[macro_use]
mod output;
mod cli;
mod control;
mod device;
mod logging;
// Retain legacy filesystem-parser tests; current scheduling uses fast neighbors.
#[cfg(test)]
#[allow(dead_code)]
mod metadata;
mod probes;
mod rescue_map;
mod reset;
mod scan_order;
mod scanner;
mod visual;
mod volumes;
mod windows_native;

#[cfg(test)]
const SECTOR_SIZE: usize = 512;
#[cfg(test)]
const CHUNK_BYTES: usize = 64 * 1024;

fn main() {
    if let Some(code) = reset::worker_entry() {
        std::process::exit(code);
    }
    output::init();
    let args = cli::Args::parse_localized();
    let image = args.out.clone();
    let mut acknowledged = false;
    let result = execute(args, &mut acknowledged);
    if let Err(error) = result {
        let text = if logging::is_open() {
            message!(
                "sd2img stopped: {error} | Log: {image}.log",
                "sd2imgは停止しました: {error} | ログ: {image}.log"
            )
        } else {
            message!("sd2img stopped: {error}", "sd2imgは停止しました: {error}")
        };
        output::final_line(&text, output::Tone::Error);
        let _ = output::flush_log();
        if !acknowledged {
            wait_for_exit();
        }
        std::process::exit(1);
    }
    let text = if control::stopping() {
        message!(
            "sd2img stopped. Output: {image} | Log: {image}.log",
            "sd2imgを中断しました。出力先: {image} | ログ: {image}.log"
        )
    } else {
        message!(
            "sd2img completed. Image: {image} | Log: {image}.log",
            "sd2imgが完了しました。イメージ: {image} | ログ: {image}.log"
        )
    };
    output::final_line(&text, output::Tone::Success);
    let _ = output::flush_log();
}

fn wait_for_exit() {
    use std::io::{IsTerminal, Write};
    if std::io::stdin().is_terminal() {
        let prompt = tr!(
            "Press Enter to exit.",
            "終了するにはEnterを押してください。"
        );
        visual::event(prompt, output::Tone::Warning);
        if !visual::active() {
            eprintln!("{prompt}");
        }
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    } else {
        // Unattended runs also remain alive until an explicit stop request.
        while !control::stopping() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

fn execute(args: cli::Args, acknowledged: &mut bool) -> std::io::Result<()> {
    unsafe {
        windows::Win32::System::Console::SetConsoleCtrlHandler(Some(control::console_handler), true)
    }
    .map_err(std::io::Error::other)?;
    if !args.operation_timeout.is_finite()
        || args.operation_timeout <= 0.0
        || args.operation_timeout > 3600.0
    {
        return Err(std::io::Error::other(tr!(
            "operation-timeout must be positive and at most 3600 seconds",
            "operation-timeoutは0より大きく3600秒以下にしてください"
        )));
    }
    watchdog::configure(std::time::Duration::from_secs_f64(args.operation_timeout));
    device::configure_parallel_limit(args.max_parallel_reads as usize);
    windows_native::ensure_elevated()?;
    let _supervisor = watchdog::Supervisor::start()?;
    let screen = visual::Screen::start()?;
    let mut result = scanner::run(args);
    reset::drain_pending();
    if let Err(error) = &result {
        output::error(&error.to_string());
    }
    if let Err(error) = output::flush_log() {
        if result.is_ok() {
            result = Err(error);
        }
    }
    visual::finished(if result.is_err() {
        tr!("Stopped with an error", "エラーで停止")
    } else if control::stopping() {
        tr!("Stopped", "中断")
    } else {
        tr!("Completed", "完了")
    });
    if !control::stopping() {
        wait_for_exit();
    }
    *acknowledged = true;
    drop(screen);
    result
}
