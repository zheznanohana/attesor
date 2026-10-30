//! Only dedicated translation workers may lower their own QoS. Never call this
//! from a guest thread: a synchronous exit/failed-exec save can return to it.

const WORKER_QOS: u32 = 0x15; // QOS_CLASS_DEFAULT: bounded cap, no Darwin BG

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
    // A user-awaited prewarm is not background work. Zero policy API calls
    // (even diagnostic readbacks) on this path, regardless of the token.
    if role == "prewarm" { return; }
    if !selected(&std::env::var("ATTESOR_JIT_ON").unwrap_or_default(),
                 &std::env::var("ATTESOR_JIT_OFF").unwrap_or_default()) {
        return;
    }
    // Only detached exit/exec-save builders get the best-effort QoS cap.
    let (result, actual, bg_result, bg) = lower_current_worker();
    if std::env::var_os("ATTESOR_STATS").is_some() {
        let message = format!("[attesor] process-qos: role={role} pid={} qos_result={result} qos={actual:#x} darwin_result={bg_result} darwin_bg={bg} policy=default\n", std::process::id());
        // The detached builder has just forked. Do not acquire Rust's stderr
        // lock, which could have belonged to a vanished guest thread.
        unsafe { libc::write(2, message.as_ptr().cast(), message.len()); }
    }
}

fn lower_current_worker() -> (i32, u32, i32, i32) {
    // Per-thread policy only. PRIO_DARWIN_BG is NOT a substitute for QoS:
    // it throttles CPU and disk/network IO, making short AOT jobs much slower.
    // A host thread opted out of pthread QoS can return EPERM after guest
    // execution. Leave that worker unchanged instead of imposing a stronger,
    // unrelated background policy. Never touch the parent or guest siblings.
    let before = unsafe { qos_class_self() };
    // Do not promote an already lower-priority worker either.
    let result = if before != 0 && before <= WORKER_QOS { 0 }
        else { unsafe { pthread_set_qos_class_self_np(WORKER_QOS, 0) } };
    (result, unsafe { qos_class_self() }, 0,
     unsafe { libc::getpriority(libc::PRIO_DARWIN_THREAD, 0) })
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
    fn waited_prewarm_inherits_policy() {
        std::thread::spawn(|| {
            let before = unsafe { qos_class_self() };
            let bg = unsafe { libc::getpriority(libc::PRIO_DARWIN_THREAD, 0) };
            background_translation_worker("prewarm");
            assert_eq!(unsafe { qos_class_self() }, before);
            assert_eq!(unsafe { libc::getpriority(libc::PRIO_DARWIN_THREAD, 0) }, bg);
        }).join().unwrap();
    }

    #[test]
    fn lower_priority_is_never_promoted() {
        std::thread::spawn(|| {
            assert_eq!(unsafe { pthread_set_qos_class_self_np(0x11, 0) }, 0);
            assert_eq!(lower_current_worker(), (0, 0x11, 0, 0));
        }).join().unwrap();
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
            let (result, actual, _, bg) = lower_current_worker();
            assert_eq!((result, actual), (0, WORKER_QOS));
            assert_eq!(bg, 0);
        });
        worker.join().unwrap();
        assert_eq!(unsafe { qos_class_self() }, parent);
        assert_eq!(unsafe { libc::getpriority(libc::PRIO_DARWIN_THREAD, 0) }, parent_bg);
        release.send(()).unwrap();
        sibling.join().unwrap();
    }
}
