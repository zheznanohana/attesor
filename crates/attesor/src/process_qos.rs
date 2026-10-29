//! Only dedicated translation workers may lower their own QoS. Never call this
//! from a guest thread: a synchronous exit/failed-exec save can return to it.

const BACKGROUND: u32 = 0x09;

extern "C" {
    fn pthread_set_qos_class_self_np(qos: u32, relative_priority: i32) -> i32;
    fn qos_class_self() -> u32;
}

fn selected(on: &str, off: &str) -> bool {
    on.split(',').any(|s| s == "process-qos")
        && !off.split(',').any(|s| s == "process-qos")
}

/// A one-way change: the caller must never subsequently execute guest code.
pub fn background_translation_worker(role: &str) {
    if !selected(&std::env::var("ATTESOR_JIT_ON").unwrap_or_default(),
                 &std::env::var("ATTESOR_JIT_OFF").unwrap_or_default()) {
        return;
    }
    let (result, actual, bg_result, bg) = lower_current_worker();
    if std::env::var_os("ATTESOR_STATS").is_some() {
        let message = format!("[attesor] process-qos: role={role} pid={} qos_result={result} qos={actual:#x} darwin_result={bg_result} darwin_bg={bg}\n", std::process::id());
        // The detached builder has just forked. Do not acquire Rust's stderr
        // lock, which could have belonged to a vanished guest thread.
        unsafe { libc::write(2, message.as_ptr().cast(), message.len()); }
    }
}

fn lower_current_worker() -> (i32, u32, i32, i32) {
    // Per-thread policy only, never PRIO_PROCESS/task_policy: neither parent
    // nor sibling guest threads inherit a process-wide priority change.
    let result = unsafe { pthread_set_qos_class_self_np(BACKGROUND, 0) };
    // A guest can leave libpthread's host thread opted out of QoS, observed as
    // EPERM in the exit builder. Darwin's current-thread background policy also
    // lowers CPU/IO/network priority and does not affect siblings or the parent.
    let (bg_result, bg) = if result != 0 { darwin_background() }
        else { (0, unsafe { libc::getpriority(libc::PRIO_DARWIN_THREAD, 0) }) };
    (result, unsafe { qos_class_self() }, bg_result, bg)
}

fn darwin_background() -> (i32, i32) {
    unsafe {
        (libc::setpriority(libc::PRIO_DARWIN_THREAD, 0, libc::PRIO_DARWIN_BG),
         libc::getpriority(libc::PRIO_DARWIN_THREAD, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_opt_in_and_off_precedence() {
        assert!(!selected("", ""));
        assert!(!selected("not-process-qos,process-qos-extra", ""));
        assert!(selected("other,process-qos", ""));
        assert!(!selected("process-qos", "other,process-qos"));
    }

    #[test]
    fn worker_changes_but_parent_and_sibling_do_not() {
        let parent = unsafe { qos_class_self() };
        let parent_bg = unsafe { libc::getpriority(libc::PRIO_DARWIN_THREAD, 0) };
        let (tx, rx) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let sibling = std::thread::spawn(move || {
            let before = unsafe { qos_class_self() };
            let before_bg = unsafe { libc::getpriority(libc::PRIO_DARWIN_THREAD, 0) };
            tx.send(before).unwrap();
            wait.recv().unwrap();
            assert_eq!(unsafe { qos_class_self() }, before);
            assert_eq!(unsafe { libc::getpriority(libc::PRIO_DARWIN_THREAD, 0) }, before_bg);
        });
        rx.recv().unwrap();
        let worker = std::thread::spawn(|| {
            let (result, actual, _, _) = lower_current_worker();
            assert_eq!((result, actual), (0, 0x09));
        });
        worker.join().unwrap();
        std::thread::spawn(|| {
            assert_eq!(darwin_background(), (0, 1));
        }).join().unwrap();
        assert_eq!(unsafe { qos_class_self() }, parent);
        assert_eq!(unsafe { libc::getpriority(libc::PRIO_DARWIN_THREAD, 0) }, parent_bg);
        release.send(()).unwrap();
        sibling.join().unwrap();
    }
}
