//! Bounded speculative translation. The worker sees only owned guest bytes and
//! produces unpublished, position-independent bytes, never guest/host pointers.
//! No worker path takes TRANSLATE, INDEX, BLOCKS, AOT/profile or guest CPU locks.

use super::emit::{self, StubKind, Unit};
use iced_x86::{BlockEncoderOptions, Encoder};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

pub(super) const PAGE_BYTES: usize = 4096;
const QUEUE_LIMIT: usize = 8;
const JOB_LIMIT: usize = 256;
const UNIT_BYTES: usize = 64 * 1024;
const BYTE_BUDGET: usize = 2 * 1024 * 1024;

// Successful fork never overlaps background emission or worker initialization.
// Dispatcher adoption never takes this gate. Guest fork pauses new jobs, then
// waits for at most the current bounded emission; async AOT fork only tries it.
static EMISSION: Mutex<()> = Mutex::new(());
static PAUSED: AtomicUsize = AtomicUsize::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static STOPPED: AtomicBool = AtomicBool::new(false);
static OWNER: AtomicU32 = AtomicU32::new(0);
static RUNTIME: OnceLock<Option<Arc<Runtime>>> = OnceLock::new();

struct Job {
    pc: u64,
    generation: u64,
    page: u64,
    source: Vec<u8>,
    commpage: bool,
}

pub(super) struct Staged {
    pub pc: u64,
    pub generation: u64,
    pub page: u64,
    pub source: Vec<u8>,
    pub commpage: bool,
    pub code: Vec<u8>,
    /// (slot, continuation), both offsets from the future aligned code base.
    pub slots: Vec<(u64, u64)>,
    pub insns: u64,
    pub fallback: u64,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    ready: Vec<Staged>,
    // Includes failed and already consumed jobs: no repeated speculative work.
    seen: HashSet<(u64, u64)>,
    produced_bytes: usize,
}
struct Runtime {
    state: Mutex<State>,
    wake: Condvar,
}

fn live() -> bool {
    let owner = OWNER.load(Ordering::Acquire);
    !STOPPED.load(Ordering::Acquire) && (owner == 0 || owner == std::process::id())
}

pub(super) fn enabled() -> bool {
    if !live() { return false; }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let token = |name| std::env::var(name).unwrap_or_default()
            .split(',').any(|s| s == "bgtrans");
        token("ATTESOR_JIT_ON") && !token("ATTESOR_JIT_OFF")
            && !super::verify_mode()
            && !super::loops::census_enabled()
            // Keep diagnostic translation order and guest-reading logging on
            // the foreground path, even when bgtrans was explicitly requested.
            && !["ATTESOR_LOOPRA_BISECT", "ATTESOR_LOOPRA_LOG", "ATTESOR_LOOPRA_ALLOW"]
                .iter().any(|k| std::env::var_os(k).is_some())
    })
}

/// Initialize all emitter/encoder lazy state on a guest runtime thread, before
/// spawning. A forked child must not inherit a lazy initializer owned by a worker
/// that no longer exists. This is setup, not execution of generated guest code.
pub(super) fn start() {
    if !enabled() { return; }
    let Ok(_emission) = EMISSION.try_lock() else { return; };
    if PAUSED.load(Ordering::SeqCst) != 0 { return; }
    let _ = RUNTIME.get_or_init(|| {
        emit::prepare_bgtrans();
        let _ = Encoder::new(64);
        OWNER.store(std::process::id(), Ordering::Release);
        let r = Arc::new(Runtime { state: Mutex::new(State::default()), wake: Condvar::new() });
        let worker = Arc::clone(&r);
        std::thread::Builder::new().name("attesor-bgtrans".into())
            .spawn(move || work(worker)).ok().map(|_| r)
    });
}

/// Full, fallible VM copy: neither permission probes nor subsequent bare guest
/// pointer loads. Check the returned length as well as the Mach return code.
pub(super) fn snapshot(page: u64) -> Option<Vec<u8>> {
    extern "C" {
        fn mach_task_self() -> u32;
        fn mach_vm_read_overwrite(task: u32, addr: u64, size: u64, data: u64, out: *mut u64) -> i32;
    }
    page.checked_add(PAGE_BYTES as u64)?;
    let mut bytes = vec![0u8; PAGE_BYTES];
    let mut copied = 0;
    let ok = unsafe { crate::profile::host_call(|| {
        mach_vm_read_overwrite(mach_task_self(), page, PAGE_BYTES as u64,
            bytes.as_mut_ptr() as u64, &mut copied)
    }) };
    (ok == 0 && copied == PAGE_BYTES as u64).then_some(bytes)
}

/// Caller holds TRANSLATE, but not INDEX/BLOCKS. At most two direct exit targets
/// from an on-demand foreground unit; no recursive speculative traversal.
pub(super) fn enqueue(targets: &[u64], generation: u64, commpage: bool) {
    if !enabled() { return; }
    let Some(Some(r)) = RUNTIME.get() else { return; };
    for &pc in targets.iter().take(2) {
        if pc & 3 != 0 || super::lookup(pc).is_some() { continue; }
        let Ok(mut s) = r.state.try_lock() else { return; };
        s.queue.retain(|j| j.generation == generation);
        s.ready.retain(|u| u.generation == generation);
        if s.seen.len() >= JOB_LIMIT || s.queue.len() >= QUEUE_LIMIT
            || s.ready.len() >= QUEUE_LIMIT || s.produced_bytes >= BYTE_BUDGET
            || !s.seen.insert((generation, pc)) { continue; }
        drop(s);
        let page = pc & !(PAGE_BYTES as u64 - 1);
        let Some(source) = snapshot(page) else { continue; };
        // Only a complete single basic block in this page is eligible. All
        // emitter lookahead is also constrained to these owned page bytes.
        let offset = (pc - page) as usize;
        let complete = source[offset..].chunks_exact(4).take(emit::MAX_BLOCK_INSNS)
            .any(|w| emit::is_terminator(u32::from_le_bytes(w.try_into().unwrap())));
        if !complete { continue; }
        let Ok(mut s) = r.state.try_lock() else { continue; };
        if !live() || s.queue.len() >= QUEUE_LIMIT { continue; }
        s.queue.push_back(Job { pc, generation, page, source, commpage });
        r.wake.notify_one();
    }
}

/// Foreground never waits for either the worker or its queue mutex.
pub(super) fn take(pc: u64) -> Option<Staged> {
    if !enabled() { return None; }
    let r = RUNTIME.get()?.as_ref()?;
    let mut s = r.state.try_lock().ok()?;
    let i = s.ready.iter().position(|u| u.pc == pc)?;
    Some(s.ready.swap_remove(i))
}

fn build(job: Job) -> Option<Staged> {
    let read = |pc: u64| {
        if pc & 3 != 0 { return None; }
        let offset = usize::try_from(pc.checked_sub(job.page)?).ok()?;
        let bytes = job.source.get(offset..offset.checked_add(4)?)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    };
    let emit = |padding| {
        let mut u = Unit::new(StubKind::Patchable, job.commpage);
        // A deliberately limited contract: no inline call continuations,
        // private loop regions or internal branches. In particular this avoids
        // loopra_bisect's diagnostic raw guest reads from the worker.
        u.inline_ok = false;
        u.no_internal_jumps = true;
        u.slot_padding = padding;
        let label = u.a.create_label();
        u.labels.insert(job.pc, label);
        emit::emit_block(&mut u, job.pc, &read);
        u.finish();
        u
    };
    let options = BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS
        | BlockEncoderOptions::RETURN_RELOC_INFOS;
    let mut u = emit(0);
    // All branches and RIP operands address labels in this unit; no host address
    // is embedded. Absolute slot continuations are deliberately NOT set here.
    let mut res = u.assemble_options(0, options).ok()?;
    if let Some(label) = u.first_slot {
        let at = res.label_ip(&label).ok()?;
        if at % 8 != 0 {
            u = emit((8 - at % 8) as usize);
            res = u.assemble_options(0, options).ok()?;
        }
    }
    let len = res.inner.code_buffer.len() as u64;
    if len == 0 || len > UNIT_BYTES as u64 || !res.inner.reloc_infos.is_empty()
        || u.labels.len() != 1 { return None; }
    let mut slots = Vec::new();
    for stub in &u.finished_stubs {
        let slot = res.label_ip(&stub.slot).ok()?;
        let cont = res.label_ip(&stub.cont).ok()?;
        if slot % 8 != 0 || slot.checked_add(8)? > len || cont >= len { return None; }
        slots.push((slot, cont));
    }
    Some(Staged { pc: job.pc, generation: job.generation, page: job.page,
        source: job.source, commpage: job.commpage, code: res.inner.code_buffer,
        slots, insns: u.insns_translated, fallback: u.insns_fallback })
}

fn work(r: Arc<Runtime>) {
    extern "C" { fn pthread_set_qos_class_self_np(class: u32, relative_priority: i32) -> i32; }
    // Do not silently run speculative compilation at the default UI QoS.
    if unsafe { pthread_set_qos_class_self_np(0x09, 0) } != 0 {
        STOPPED.store(true, Ordering::Release);
        return;
    }
    loop {
        let job = {
            let Ok(mut s) = r.state.lock() else { return; };
            while s.queue.is_empty() && live() {
                let Ok(next) = r.wake.wait(s) else { return; };
                s = next;
            }
            if !live() { return; }
            s.queue.pop_front().unwrap()
        };
        if job.generation != generation() { continue; }
        let result = {
            let Ok(_emission) = EMISSION.try_lock() else { continue; };
            if !live() { return; }
            if PAUSED.load(Ordering::SeqCst) != 0 { continue; }
            build(job)
        };
        if !live() { return; }
        let Ok(mut s) = r.state.lock() else { return; };
        if let Some(unit) = result {
            if unit.generation == generation() && s.ready.len() < QUEUE_LIMIT
                && s.produced_bytes + unit.code.len() <= BYTE_BUDGET {
                s.produced_bytes += unit.code.len();
                s.ready.push(unit);
            }
        }
    }
}

/// Exit never joins a low-priority worker. A currently emitting unit is discarded.
pub(super) fn stop() {
    STOPPED.store(true, Ordering::Release);
    // A child must never touch a vanished worker's inherited queue primitives.
    if OWNER.load(Ordering::Acquire) != std::process::id() { return; }
    if let Some(Some(r)) = RUNTIME.get() {
        if let Ok(mut s) = r.state.try_lock() {
            s.queue.clear();
            s.ready.clear();
        }
        r.wake.notify_all();
    }
}

/// Called immediately in every libc fork child, before any background API.
/// Do NOT touch an inherited mutex/Condvar/OnceLock: its owning thread may have
/// vanished. Child foreground translation remains available; background work is
/// disabled for the rest of that child process. No shutdown join is required.
pub(super) fn after_fork_child() {
    STOPPED.store(true, Ordering::Release);
}

/// Separate from execution EPOCH: even an invalidation with no published victim
/// must cancel speculative input. Caller holds TRANSLATE before changing it.
pub(super) fn invalidate() {
    if enabled() { GENERATION.fetch_add(1, Ordering::SeqCst); }
}

pub(super) fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// The gate is dropped in both fork branches. Only the calling guest-fork
/// thread pauses the worker; no queue lock is held while waiting for emission.
pub(super) struct ForkGuard {
    _emission: Option<MutexGuard<'static, ()>>,
    paused: bool,
}
impl Drop for ForkGuard {
    fn drop(&mut self) {
        // Release the gate before allowing another worker job to start.
        self._emission.take();
        if self.paused { PAUSED.fetch_sub(1, Ordering::SeqCst); }
    }
}

pub(super) fn fork_guard(wait: bool) -> Result<ForkGuard, ()> {
    if OWNER.load(Ordering::Acquire) == 0 && !enabled() {
        return Ok(ForkGuard { _emission: None, paused: false });
    }
    if wait {
        PAUSED.fetch_add(1, Ordering::SeqCst);
        // Preserve guest fork semantics rather than inventing EAGAIN because a
        // speculative worker happened to be busy. This can incur scheduling
        // delay, despite a single page / 96-insn emission bound. Never called
        // by dispatcher adoption, and never holds the worker queue mutex.
        match EMISSION.lock() {
            Ok(guard) => Ok(ForkGuard { _emission: Some(guard), paused: true }),
            Err(_) => {
                PAUSED.fetch_sub(1, Ordering::SeqCst);
                Err(())
            }
        }
    } else {
        EMISSION.try_lock().map(|guard| ForkGuard {
            _emission: Some(guard), paused: false,
        }).map_err(|_| ())
    }
}

#[cfg(test)]
pub(super) fn ready_for_test(pc: u64) -> bool {
    RUNTIME.get().and_then(|r| r.as_ref()).is_some_and(|r|
        r.state.lock().unwrap().ready.iter().any(|s| s.pc == pc))
}

#[cfg(test)]
pub(super) fn stage_for_test(pc: u64, generation: u64, commpage: bool) {
    let page = pc & !(PAGE_BYTES as u64 - 1);
    let staged = build(Job { pc, generation, page, source: snapshot(page).unwrap(), commpage }).unwrap();
    RUNTIME.get().unwrap().as_ref().unwrap().state.lock().unwrap().ready.push(staged);
}

#[cfg(test)]
pub(super) fn emission_for_test() -> MutexGuard<'static, ()> { EMISSION.lock().unwrap() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_failures_and_owned_staging_rebases() {
        assert!(snapshot(0).is_none());
        assert!(snapshot(u64::MAX - PAGE_BYTES as u64 + 1).is_none());
        let p = unsafe { libc::mmap(std::ptr::null_mut(), PAGE_BYTES * 2,
            libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
        assert_ne!(p, libc::MAP_FAILED);
        assert_eq!(unsafe { libc::mprotect((p as *mut u8).add(PAGE_BYTES).cast(), PAGE_BYTES, libc::PROT_NONE) }, 0);
        assert!(snapshot(p as u64 + PAGE_BYTES as u64 / 2).is_none(), "partial VM copy must not pass");
        let page = 0x2000_0000;
        let mut source = vec![0u8; PAGE_BYTES];
        source[..8].copy_from_slice(&[0x91000400u32.to_le_bytes(), 0x14000001u32.to_le_bytes()].concat());
        let staged = build(Job { pc: page, generation: 0, page, source, commpage: false }).unwrap();
        assert!(!staged.slots.is_empty());
        for &n in &[0, 256] {
            let cc = super::super::code_cache();
            let _gap = cc.alloc(n);
            let at = cc.alloc(staged.code.len() as u64);
            unsafe { std::ptr::copy_nonoverlapping(staged.code.as_ptr(), at as *mut u8, staged.code.len()); }
            for &(slot, cont) in &staged.slots {
                assert_eq!((at + slot) % 8, 0);
                unsafe { *((at + slot) as *mut u64) = at + cont; }
            }
            let mut cpu = attesor_core::cpu::Cpu::new(); super::super::setup_cpu(&mut cpu);
            cpu.x[0] = 41; cpu.set_pc(page);
            unsafe { super::super::attesor_jit_enter(&mut *cpu, at); }
            assert_eq!((cpu.x[0], cpu.pc(), cpu.exit_reason), (42, page + 8, emit::EXIT_CONTINUE));
        }
        // The staging reader cannot spill to a second/unowned page.
        let source = vec![0u8; PAGE_BYTES];
        let outside = build(Job { pc: page + PAGE_BYTES as u64, generation: 0, page, source, commpage: false }).unwrap();
        assert_eq!(outside.insns, 0, "out-of-snapshot reader must not read live guest text");
        unsafe { libc::munmap(p, PAGE_BYTES * 2); }
    }
}
