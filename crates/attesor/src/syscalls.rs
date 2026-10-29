//! Guest system call emulation.
//!
//! arm64 Darwin: `svc #0x80` with the number in x16 (positive: BSD, negative:
//! Mach trap, 0x80000000: platform call selected by x3). Arguments in x0..x7.
//! BSD results go in x0/x1 with the carry flag reporting errors.
//! Most calls pass straight through to the host kernel.

use crate::cache;
use crate::hostsys::{self, SysResult};
use crate::threads;
use attesor_core::cpu::Cpu;
use std::sync::atomic::{AtomicBool, Ordering};

pub static TRACE: AtomicBool = AtomicBool::new(false);

const PLATFORM_SYSCALL: i32 = 0x8000_0000u32 as i32;

// BSD numbers we care about
const SYS_SYSCALL: u64 = 0;
const SYS_EXIT: u64 = 1;
const SYS_VFORK: u64 = 66;
const SYS_FORK: u64 = 2;
const SYS_SIGACTION: u64 = 46;
const SYS_SIGALTSTACK: u64 = 53;
const SYS_MPROTECT: u64 = 74;
const SYS_SIGRETURN: u64 = 184;
const SYS_MMAP: u64 = 197;
const SYS_MUNMAP: u64 = 73;
const SYS_SYSCTL: u64 = 202;
const SYS_SYSCTLBYNAME: u64 = 274;
const SYS_SHARED_REGION_CHECK_NP: u64 = 294;
const SYS_BSDTHREAD_CREATE: u64 = 360;
const SYS_BSDTHREAD_TERMINATE: u64 = 361;
const SYS_BSDTHREAD_REGISTER: u64 = 366;
const SYS_WORKQ_OPEN: u64 = 367;
const SYS_WORKQ_KERNRETURN: u64 = 368;
const SYS_SHARED_REGION_MAP_AND_SLIDE_2_NP: u64 = 536;
const SYS_MAP_WITH_LINKING_NP: u64 = 550;
const SYS_EXECVE: u64 = 59;
const SYS_POSIX_SPAWN: u64 = 244;

const PROT_EXEC: u64 = 4;
const MAP_JIT: u64 = 0x800;

#[inline]
fn set_ok(cpu: &mut Cpu, r0: u64, r1: u64) {
    cpu.x[0] = r0;
    cpu.x[1] = r1;
    cpu.cf = 0;
}
#[inline]
fn set_err(cpu: &mut Cpu, errno: u64) {
    cpu.x[0] = errno;
    cpu.cf = 1;
}
#[inline]
fn set_res(cpu: &mut Cpu, r: SysResult) {
    if r.carry {
        set_err(cpu, r.rax);
    } else {
        set_ok(cpu, r.rax, r.rdx);
    }
}

fn cstr_at(p: u64) -> String {
    if p == 0 {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p as *const libc::c_char).to_string_lossy().into_owned() }
}

/// Guest program name for per-process log files.
pub static GUEST_NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// ATTESOR_STDERR_LOG=1: copy everything the guest writes to fd 2 into
/// ~/Library/Logs/Attesor/stderr/<name>-<pid>.log (capped), so diagnostics of helpers whose stderr
/// a parent pipes away (and drops) survive.
fn tee_stderr(n: u64, args: &[u64; 8], written: u64) {
    const CAP: u64 = 4 << 20;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static FILE: std::sync::Mutex<Option<(std::fs::File, u64)>> = std::sync::Mutex::new(None);
    if !*ON.get_or_init(|| std::env::var_os("ATTESOR_STDERR_LOG").is_some()) || args[0] != 2 || written == 0 {
        return;
    }
    let mut g = FILE.lock().unwrap();
    if g.is_none() {
        let Some(home) = std::env::var_os("HOME") else { return };
        let dir = std::path::Path::new(&home).join("Library/Logs/Attesor/stderr");
        let _ = std::fs::create_dir_all(&dir);
        let name = GUEST_NAME.get().map(String::as_str).unwrap_or("guest").replace(['/', ' '], "_");
        let path = dir.join(format!("{name}-{}.log", std::process::id()));
        let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else { return };
        *g = Some((f, 0));
    }
    let Some((f, total)) = g.as_mut() else { return };
    if *total >= CAP {
        return;
    }
    use std::io::Write;
    let mut put = |p: u64, len: u64| {
        let len = len.min(CAP - *total);
        if p != 0 && len != 0 {
            let _ = f.write_all(unsafe { std::slice::from_raw_parts(p as *const u8, len as usize) });
            *total += len;
        }
    };
    match n {
        4 | 397 => put(args[1], written),
        // writev / writev_nocancel: walk the iovecs up to the byte count actually written
        121 | 412 => {
            let mut left = written;
            for i in 0..(args[2] as usize).min(1024) {
                if left == 0 {
                    break;
                }
                let (base, len) = unsafe { (*((args[1] + i as u64 * 16) as *const u64), *((args[1] + i as u64 * 16 + 8) as *const u64)) };
                let take = len.min(left);
                put(base, take);
                left -= take;
            }
        }
        _ => {}
    }
}

// Classify waiting operations, not all syscalls as idle. Nonblocking variants
// return quickly, so their incidental idle samples are negligible.
fn svc_state(cpu: &Cpu) -> crate::profile::State {
    use crate::profile::State;
    let num = cpu.x[16] as u32 as i32;
    if num == PLATFORM_SYSCALL { return State::Machdep; }
    if num < 0 {
        let n = num.wrapping_neg() as u32;
        // mach_msg[_overwrite]/mach_msg2: MACH_RCV_MSG = 2 in options (x1).
        return if (matches!(n, 31 | 32 | 47) && cpu.x[1] & 2 != 0)
            || matches!(n, 36 | 37 | 38 | 39 | 61 | 62 | 90) {
            State::Idle
        } else { State::Mach };
    }
    let (n, a) = if num == 0 { (cpu.x[0], &cpu.x[1..]) } else { (num as u64, &cpu.x[..]) };
    if n == crate::gpu::SYS_MCL_HOSTCALL { return State::Gpu; }
    // wait4, select/poll (+ nocancel), psynch waits, semwait, kevent,
    // ulock_wait/ulock_wait2, workqueue park/return, sigsuspend.
    if matches!(n, 7 | 93 | 111 | 230 | 297 | 298 | 300 | 301 | 305 | 306 | 307 | 334 | 363 | 369 | 374 | 375 | 394 | 395 | 400 | 407 | 410 | 417 | 423 | 515 | 544)
        || (n == SYS_WORKQ_KERNRETURN && matches!(a[0], 4 | 0x40 | 0x100)) {
        State::Idle
    } else { State::Bsd }
}

/// Handle SVC. `cpu.pc()` already points after the SVC.
pub fn handle_svc(cpu: &mut Cpu) {
    let _profile = if crate::profile::ENABLED.load(Ordering::Relaxed) {
        let num = cpu.x[16] as u32 as i32;
        let (class, number) = if num == PLATFORM_SYSCALL {
            (crate::profile::Class::Machdep, cpu.x[3] as u32)
        } else if num < 0 {
            (crate::profile::Class::Mach, num.wrapping_neg() as u32)
        } else {
            let n = if num == 0 { cpu.x[0] } else { num as u64 };
            if n == crate::gpu::SYS_MCL_HOSTCALL {
                (crate::profile::Class::Gpu, cpu.x[if num == 0 { 1 } else { 0 }] as u32)
            } else { (crate::profile::Class::Bsd, n as u32) }
        };
        Some(crate::profile::enter_call(svc_state(cpu), class, number))
    } else { None };
    let num = cpu.x[16] as u32 as i32;
    if num == PLATFORM_SYSCALL {
        match cpu.x[3] {
            2 => cpu.tpidrro_el0 = cpu.x[0],
            3 => cpu.x[0] = cpu.tpidrro_el0,
            _ => {}
        }
        return;
    }
    if num < 0 {
        mach_trap(cpu, num.wrapping_neg() as u32 as u64);
        return;
    }
    let mut n = num as u64;
    let mut args = [cpu.x[0], cpu.x[1], cpu.x[2], cpu.x[3], cpu.x[4], cpu.x[5], cpu.x[6], cpu.x[7]];
    if n == SYS_SYSCALL {
        n = args[0];
        args = [args[1], args[2], args[3], args[4], args[5], args[6], args[7], 0];
    }
    let trace = TRACE.load(Ordering::Relaxed);
    if trace {
        eprintln!("[sys] {}#{}({:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}) pc={:#x}", syscall_name(n), n, args[0], args[1], args[2], args[3], args[4], args[5], cpu.pc() - 4);
    }
    bsd(cpu, n, args);
    if matches!(n, 4 | 397 | 121 | 412) && cpu.cf == 0 {
        tee_stderr(n, &args, cpu.x[0]);
    }
    if trace {
        eprintln!("[sys]   -> {}{:#x}", if cpu.cf != 0 { "ERR " } else { "" }, cpu.x[0]);
    }
}

/// Guest paths of open fds under the redirected cache prefixes ("/", "/System", ...),
/// so `openat(dirfd, "rel")` walks (as dyld does) can be resolved to a full guest path.
static FD_VPATHS: std::sync::Mutex<Option<std::collections::HashMap<i32, String>>> = std::sync::Mutex::new(None);

fn vpath_join(base: &str, rel: &str) -> String {
    if base == "/" {
        format!("/{rel}")
    } else {
        format!("{}/{rel}", base.trim_end_matches('/'))
    }
}

fn bsd(cpu: &mut Cpu, n: u64, mut args: [u64; 8]) {
    // Path-taking syscalls: (path arg index, dirfd arg index for *at variants).
    let (pidx, at) = match n {
        5 | 398 => (Some(0), false),
        33 | 58 | 188 | 190 | 220 | 338 | 340 => (Some(0), false),
        463 | 464 | 469 | 470 => (Some(1), true),
        _ => (None, false),
    };
    let mut keep: Option<std::ffi::CString> = None;
    let mut guest_path: Option<String> = None;
    if let Some(i) = pidx {
        if args[i] != 0 {
            let orig = cstr_at(args[i]);
            let full = if orig.starts_with('/') {
                Some(orig.clone())
            } else if at {
                let dfd = args[0] as i32;
                FD_VPATHS.lock().unwrap().as_ref().and_then(|m| m.get(&dfd).map(|b| vpath_join(b, &orig)))
            } else {
                None
            };
            if TRACE.load(Ordering::Relaxed) {
                eprintln!("[path] #{n} {orig} -> {full:?}");
            }
            if let Some(full) = full {
                if let Some(new) = crate::paths::redirect(&full) {
                    if let Ok(c) = std::ffi::CString::new(new) {
                        args[i] = c.as_ptr() as u64;
                        if at {
                            args[0] = (-2i64) as u64; // AT_FDCWD; path is absolute now
                        }
                        keep = Some(c);
                    }
                }
                guest_path = Some(full);
            }
        }
    }
    bsd_inner(cpu, n, args);
    drop(keep);
    if matches!(n, 5 | 398 | 463 | 464) && cpu.cf == 0 {
        if let Some(g) = guest_path {
            if g == "/" || g.starts_with("/System") {
                let g = if g.len() > 1 { g.trim_end_matches('/').to_string() } else { g };
                FD_VPATHS.lock().unwrap().get_or_insert_with(Default::default).insert(cpu.x[0] as i32, g);
            }
        }
    } else if matches!(n, 6 | 399) {
        if let Some(m) = FD_VPATHS.lock().unwrap().as_mut() {
            m.remove(&(args[0] as i32));
        }
    }
}

fn bsd_inner(cpu: &mut Cpu, n: u64, mut args: [u64; 8]) {
    match n {
        crate::gpu::SYS_MCL_HOSTCALL => crate::gpu::hostcall(cpu),
        crate::native::SYS_NATIVE_CALL => crate::native::hostcall(cpu),
        SYS_VFORK | SYS_FORK => {
            // vfork shares our address space and stack: use fork instead. Go through libc's
            // fork (not the raw syscall) so the host libSystem re-initialises in the child
            // (mach_task_self, MIG reply ports, ...); the JIT reads guest memory through
            // mach_vm_read with the cached task port.
            let pid = crate::aot::fork_guest();
            if pid < 0 {
                set_err(cpu, unsafe { *libc::__error() } as u64);
            } else if pid == 0 {
                // Child: XNU returns the parent's pid with the "is child" flag in x1.
                set_ok(cpu, unsafe { crate::profile::host_call(|| libc::getppid()) } as u64, 1);
            } else {
                set_ok(cpu, pid as u64, 0);
            }
        }
        SYS_EXIT => {
            crate::engine::flush_profile();
            crate::gdb::on_exit(args[0] as i32);
            let r = hostsys::unix(SYS_EXIT, &args);
            set_res(cpu, r);
        }
        SYS_SIGACTION => {
            let r = crate::signals::sigaction(args[0] as i32, args[1], args[2]);
            match r {
                Ok(()) => set_ok(cpu, 0, 0),
                Err(e) => set_err(cpu, e as u64),
            }
        }
        SYS_SIGALTSTACK => {
            if args[1] != 0 {
                unsafe { std::ptr::write_bytes(args[1] as *mut u8, 0, 24) };
                unsafe { *((args[1] + 16) as *mut u32) = 4 }; // SS_DISABLE
            }
            set_ok(cpu, 0, 0);
        }
        SYS_SIGRETURN => {
            crate::signals::sigreturn(cpu, args[0], args[1] as i32);
        }
        SYS_MMAP => {
            let was_exec = args[2] & PROT_EXEC != 0;
            args[2] &= !PROT_EXEC;
            args[3] &= !MAP_JIT;
            let r = mmap_16k(&args);
            // dyld maps a library's __TEXT (file offset 0, executable) at its final address:
            // register it with the AOT cache so translations persist across runs.
            if !r.carry && was_exec && args[5] == 0 && (args[4] as i64) >= 0 && args[1] >= 0x1000 {
                crate::symbols::register_mapped_file(r.rax, args[4] as i32);
                crate::aot::register_image_at(r.rax);
            }
            set_res(cpu, r);
        }
        SYS_MPROTECT => {
            let exec = args[2] & PROT_EXEC != 0;
            args[2] &= !PROT_EXEC;
            let r = hostsys::unix(n, &args);
            if !r.carry && exec {
                crate::engine::invalidate_range(args[0], args[1]);
            }
            set_res(cpu, r);
        }
        SYS_SYSCTLBYNAME => {
            let name = cstr_at(args[0]);
            if name == "hw.machine" {
                write_sysctl_str(cpu, args[2], args[3], "arm64");
                return;
            }
            if let Some(v) = sysctl_override(&name) {
                write_sysctl_int(cpu, args[2], args[3], v);
                return;
            }
            let r = hostsys::unix(n, &args);
            set_res(cpu, r);
        }
        SYS_SYSCTL => {
            // hw.machine (CTL_HW=6, HW_MACHINE=1) is what uname() reports.
            let mib = unsafe { std::slice::from_raw_parts(args[0] as *const i32, (args[1] as usize).min(8)) };
            if args[1] == 2 && mib == [6, 1] {
                write_sysctl_str(cpu, args[2], args[3], "arm64");
                return;
            }
            // libc's sysctlbyname() resolves the name first (name2oid, mib {0,3}) and then queries the
            // OID, so overrides must answer both steps: overridden names get a private OID
            // {FAKE_OID, index}, which the second step maps back. Before, only SYS_SYSCTLBYNAME was
            // overridden, which arm64 libc never uses: every hw.optional.arm.* answered ENOENT and
            // hw.pagesize the host's 4096.
            if args[1] == 2 && mib == [0, 3] && args[4] != 0 {
                let len = (args[5] as usize).min(256);
                let raw = unsafe { std::slice::from_raw_parts(args[4] as *const u8, len) };
                let name = String::from_utf8_lossy(raw.split(|&b| b == 0).next().unwrap_or(&[])).into_owned();
                if name == "hw.machine" || sysctl_override(&name).is_some() {
                    let mut names = FAKE_OID_NAMES.lock().unwrap();
                    let idx = names.iter().position(|n| *n == name).unwrap_or_else(|| { names.push(name.clone()); names.len() - 1 });
                    unsafe {
                        if args[2] != 0 && args[3] != 0 && *(args[3] as *const u64) >= 8 {
                            *(args[2] as *mut [i32; 2]) = [FAKE_OID, idx as i32];
                            *(args[3] as *mut u64) = 8;
                        }
                    }
                    set_ok(cpu, 0, 0);
                    return;
                }
            }
            if args[1] == 2 && mib[0] == FAKE_OID {
                let name = FAKE_OID_NAMES.lock().unwrap().get(mib[1] as usize).cloned();
                match name.as_deref() {
                    Some("hw.machine") => write_sysctl_str(cpu, args[2], args[3], "arm64"),
                    Some(n) => write_sysctl_int(cpu, args[2], args[3], sysctl_override(n).unwrap_or(0)),
                    None => set_err(cpu, libc::ENOENT as u64),
                }
                return;
            }
            let r = hostsys::unix(n, &args);
            set_res(cpu, r);
        }
        SYS_SHARED_REGION_CHECK_NP => {
            // -1 (DYLD_VM_END_MWL) marks the end of map_with_linking; 0 just
            // refreshes the task's shared region.
            if args[0] == u64::MAX || args[0] == 0 {
                set_ok(cpu, 0, 0);
                return;
            }
            match cache::check_np() {
                Some(base) => {
                    if crate::guestmem::write_u64(args[0], base) {
                        set_ok(cpu, 0, 0);
                    } else {
                        set_err(cpu, libc::EFAULT as u64);
                    }
                }
                None => set_err(cpu, libc::ENOMEM as u64),
            }
        }
        SYS_SHARED_REGION_MAP_AND_SLIDE_2_NP => {
            let e = cache::map_and_slide_2(args[0] as u32, args[1], args[2] as u32, args[3]);
            if e == 0 {
                set_ok(cpu, 0, 0);
            } else {
                set_err(cpu, e as u64);
            }
        }
        SYS_MAP_WITH_LINKING_NP => set_err(cpu, libc::ENOTSUP as u64),
        SYS_BSDTHREAD_REGISTER => {
            let r = threads::bsdthread_register(&args);
            match r {
                Ok(v) => set_ok(cpu, v, 0),
                Err(e) => set_err(cpu, e),
            }
        }
        SYS_BSDTHREAD_CREATE => match threads::bsdthread_create(&args) {
            Ok(v) => set_ok(cpu, v, 0),
            Err(e) => set_err(cpu, e),
        },
        SYS_BSDTHREAD_TERMINATE => threads::bsdthread_terminate(cpu, &args),
        SYS_WORKQ_OPEN => set_ok(cpu, 0, 0),
        SYS_WORKQ_KERNRETURN => match crate::workq::kernreturn(cpu, &args) {
            Ok(v) => set_ok(cpu, v, 0),
            Err(e) => set_err(cpu, e),
        },
        SYS_EXECVE | SYS_POSIX_SPAWN => {
            if n == SYS_EXECVE {
                crate::profile::flush();
                crate::aot::save_before_exec();
            }
            let r = crate::spawn::exec_like(n, &args);
            set_res(cpu, r);
        }
        374 | 375 => {
            if TRACE.load(Ordering::Relaxed) {
                eprintln!("[kev] #{n} fd/id={:#x} nchanges={} nevents={} flags={:#x}", args[0], args[2], args[4], args[7]);
                for i in 0..(args[2].min(4)) {
                    let e = args[1] + i * 72;
                    let v = unsafe { std::slice::from_raw_parts(e as *const u64, 9) };
                    eprintln!("[kev]   change ident={:#x} filter={} flags={:#x} qos={:#x} udata={:#x} fflags={:#x}", v[0], (v[1] & 0xffff) as i16, (v[1] >> 16) & 0xffff, v[1] >> 32, v[2], v[3] & 0xffffffff);
                }
            }
            let r = crate::workq::kevent_redirect(n, &args).unwrap_or_else(|| hostsys::unix(n, &args));
            set_res(cpu, r);
        }
        // bsdthread_ctl(BSDTHREAD_CTL_SET_SELF): thread QoS/priority/voucher. Guest threads are
        // host threads we schedule ourselves, and the host kernel rejects some arm64-era
        // priority combinations with EINVAL (libdispatch then aborts), so just accept it.
        478 if args[0] == 0x100 => set_ok(cpu, 0, 0),
        92 if matches!(args[1], 59 | 61 | 97 | 98 | 103) => {
            // Code-signing fcntls (F_ADDSIGS, F_ADDFILESIGS[_RETURN/_INFO], F_CHECK_LV):
            // the Intel host kernel cannot validate arm64 signatures, so accept them.
            // F_ADDFILESIGS_RETURN/_INFO report the signed extent back in fs_file_start;
            // the signature blob sits at the tail, so everything before it is signed.
            if matches!(args[1], 97 | 103) && args[2] != 0 {
                unsafe {
                    let fs = args[2] as *mut u64;
                    let blob_off = *fs.add(1);
                    *fs = blob_off;
                }
            }
            set_ok(cpu, 0, 0);
        }
        92 if args[1] == 105 && args[2] != 0 => {
            // F_GETSIGSINFO: dyld asks whether a file slice is a platform (OS) binary before it
            // lets an arm64 process load an arm64e OS dylib from disk (e.g. libobjc-trampolines,
            // which is not in the macOS 13 shared cache). The faked F_ADDFILESIGS above never
            // registered a signature with the Intel host kernel, so answer for system files.
            let mut path = [0u8; 1024];
            let g = hostsys::unix(92, &[args[0], 50 /* F_GETPATH */, path.as_mut_ptr() as u64, 0, 0, 0, 0, 0]);
            let p = std::ffi::CStr::from_bytes_until_nul(&path).ok().and_then(|c| c.to_str().ok()).unwrap_or("");
            if !g.carry && (p.starts_with("/usr/lib/") || p.starts_with("/System/")) {
                // struct fgetsigsinfo { off_t fg_file_start; int fg_info_request; int fg_sig_is_platform; }
                unsafe { *((args[2] + 12) as *mut i32) = 1 };
                set_ok(cpu, 0, 0);
            } else {
                let r = hostsys::unix(n, &args);
                set_res(cpu, r);
            }
        }
        _ => {
            let r = hostsys::unix(n, &args);
            set_res(cpu, r);
        }
    }
}

/// arm64 macOS has 16 KiB pages; the Intel host has 4 KiB. Guest allocators (V8,
/// PartitionAlloc, libmalloc) rely on mmap returning 16 KiB-aligned, 16 KiB-sized
/// regions. Try the requested mapping first; only over-reserve, align, and trim
/// when the host did not already give us a suitably aligned address.
fn mmap_16k(args: &[u64; 8]) -> hostsys::SysResult {
    // A preference, never a reservation or MAP_FIXED: concurrent mappings and
    // stale hints are harmless, and caller-supplied hints always take priority.
    static NEXT_HINT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    mmap_16k_with(args, &NEXT_HINT, hostsys::unix)
}

fn mmap_16k_with(args: &[u64; 8], hint: &std::sync::atomic::AtomicU64,
                 mut syscall: impl FnMut(u64, &[u64; 8]) -> SysResult) -> SysResult {
    const PAGE: u64 = 0x4000;
    const MAP_FIXED: u64 = 0x10;
    const MAP_ANON: u64 = 0x1000;
    const MAP_PRIVATE: u64 = 0x2;
    let (len, flags) = (args[1], args[3]);
    if len == 0 || flags & MAP_FIXED != 0 {
        return syscall(SYS_MMAP, args);
    }
    let Some(rounded) = len.checked_add(PAGE - 1) else {
        return SysResult { rax: libc::EINVAL as u64, rdx: 0, carry: true };
    };
    let len16 = rounded & !(PAGE - 1);
    let mut a = *args;
    a[1] = len16;
    if a[0] == 0 { a[0] = hint.load(Ordering::Relaxed); }
    let first = syscall(SYS_MMAP, &a);
    let aligned_first = !first.carry && first.rax & (PAGE - 1) == 0;
    crate::profile::mmap_fast_path(aligned_first);
    if first.carry { return first; }
    if aligned_first {
        hint.store(first.rax + len16, Ordering::Relaxed);
        return first;
    }
    syscall(SYS_MUNMAP, &[first.rax, len16, 0, 0, 0, 0, 0, 0]);
    // Reserve len16 + one page of slack, address space only.
    let Some(reserve_len) = len16.checked_add(PAGE) else {
        return SysResult { rax: libc::EINVAL as u64, rdx: 0, carry: true };
    };
    let reserve = syscall(SYS_MMAP, &[0, reserve_len, 0, MAP_ANON | MAP_PRIVATE, u64::MAX, 0, 0, 0]);
    if reserve.carry {
        return reserve;
    }
    let base = reserve.rax;
    let aligned = (base + PAGE - 1) & !(PAGE - 1);
    // Map the real thing over the aligned window.
    a[0] = aligned;
    a[3] = flags | MAP_FIXED;
    let r = syscall(SYS_MMAP, &a);
    if r.carry {
        syscall(SYS_MUNMAP, &[base, reserve_len, 0, 0, 0, 0, 0, 0]);
        return r;
    }
    // Give back the slack before and after the aligned window.
    if aligned > base {
        syscall(SYS_MUNMAP, &[base, aligned - base, 0, 0, 0, 0, 0, 0]);
    }
    let end = aligned + len16;
    let reserve_end = base + reserve_len;
    if reserve_end > end {
        syscall(SYS_MUNMAP, &[end, reserve_end - end, 0, 0, 0, 0, 0, 0]);
    }
    hint.store(end, Ordering::Relaxed);
    r
}

fn write_sysctl_str(cpu: &mut Cpu, oldp: u64, oldlenp: u64, v: &str) {
    let bytes = [v.as_bytes(), &[0]].concat();
    unsafe {
        if oldlenp != 0 {
            let len = *(oldlenp as *const u64) as usize;
            if oldp != 0 {
                if len < bytes.len() {
                    set_err(cpu, libc::ENOMEM as u64);
                    return;
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), oldp as *mut u8, bytes.len());
            }
            *(oldlenp as *mut u64) = bytes.len() as u64;
        }
    }
    set_ok(cpu, 0, 0);
}

fn write_sysctl_int(cpu: &mut Cpu, oldp: u64, oldlenp: u64, v: i64) {
    unsafe {
        if oldlenp != 0 {
            let len = *(oldlenp as *const u64);
            if oldp != 0 {
                if len >= 8 {
                    *(oldp as *mut i64) = v;
                    *(oldlenp as *mut u64) = 8;
                } else if len >= 4 {
                    *(oldp as *mut i32) = v as i32;
                    *(oldlenp as *mut u64) = 4;
                } else {
                    set_err(cpu, libc::ENOMEM as u64);
                    return;
                }
            } else {
                *(oldlenp as *mut u64) = 4;
            }
        }
    }
    set_ok(cpu, 0, 0);
}

/// Private sysctl OID namespace (mib[0]) for overridden names; real top-level ids are small.
const FAKE_OID: i32 = 0x4154;
static FAKE_OID_NAMES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// sysctls whose answer must describe the emulated arm64 machine.
fn sysctl_override(name: &str) -> Option<i64> {
    Some(match name {
        "sysctl.proc_translated" => 0,
        "sysctl.proc_native" => 1,
        "hw.cputype" => 0x0100_000c,
        "hw.cpusubtype" => 2,
        "hw.optional.arm64" => 1,
        "hw.optional.neon" | "hw.optional.AdvSIMD" | "hw.optional.floatingpoint" => 1,
        "hw.optional.armv8_crc32" | "hw.optional.arm.FEAT_CRC32" => 1,
        "hw.optional.armv8_1_atomics" | "hw.optional.arm.FEAT_LSE" => 1,
        "hw.optional.arm.FEAT_LSE2" | "hw.optional.arm.FEAT_LRCPC" | "hw.optional.arm.FEAT_LRCPC2" => 1,
        "hw.optional.arm.FEAT_PAuth" | "hw.optional.arm.FEAT_JSCVT" | "hw.optional.arm.FEAT_FlagM"
        | "hw.optional.arm.FEAT_FlagM2" | "hw.optional.arm.FEAT_DotProd" | "hw.optional.arm.FEAT_FRINTTS"
        | "hw.optional.arm.FEAT_SB" | "hw.optional.arm.FEAT_DPB" | "hw.optional.arm.FEAT_DPB2" => 1,
        "hw.optional.arm.FEAT_FP16" | "hw.optional.arm.FEAT_FHM" | "hw.optional.arm.FEAT_FCMA"
        | "hw.optional.arm.FEAT_RDM" | "hw.optional.arm.FEAT_SHA3" | "hw.optional.arm.FEAT_SHA512"
        | "hw.optional.arm.FEAT_SME" | "hw.optional.arm.FEAT_SME2" | "hw.optional.arm.FEAT_BF16"
        | "hw.optional.arm.FEAT_I8MM" | "hw.optional.armv8_2_fhm" | "hw.optional.armv8_2_sha512"
        | "hw.optional.armv8_2_sha3" | "hw.optional.armv8_3_compnum" | "hw.optional.arm.FEAT_SHA1"
        | "hw.optional.arm.FEAT_AFP" | "hw.optional.arm.FEAT_RPRES" | "hw.optional.arm.FEAT_ECV"
        | "hw.optional.arm.FEAT_WFxT" | "hw.optional.arm.FEAT_CSSC" | "hw.optional.arm.FEAT_HBC" => 0,
        // crypto extensions the JIT translates natively (AES-NI / PCLMULQDQ when the host has them;
        // SHA256H/SU0/SU1 use only SSE4.1). SHA1/SHA512/SHA3 stay unadvertised: interpreted, they would be
        // slower than the software paths programs pick without them.
        "hw.optional.arm.FEAT_SHA256" => 1,
        "hw.optional.arm.FEAT_AES" => crate::jit::emit::has_aes() as i64,
        "hw.optional.arm.FEAT_PMULL" => (crate::jit::emit::has_aes() && crate::jit::emit::has_pclmul()) as i64,
        "hw.pagesize" | "hw.pagesize32" | "vm.pagesize" => 16384,
        "hw.cpufamily" => crate::commpage::CPUFAMILY as i64,
        "hw.optional.x86_64" | "hw.optional.sse" | "hw.optional.sse2" | "hw.optional.sse3"
        | "hw.optional.supplementalsse3" | "hw.optional.sse4_1" | "hw.optional.sse4_2"
        | "hw.optional.avx1_0" | "hw.optional.avx2_0" => 0,
        _ => return None,
    })
}

fn mach_trap(cpu: &mut Cpu, n: u64) {
    let trace = TRACE.load(Ordering::Relaxed);
    match n {
        3 => {
            cpu.x[0] = unsafe { mach_absolute_time() };
            return;
        }
        4 => {
            cpu.x[0] = unsafe { mach_continuous_time() };
            return;
        }
        _ => {}
    }
    let args = [cpu.x[0], cpu.x[1], cpu.x[2], cpu.x[3], cpu.x[4], cpu.x[5], cpu.x[6], cpu.x[7]];
    if n == 10 && args[3] & 1 != 0 && args[1] != 0 && args[2] != 0 {
        // mach_vm_allocate(VM_FLAGS_ANYWHERE): place it from a deterministic arena so that
        // dyld-loaded libraries land at the same addresses every run (persistent AOT cache).
        // The arena is a large PROT_NONE reservation; allocations overwrite pieces of it.
        const ARENA_SIZE: u64 = 64 << 30;
        static ARENA: std::sync::OnceLock<(u64, std::sync::atomic::AtomicU64)> = std::sync::OnceLock::new();
        let (base, next) = ARENA.get_or_init(|| {
            for cand in [0x40_0000_0000u64, 0x20_0000_0000, 0x10_0000_0000, 0x0c_0000_0000, 0x08_0000_0000] {
                let p = {
                    let _host = crate::profile::host();
                    unsafe { libc::mmap(cand as *mut _, ARENA_SIZE as usize, libc::PROT_NONE, libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE, -1, 0) }
                };
                if p as u64 == cand {
                    return (cand, std::sync::atomic::AtomicU64::new(0));
                }
                if p != libc::MAP_FAILED {
                    let _host = crate::profile::host();
                    unsafe { libc::munmap(p, ARENA_SIZE as usize) };
                }
            }
            (0, std::sync::atomic::AtomicU64::new(0))
        });
        let sz = (args[2] + 0x3fff) & !0x3fff;
        if *base != 0 {
            let off = next.fetch_add(sz, Ordering::SeqCst);
            if off + sz <= ARENA_SIZE {
                let at = base + off;
                let orig_hint = unsafe { *(args[1] as *const u64) };
                unsafe { *(args[1] as *mut u64) = at };
                let mut fixed = args;
                fixed[3] = (fixed[3] & !1) | 0x4000; // fixed + VM_FLAGS_OVERWRITE
                let r = hostsys::mach(n, &fixed);
                if r.rax == 0 {
                    cpu.x[0] = 0;
                    return;
                }
                if trace {
                    eprintln!("[mach] arena vm_allocate at {at:#x} size {sz:#x} failed: {:#x}", r.rax);
                }
                unsafe { *(args[1] as *mut u64) = orig_hint };
            }
        }
    }
    if n == 47 && crate::iokit::intercept(args[0], args[1]) {
        if trace {
            eprintln!("[mach] msg2 answered by iokit shim");
        }
        cpu.x[0] = 0;
        return;
    }
    // mach_msg2: remember the request id/size before the reply overwrites the header.
    let req_id = if n == 47 && args[0] != 0 { unsafe { *((args[0] + 20) as *const u32) } } else { 0 };
    if trace && n == 47 && args[0] != 0 {
        unsafe {
            let h = args[0] as *const u32;
            let size = (*h.add(1)).min(512) as usize;
            let body = std::slice::from_raw_parts(args[0] as *const u8, size);
            let mut strs = String::new();
            let mut cur = String::new();
            for &b in body {
                if (0x20..0x7f).contains(&b) {
                    cur.push(b as char);
                } else {
                    if cur.len() >= 4 {
                        strs.push_str(&format!(" \"{cur}\""));
                    }
                    cur.clear();
                }
            }
            eprintln!("[mach] msg2> bits={:#x} size={} rport={:#x} lport={:#x} id={} opt={:#x}{}", *h, *h.add(1), *h.add(2), *h.add(3), *h.add(5), args[1], strs);
        }
    }
    let r = hostsys::mach(n, &args);
    if n == 47 && args[0] != 0 {
        let reply_id = unsafe { *((args[0] + 20) as *const u32) };
        let retcode = unsafe { *((args[0] + 32) as *const u32) };
        if trace {
            eprintln!("[mach] msg2 id={req_id} reply_id={reply_id} retcode={retcode:#x}");
            if (2800..3000).contains(&req_id) {
                let w: Vec<String> = (0..16).map(|i| format!("{:08x}", unsafe { *((args[0] + i * 4) as *const u32) })).collect();
                eprintln!("[mach] reply words: {}", w.join(" "));
            }
        }
        // task_restartable_ranges_register: the Intel host kernel does not support it, and
        // libobjc treats failure as fatal. Restartable ranges only matter for arm64 threads
        // we already run in the interpreter/JIT, so pretend it worked.
        if req_id == 8000 && reply_id == req_id + 100 && retcode == 46 {
            unsafe { *((args[0] + 32) as *mut u32) = 0 };
        }
    }
    if trace {
        eprintln!("[mach] trap {}({:#x}, {:#x}, {:#x}, {:#x}) -> {:#x}", n, args[0], args[1], args[2], args[3], r.rax);
    }
    cpu.x[0] = r.rax;
}

extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_continuous_time() -> u64;
}

pub fn mach_name(n: u32) -> &'static str {
    match n {
        3 => "mach_absolute_time", 4 => "mach_continuous_time", 10 => "mach_vm_allocate",
        12 => "mach_vm_deallocate", 14 => "mach_vm_protect", 15 => "mach_vm_map",
        16 => "mach_port_allocate", 17 => "mach_port_destroy", 18 => "mach_port_deallocate",
        19 => "mach_port_mod_refs", 20 => "mach_port_move_member", 21 => "mach_port_insert_right",
        26 => "mach_reply_port", 27 => "thread_self", 28 => "task_self", 29 => "host_self",
        31 => "mach_msg", 32 => "mach_msg_overwrite", 33 => "semaphore_signal",
        34 => "semaphore_signal_all", 35 => "semaphore_signal_thread", 36 => "semaphore_wait",
        37 => "semaphore_wait_signal", 38 => "semaphore_timedwait", 39 => "semaphore_timedwait_signal",
        47 => "mach_msg2", 61 => "thread_switch", 62 => "clock_sleep", 89 => "mach_timebase_info",
        90 => "mach_wait_until", 91 => "mk_timer_create", 92 => "mk_timer_destroy",
        93 => "mk_timer_arm", 94 => "mk_timer_cancel", _ => "?",
    }
}

pub fn syscall_name(n: u64) -> &'static str {
    match n {
        1 => "exit",
        2 => "fork",
        3 => "read",
        4 => "write",
        5 => "open",
        6 => "close",
        20 => "getpid",
        33 => "access",
        46 => "sigaction",
        48 => "sigprocmask",
        54 => "ioctl",
        59 => "execve",
        73 => "munmap",
        74 => "mprotect",
        92 => "fcntl",
        116 => "gettimeofday",
        153 => "pread",
        169 => "csops",
        184 => "sigreturn",
        194 => "getrlimit",
        197 => "mmap",
        202 => "sysctl",
        220 => "getattrlist",
        244 => "posix_spawn",
        274 => "sysctlbyname",
        294 => "shared_region_check_np",
        327 => "issetugid",
        336 => "proc_info",
        338 => "stat64",
        339 => "fstat64",
        340 => "lstat64",
        344 => "getdirentries64",
        360 => "bsdthread_create",
        361 => "bsdthread_terminate",
        366 => "bsdthread_register",
        367 => "workq_open",
        368 => "workq_kernreturn",
        372 => "thread_selfid",
        396 => "read_nocancel",
        397 => "write_nocancel",
        398 => "open_nocancel",
        399 => "close_nocancel",
        463 => "openat",
        464 => "openat_nocancel",
        500 => "getentropy",
        515 => "ulock_wait",
        516 => "ulock_wake",
        520 => "terminate_with_payload",
        521 => "abort_with_payload",
        536 => "shared_region_map_and_slide_2_np",
        550 => "map_with_linking_np",
        7 => "wait4",
        9 => "link",
        10 => "unlink",
        12 => "sys_chdir",
        13 => "sys_fchdir",
        14 => "mknod",
        15 => "chmod",
        16 => "chown",
        18 => "getfsstat",
        23 => "setuid",
        24 => "getuid",
        25 => "geteuid",
        26 => "ptrace",
        27 => "recvmsg",
        28 => "sendmsg",
        29 => "recvfrom",
        30 => "accept",
        31 => "getpeername",
        32 => "getsockname",
        34 => "chflags",
        35 => "fchflags",
        36 => "sync",
        37 => "kill",
        38 => "sys_crossarch_trap",
        39 => "getppid",
        41 => "sys_dup",
        42 => "pipe",
        43 => "getegid",
        47 => "getgid",
        49 => "getlogin",
        50 => "setlogin",
        51 => "acct",
        52 => "sigpending",
        53 => "sigaltstack",
        55 => "reboot",
        56 => "revoke",
        57 => "symlink",
        58 => "readlink",
        60 => "umask",
        61 => "chroot",
        65 => "msync",
        66 => "vfork",
        67 => "oslog_coproc_reg",
        68 => "oslog_coproc",
        75 => "madvise",
        78 => "mincore",
        79 => "getgroups",
        80 => "setgroups",
        81 => "getpgrp",
        82 => "setpgid",
        83 => "setitimer",
        85 => "swapon",
        86 => "getitimer",
        89 => "sys_getdtablesize",
        90 => "sys_dup2",
        93 => "select",
        95 => "fsync",
        96 => "setpriority",
        97 => "socket",
        98 => "connect",
        100 => "getpriority",
        104 => "bind",
        105 => "setsockopt",
        106 => "listen",
        111 => "sigsuspend",
        117 => "getrusage",
        118 => "getsockopt",
        120 => "readv",
        121 => "writev",
        122 => "settimeofday",
        123 => "fchown",
        124 => "fchmod",
        126 => "setreuid",
        127 => "setregid",
        128 => "rename",
        131 => "sys_flock",
        132 => "mkfifo",
        133 => "sendto",
        134 => "shutdown",
        135 => "socketpair",
        136 => "mkdir",
        137 => "rmdir",
        138 => "utimes",
        139 => "futimes",
        140 => "adjtime",
        142 => "gethostuuid",
        147 => "setsid",
        151 => "getpgid",
        152 => "setprivexec",
        154 => "pwrite",
        155 => "nfssvc",
        157 => "statfs",
        158 => "fstatfs",
        159 => "unmount",
        161 => "getfh",
        164 => "funmount",
        165 => "quotactl",
        167 => "mount",
        170 => "csops_audittoken",
        173 => "waitid",
        177 => "kdebug_typefilter",
        178 => "kdebug_trace_string",
        179 => "kdebug_trace64",
        180 => "kdebug_trace",
        181 => "setgid",
        182 => "setegid",
        183 => "seteuid",
        185 => "sys_panic_with_data",
        186 => "thread_selfcounts",
        187 => "fdatasync",
        188 => "stat",
        189 => "sys_fstat",
        190 => "lstat",
        191 => "pathconf",
        192 => "sys_fpathconf",
        195 => "setrlimit",
        196 => "getdirentries",
        199 => "lseek",
        200 => "truncate",
        201 => "ftruncate",
        203 => "mlock",
        204 => "munlock",
        205 => "undelete",
        216 => "open_dprotected_np",
        217 => "fsgetpath_ext",
        218 => "openat_dprotected_np",
        221 => "setattrlist",
        222 => "getdirentriesattr",
        223 => "exchangedata",
        225 => "searchfs",
        226 => "delete",
        227 => "copyfile",
        228 => "fgetattrlist",
        229 => "fsetattrlist",
        230 => "poll",
        234 => "getxattr",
        235 => "fgetxattr",
        236 => "setxattr",
        237 => "fsetxattr",
        238 => "removexattr",
        239 => "fremovexattr",
        240 => "listxattr",
        241 => "flistxattr",
        242 => "fsctl",
        243 => "initgroups",
        245 => "ffsctl",
        248 => "fhopen",
        250 => "minherit",
        251 => "semsys",
        252 => "msgsys",
        253 => "shmsys",
        254 => "semctl",
        255 => "semget",
        256 => "semop",
        258 => "msgctl",
        259 => "msgget",
        260 => "msgsnd",
        261 => "msgrcv",
        262 => "shmat",
        263 => "shmctl",
        264 => "shmdt",
        265 => "shmget",
        266 => "shm_open",
        267 => "shm_unlink",
        268 => "sem_open",
        269 => "sem_close",
        270 => "sem_unlink",
        271 => "sem_wait",
        272 => "sem_trywait",
        273 => "sem_post",
        277 => "open_extended",
        278 => "umask_extended",
        279 => "stat_extended",
        280 => "lstat_extended",
        281 => "sys_fstat_extended",
        282 => "chmod_extended",
        283 => "fchmod_extended",
        284 => "access_extended",
        285 => "sys_settid",
        286 => "gettid",
        287 => "setsgroups",
        288 => "getsgroups",
        289 => "setwgroups",
        290 => "getwgroups",
        291 => "mkfifo_extended",
        292 => "mkdir_extended",
        293 => "identitysvc",
        296 => "vm_pressure_monitor",
        297 => "psynch_rw_longrdlock",
        298 => "psynch_rw_yieldwrlock",
        299 => "psynch_rw_downgrade",
        300 => "psynch_rw_upgrade",
        301 => "psynch_mutexwait",
        302 => "psynch_mutexdrop",
        303 => "psynch_cvbroad",
        304 => "psynch_cvsignal",
        305 => "psynch_cvwait",
        306 => "psynch_rw_rdlock",
        307 => "psynch_rw_wrlock",
        308 => "psynch_rw_unlock",
        309 => "psynch_rw_unlock2",
        310 => "getsid",
        311 => "sys_settid_with_pid",
        312 => "psynch_cvclrprepost",
        313 => "aio_fsync",
        314 => "aio_return",
        315 => "aio_suspend",
        316 => "aio_cancel",
        317 => "aio_error",
        318 => "aio_read",
        319 => "aio_write",
        320 => "lio_listio",
        322 => "iopolicysys",
        323 => "process_policy",
        324 => "mlockall",
        325 => "munlockall",
        328 => "__pthread_kill",
        329 => "__pthread_sigmask",
        330 => "__sigwait",
        331 => "__disable_threadsignal",
        332 => "__pthread_markcancel",
        333 => "__pthread_canceled",
        334 => "__semwait_signal",
        337 => "sendfile",
        341 => "stat64_extended",
        342 => "lstat64_extended",
        343 => "sys_fstat64_extended",
        345 => "statfs64",
        346 => "fstatfs64",
        347 => "getfsstat64",
        348 => "__pthread_chdir",
        349 => "__pthread_fchdir",
        350 => "audit",
        351 => "auditon",
        353 => "getauid",
        354 => "setauid",
        357 => "getaudit_addr",
        358 => "setaudit_addr",
        359 => "auditctl",
        362 => "kqueue",
        363 => "kevent",
        364 => "lchown",
        369 => "kevent64",
        373 => "ledger",
        374 => "kevent_qos",
        375 => "kevent_id",
        380 => "__mac_execve",
        381 => "__mac_syscall",
        382 => "__mac_get_file",
        383 => "__mac_set_file",
        384 => "__mac_get_link",
        385 => "__mac_set_link",
        386 => "__mac_get_proc",
        387 => "__mac_set_proc",
        388 => "__mac_get_fd",
        389 => "__mac_set_fd",
        390 => "__mac_get_pid",
        394 => "pselect",
        395 => "pselect_nocancel",
        400 => "wait4_nocancel",
        401 => "recvmsg_nocancel",
        402 => "sendmsg_nocancel",
        403 => "recvfrom_nocancel",
        404 => "accept_nocancel",
        405 => "msync_nocancel",
        406 => "sys_fcntl_nocancel",
        407 => "select_nocancel",
        408 => "fsync_nocancel",
        409 => "connect_nocancel",
        410 => "sigsuspend_nocancel",
        411 => "readv_nocancel",
        412 => "writev_nocancel",
        413 => "sendto_nocancel",
        414 => "pread_nocancel",
        415 => "pwrite_nocancel",
        416 => "waitid_nocancel",
        417 => "poll_nocancel",
        418 => "msgsnd_nocancel",
        419 => "msgrcv_nocancel",
        420 => "sem_wait_nocancel",
        421 => "aio_suspend_nocancel",
        422 => "__sigwait_nocancel",
        423 => "__semwait_signal_nocancel",
        424 => "__mac_mount",
        425 => "__mac_get_mount",
        426 => "__mac_getfsstat",
        427 => "fsgetpath",
        428 => "audit_session_self",
        429 => "audit_session_join",
        430 => "sys_fileport_makeport",
        431 => "sys_fileport_makefd",
        432 => "audit_session_port",
        433 => "pid_suspend",
        434 => "pid_resume",
        435 => "pid_hibernate",
        436 => "pid_shutdown_sockets",
        439 => "kas_info",
        440 => "memorystatus_control",
        441 => "guarded_open_np",
        442 => "guarded_close_np",
        443 => "guarded_kqueue_np",
        444 => "change_fdguard_np",
        445 => "usrctl",
        446 => "proc_rlimit_control",
        447 => "connectx",
        448 => "disconnectx",
        449 => "peeloff",
        450 => "socket_delegate",
        451 => "telemetry",
        452 => "proc_uuid_policy",
        453 => "memorystatus_get_level",
        454 => "system_override",
        455 => "vfs_purge",
        456 => "sfi_ctl",
        457 => "sfi_pidctl",
        458 => "coalition",
        459 => "coalition_info",
        460 => "necp_match_policy",
        461 => "getattrlistbulk",
        462 => "clonefileat",
        465 => "renameat",
        466 => "faccessat",
        467 => "fchmodat",
        468 => "fchownat",
        469 => "fstatat",
        470 => "fstatat64",
        471 => "linkat",
        472 => "unlinkat",
        473 => "readlinkat",
        474 => "symlinkat",
        475 => "mkdirat",
        476 => "getattrlistat",
        477 => "proc_trace_log",
        478 => "bsdthread_ctl",
        479 => "openbyid_np",
        480 => "recvmsg_x",
        481 => "sendmsg_x",
        482 => "thread_selfusage",
        483 => "csrctl",
        484 => "guarded_open_dprotected_np",
        485 => "guarded_write_np",
        486 => "guarded_pwrite_np",
        487 => "guarded_writev_np",
        488 => "renameatx_np",
        489 => "mremap_encrypted",
        490 => "netagent_trigger",
        491 => "stack_snapshot_with_config",
        492 => "microstackshot",
        494 => "persona",
        496 => "mach_eventlink_signal",
        497 => "mach_eventlink_wait_until",
        498 => "mach_eventlink_signal_wait_until",
        499 => "work_interval_ctl",
        501 => "necp_open",
        502 => "necp_client_action",
        503 => "__nexus_open",
        504 => "__nexus_register",
        505 => "__nexus_deregister",
        506 => "__nexus_create",
        507 => "__nexus_destroy",
        508 => "__nexus_get_opt",
        509 => "__nexus_set_opt",
        510 => "__channel_open",
        511 => "__channel_get_info",
        512 => "__channel_sync",
        513 => "__channel_get_opt",
        514 => "__channel_set_opt",
        517 => "fclonefileat",
        518 => "fs_snapshot",
        522 => "necp_session_open",
        523 => "necp_session_action",
        524 => "setattrlistat",
        525 => "net_qos_guideline",
        526 => "fmount",
        527 => "ntp_adjtime",
        528 => "ntp_gettime",
        529 => "os_fault_with_payload",
        530 => "kqueue_workloop_ctl",
        531 => "__mach_bridge_remote_time",
        532 => "coalition_ledger",
        533 => "log_data",
        534 => "memorystatus_available_memory",
        535 => "objc_bp_assist_cfg_np",
        537 => "pivot_root",
        538 => "task_inspect_for_pid",
        539 => "task_read_for_pid",
        540 => "sys_preadv",
        541 => "sys_pwritev",
        542 => "sys_preadv_nocancel",
        543 => "sys_pwritev_nocancel",
        544 => "sys_ulock_wait2",
        545 => "proc_info_extended_id",
        546 => "tracker_action",
        547 => "debug_syscall_reject",
        548 => "sys_debug_syscall_reject_config",
        549 => "graftdmg",
        551 => "freadlink",
        552 => "sys_record_system_event",
        553 => "mkfifoat",
        554 => "mknodat",
        555 => "ungraftdmg",
        556 => "sys_coalition_policy_set",
        557 => "sys_coalition_policy_get",
        _ => "?",
    }
}

#[cfg(test)]
mod profile_state_tests {
    use super::*;
    #[test]
    fn wait_classification_and_indirect_syscalls() {
        use crate::profile::State;
        let mut cpu = Cpu::default();
        for n in [31, 32, 47] {
            cpu.x[16] = (-n as i64) as u64;
            cpu.x[1] = 1;
            assert!(matches!(svc_state(&cpu), State::Mach));
            cpu.x[1] = 3;
            assert!(matches!(svc_state(&cpu), State::Idle));
        }
        for n in [36, 37, 38, 39, 61, 62, 90] {
            cpu.x[16] = (-n as i64) as u64;
            assert!(matches!(svc_state(&cpu), State::Idle));
        }
        for n in [33, 34, 35, 91, 92] {
            cpu.x[16] = (-n as i64) as u64;
            assert!(matches!(svc_state(&cpu), State::Mach));
        }
        for n in [93, 230, 301, 305, 334, 363, 369, 374, 375, 407, 417, 423, 515, 544] {
            cpu.x[16] = n;
            assert!(matches!(svc_state(&cpu), State::Idle));
            cpu.x[16] = 0;
            cpu.x[0] = n;
            assert!(matches!(svc_state(&cpu), State::Idle));
        }
        cpu.x[16] = crate::gpu::SYS_MCL_HOSTCALL;
        assert!(matches!(svc_state(&cpu), State::Gpu));
        cpu.x[16] = 4;
        assert!(matches!(svc_state(&cpu), State::Bsd));
    }
}

#[cfg(all(test, target_os = "macos", target_arch = "x86_64"))]
#[path = "syscalls/mmap_tests.rs"]
mod mmap_tests;
