use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::BOOL;
use windows::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};

pub(crate) static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

pub(crate) unsafe extern "system" fn console_handler(event: u32) -> BOOL {
    if event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT {
        STOP_REQUESTED.store(true, Ordering::SeqCst);
        BOOL(1)
    } else {
        BOOL(0)
    }
}

pub(crate) fn stopping() -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst)
}

pub(crate) fn interruptible_sleep(duration: Duration) {
    let start = Instant::now();
    while !stopping() && start.elapsed() < duration {
        std::thread::sleep(
            (duration - start.elapsed().min(duration)).min(Duration::from_millis(50)),
        );
    }
}
