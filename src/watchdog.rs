//! Non-destructive supervision: report slow native operations without terminating the run.
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

static OPERATION_MILLIS: AtomicU64 = AtomicU64::new(10_000);
pub(crate) fn configure(duration: Duration) {
    OPERATION_MILLIS.store((duration.as_millis() as u64).max(1), Ordering::Relaxed);
}
pub(crate) fn operation_timeout() -> Duration {
    Duration::from_millis(OPERATION_MILLIS.load(Ordering::Relaxed))
}
struct Operation {
    id: u64,
    name: &'static str,
    deadline: Instant,
    reported: bool,
}
fn operations() -> &'static Mutex<Vec<Operation>> {
    static OPERATIONS: OnceLock<Mutex<Vec<Operation>>> = OnceLock::new();
    OPERATIONS.get_or_init(|| Mutex::new(Vec::new()))
}
pub(crate) struct Guard(u64);
impl Drop for Guard {
    fn drop(&mut self) {
        operations().lock().unwrap().retain(|op| op.id != self.0);
    }
}
pub(crate) fn operation(name: &'static str) -> Guard {
    timed(name, operation_timeout())
}
pub(crate) fn timed(name: &'static str, limit: Duration) -> Guard {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    operations().lock().unwrap().push(Operation {
        id,
        name,
        deadline: Instant::now() + limit,
        reported: false,
    });
    Guard(id)
}
pub(crate) struct Supervisor {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Supervisor {
    pub(crate) fn start() -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::Builder::new().name("sd2img-watchdog".to_owned()).spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let (overdue, notices) = {
                    let mut active = operations().lock().unwrap();
                    let now = Instant::now(); let mut notices = Vec::new();
                    for operation in active.iter_mut().filter(|op| now >= op.deadline) {
                        if !operation.reported { operation.reported = true; notices.push(operation.name); }
                    }
                    (active.iter().find(|op| now >= op.deadline).map(|op| op.name), notices)
                };
                crate::visual::pending_operation(overdue);
                for name in notices {
                    let text = message!("Operation is still pending: {name}. Keeping the operation alive and waiting for completion.",
                        "処理の完了通知を待っています: {name}。強制停止せず、完了後に続行します。");
                    crate::visual::event(&text, crate::output::Tone::Warning);
                    crate::logging::emergency_append(text);
                }
                thread::sleep(Duration::from_millis(100));
            }
        })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completed_operations_are_removed_from_monitoring() {
        let guard = operation("completed test operation");
        let id = guard.0;
        assert!(operations().lock().unwrap().iter().any(|op| op.id == id));
        drop(guard);
        assert!(!operations().lock().unwrap().iter().any(|op| op.id == id));
    }
}
