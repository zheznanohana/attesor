//! Opt-in, bounded live Plain translations. The shared mapping is data, never executable.
//! No process-local pointer, mutable chain slot, or lifetime guard crosses the boundary.
use super::emit::{self, StubKind, Unit};
use attesor_core::cpu::off;
use iced_x86::{Decoder, DecoderOptions, Mnemonic, OpKind, Register};
use std::sync::atomic::{AtomicU32, Ordering};

const MAGIC: u32 = 0x5354_0001;
const HEADER: usize = 4096;
const SLOT: usize = 16384;
const SLOTS: usize = 512;
const SIZE: usize = HEADER + SLOT * SLOTS;
const META: usize = 32;
const MAX_KEY: usize = 2048;
const MAX_CODE: usize = 12000;

pub(super) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
    let has = |name| std::env::var(name).unwrap_or_default().split(',').any(|v| v == "sharedtrans");
    has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF")
        && std::env::var_os("ATTESOR_JIT_VERIFY").is_none()
        && std::env::var_os("ATTESOR_STATS").is_none()
        && std::env::var_os("ATTESOR_PROFILE").is_none()
        && !super::cost::probes()
    })
}

pub(super) struct Plain {
    pub pc: u64,
    pub source: Vec<u8>,
    pub code: Vec<u8>,
    pub shared: bool,
}

fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ *b as u64).wrapping_mul(0x100_0000_01b3))
}

// Deliberately narrow: no memory, flags, PC-relative address construction, calls,
// SIMD, system instructions, or indirect branches. Invalid operands still must
// pass the emitter's fallback count and final host-code policy below.
fn integer(w: u32) -> bool {
    w & 0x1f80_0000 == 0x1280_0000 // MOVN/Z/K
        || w & 0x3f00_0000 == 0x1100_0000 // ADD/SUB immediate, S=0
        || w & 0x3f20_0000 == 0x0b00_0000 // ADD/SUB shifted register, S=0
        || w & 0x1f00_0000 == 0x0a00_0000 && (w >> 29) & 3 != 3 // AND/ORR/EOR
}

// guestmem::read historically checks the Mach return code only. Shared
// adoption additionally requires the kernel to report every requested byte.
#[cfg(test)]
static SOURCE_READ_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static MAP_OPEN_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn read_exact(addr: u64, bytes: &mut [u8]) -> bool {
    #[cfg(test)] SOURCE_READ_CALLS.fetch_add(1, Ordering::Relaxed);
    super::fix2_diag::add(5);
    extern "C" {
        fn mach_task_self() -> u32;
        fn mach_vm_read_overwrite(task: u32, addr: u64, size: u64, data: u64, outsize: *mut u64) -> i32;
    }
    let mut count = 0u64;
    let result = unsafe {
        crate::profile::host_call(|| mach_vm_read_overwrite(mach_task_self(), addr, bytes.len() as u64,
            bytes.as_mut_ptr() as u64, &mut count))
    };
    result == 0 && count == bytes.len() as u64
}

pub(super) fn source_unchanged(unit: &Plain) -> bool {
    let mut current = vec![0u8; unit.source.len()];
    read_exact(unit.pc, &mut current) && current == unit.source
}

fn snapshot(pc: u64, image_end: u64) -> Option<Vec<u8>> {
    if pc & 3 != 0 { return None; }
    let end = pc.checked_add((emit::MAX_BLOCK_INSNS * 4) as u64)?.min(image_end);
    let mut source = Vec::new();
    let mut at = pc;
    while at < end {
        let page_end = (at | 0xfff).checked_add(1)?.min(end);
        let len = usize::try_from(page_end - at).ok()? & !3;
        if len == 0 { return None; }
        let mut page = vec![0; len];
        if !read_exact(at, &mut page) { return None; }
        for word in page.chunks_exact(4) {
            let w = u32::from_le_bytes(word.try_into().ok()?);
            let branch = w & 0xfc00_0000 == 0x1400_0000;
            if !branch && !integer(w) { return None; }
            source.extend_from_slice(word);
            if branch { return Some(source); }
        }
        at = page_end;
    }
    (source.len() == emit::MAX_BLOCK_INSNS * 4).then_some(source)
}

/// Validate the entire final x86 buffer, not just guest encodings. Only Cpu
/// architectural GPR accesses/stores and the local Cpu exit trampoline are legal.
/// Thus arbitrary immediates cannot become host addresses, and every internal
/// relative branch is rebased by copying the whole buffer unchanged.
fn plain_code(code: &[u8]) -> bool {
    if code.is_empty() || code.len() > MAX_CODE { return false; }
    let mut decoder = Decoder::with_ip(64, code, 0, DecoderOptions::NONE);
    let mut starts = Vec::new();
    let mut targets = Vec::new();
    while decoder.can_decode() {
        let i = decoder.decode();
        if i.is_invalid() { return false; }
        starts.push(i.ip());
        if !matches!(i.mnemonic(), Mnemonic::Mov | Mnemonic::Movzx | Mnemonic::Movsx | Mnemonic::Movsxd
            | Mnemonic::Add | Mnemonic::Sub | Mnemonic::And | Mnemonic::Or | Mnemonic::Xor
            | Mnemonic::Shl | Mnemonic::Shr | Mnemonic::Sar | Mnemonic::Rol | Mnemonic::Ror
            | Mnemonic::Not | Mnemonic::Neg | Mnemonic::Jmp) { return false; }
        for op in 0..i.op_count() {
            match i.op_kind(op) {
                OpKind::Register => {
                    // r15 must remain the pinned local Cpu, not a shared literal.
                    if i.op_register(op).full_register() == Register::R15 { return false; }
                }
                OpKind::Memory => {
                    if i.memory_base() != Register::R15 || i.memory_index() != Register::None { return false; }
                    let d = i.memory_displacement64() as usize;
                    let size = i.memory_size().size();
                    let gpr = d < off::X + 256 && d >= off::X && d.checked_add(size).is_some_and(|e| e <= off::X + 256);
                    let field = [off::PC, off::EXIT_REASON, off::EXIT_DATA].contains(&d) && size == 8;
                    let exit = i.mnemonic() == Mnemonic::Jmp && d == off::JIT_EXIT && size == 8;
                    if !(gpr || field || exit) || (i.mnemonic() == Mnemonic::Jmp && !exit) { return false; }
                }
                OpKind::NearBranch64 => {
                    if i.mnemonic() != Mnemonic::Jmp { return false; }
                    targets.push(i.near_branch_target());
                }
                OpKind::Immediate8 | OpKind::Immediate8to16 | OpKind::Immediate8to32 | OpKind::Immediate8to64
                | OpKind::Immediate16 | OpKind::Immediate32 | OpKind::Immediate32to64 | OpKind::Immediate64 => {}
                _ => return false,
            }
        }
        if i.mnemonic() == Mnemonic::Jmp && !matches!(i.op0_kind(), OpKind::Memory | OpKind::NearBranch64) { return false; }
    }
    targets.into_iter().all(|t| starts.binary_search(&t).is_ok())
}

fn compile(pc: u64, source: &[u8]) -> Option<Vec<u8>> {
    let read = |a: u64| {
        let o = usize::try_from(a.checked_sub(pc)?).ok()?;
        if o & 3 != 0 { return None; }
        Some(u32::from_le_bytes(source.get(o..o.checked_add(4)?)?.try_into().ok()?))
    };
    let mut u = Unit::new(StubKind::Plain, super::commpage_checks());
    u.no_internal_jumps = true;
    let l = u.a.create_label();
    u.labels.insert(pc, l);
    let end = emit::emit_block(&mut u, pc, &read);
    if end != pc.checked_add(source.len() as u64)? || u.insns_fallback != 0 || !u.inline_queue.is_empty() { return None; }
    u.finish();
    // No mutable slots, constant pools, or host address relocation tables.
    if u.first_slot.is_some() { return None; }
    let zero = u.a.assemble(0).ok()?;
    let moved = u.a.assemble(0x1234_5678_0000).ok()?;
    if zero != moved || !plain_code(&zero) { return None; }
    Some(zero)
}

struct Mapping(*mut u8);
// The shared protocol uses atomic state publication; ready slots are immutable.
// No borrowed mutable slices or fd escape, even between native threads.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}
fn mapping() -> Option<&'static Mapping> {
    static POOL: std::sync::OnceLock<Option<Mapping>> = std::sync::OnceLock::new();
    POOL.get_or_init(Mapping::open).as_ref()
}
/// Cache attachment failure too: unsupported/denied sharing must not make each
/// ordinary translation construct identity, snapshot guest text and retry shm.
pub(super) fn available() -> bool { enabled() && mapping().is_some() }
impl Drop for Mapping {
    fn drop(&mut self) { unsafe { libc::munmap(self.0.cast(), SIZE); } }
}
impl Mapping {
    fn open() -> Option<Self> {
        #[cfg(test)] MAP_OPEN_CALLS.fetch_add(1, Ordering::Relaxed);
        super::fix2_diag::add(6);
        // One fixed quota per effective UID/format, not one object per image or
        // process/build. Keys live inside slots. Nothing automatically unlinks it.
        let uid = unsafe { libc::geteuid() };
        #[cfg(not(test))]
        let name = format!("/attesor-st1-{uid}");
        // Test-only isolation: never append test records to a user's live pool.
        #[cfg(test)]
        let name = std::env::var("ATTESOR_TEST_SHARED_NAME").unwrap_or_else(|_| format!("/attesor-g4-{}", std::process::id()));
        let name = std::ffi::CString::new(name).ok()?;
        // Darwin shm_open rejects O_CLOEXEC. POSIX descriptors are normally
        // already close-on-exec; verify/set it explicitly before publication.
        let open = |flags| {
            #[cfg(test)]
            if let Ok(path) = std::env::var("ATTESOR_TEST_SHARED_FILE") {
                let Ok(path) = std::ffi::CString::new(path) else { return -1; };
                return unsafe { libc::open(path.as_ptr(), flags, 0o600) };
            }
            unsafe { libc::shm_open(name.as_ptr(), flags, 0o600) }
        };
        let mut created = true;
        let mut fd = open(libc::O_RDWR | libc::O_CREAT | libc::O_EXCL);
        if fd < 0 {
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) { return None; }
            created = false;
            fd = open(libc::O_RDWR);
        }
        if fd < 0 { return None; }
        let result = (|| {
            let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if fd_flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) } < 0 { return None; }
            if created && unsafe { libc::ftruncate(fd, SIZE as libc::off_t) } != 0 { return None; }
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(fd, &mut stat) } != 0 || stat.st_uid != uid || stat.st_mode & 0o077 != 0 || stat.st_size != SIZE as libc::off_t { return None; }
            let p = unsafe { libc::mmap(std::ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
            if p == libc::MAP_FAILED { return None; }
            let mapping = Self(p.cast());
            let ready = unsafe { &*mapping.0.cast::<AtomicU32>() };
            if created { ready.store(MAGIC, Ordering::Release); }
            // Never wait for an initializing/crashed creator.
            if ready.load(Ordering::Acquire) != MAGIC { return None; }
            Some(mapping)
        })();
        unsafe { libc::close(fd); }
        result
    }
    fn slot(&self, n: usize) -> *mut u8 { unsafe { self.0.add(HEADER + SLOT * n) } }
    fn read(&self, key: &[u8], pc: u64, source: &[u8]) -> Option<Vec<u8>> {
        for n in 0..SLOTS {
            let p = self.slot(n);
            if unsafe { &*p.cast::<AtomicU32>() }.load(Ordering::Acquire) != 2 { continue; }
            // State 2 is immutable for the entire named object's lifetime.
            let b = unsafe { std::slice::from_raw_parts(p.add(4), SLOT - 4) };
            let rd = |o| u32::from_le_bytes(b[o..o+4].try_into().unwrap()) as usize;
            let (k, s, c) = (rd(0), rd(4), rd(8));
            if k != key.len() || s != source.len() || k > MAX_KEY || c == 0 || c > MAX_CODE { continue; }
            let Some(end) = META.checked_add(k).and_then(|v| v.checked_add(s)).and_then(|v| v.checked_add(c)) else { continue; };
            if end > SLOT { continue; }
            let payload = unsafe { std::slice::from_raw_parts(p.add(META), k + s + c) };
            if u64::from_le_bytes(b[12..20].try_into().unwrap()) != pc
                || u64::from_le_bytes(b[20..28].try_into().unwrap()) != hash(payload)
                || &payload[..k] != key || &payload[k..k+s] != source { continue; }
            let code = &payload[k+s..];
            if plain_code(code) { return Some(code.to_vec()); }
        }
        None
    }
    fn publish(&self, key: &[u8], pc: u64, source: &[u8], code: &[u8]) -> bool {
        let mut payload = Vec::with_capacity(key.len() + source.len() + code.len());
        payload.extend_from_slice(key); payload.extend_from_slice(source); payload.extend_from_slice(code);
        if key.len() > MAX_KEY || META + payload.len() > SLOT { return false; }
        for n in 0..SLOTS {
            let p = self.slot(n);
            let state = unsafe { &*p.cast::<AtomicU32>() };
            if state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed).is_err() { continue; }
            // A dead writer leaks only this bounded slot. No PID stealing, waits,
            // truncation, recycling, flock, or robust-mutex recovery is required.
            unsafe {
                let put32 = |o, v: usize| std::ptr::copy_nonoverlapping((v as u32).to_le_bytes().as_ptr(), p.add(o), 4);
                put32(4, key.len()); put32(8, source.len()); put32(12, code.len());
                std::ptr::copy_nonoverlapping(pc.to_le_bytes().as_ptr(), p.add(16), 8);
                std::ptr::copy_nonoverlapping(hash(&payload).to_le_bytes().as_ptr(), p.add(24), 8);
                std::ptr::copy_nonoverlapping(payload.as_ptr(), p.add(META), payload.len());
            }
            state.store(2, Ordering::Release);
            return true;
        }
        false
    }
}

/// Called under TRANSLATE, but never acquires an application/global mutex.
/// Failure to attach/read/compile/publish always leaves the normal JIT available.
pub(super) fn prepare(identity: &str, pc: u64, image_end: u64) -> Option<Plain> {
    super::fix2_diag::add(4);
    if identity.len() > MAX_KEY { return None; }
    let mapping = mapping()?;
    let source = snapshot(pc, image_end)?;
    if let Some(code) = mapping.read(identity.as_bytes(), pc, &source) {
        return Some(Plain { pc, source, code, shared: true });
    }
    let code = compile(pc, &source)?;
    if !mapping.publish(identity.as_bytes(), pc, &source, &code) { return None; }
    Some(Plain { pc, source, code, shared: false })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::*;
    use attesor_core::cpu::Cpu;
    struct Arena(*mut libc::c_void,u64);
    impl Arena {
        fn new(hint:u64)->Self {
            let p=unsafe{libc::mmap(hint as *mut _,0x10000,3,libc::MAP_ANON|libc::MAP_PRIVATE,-1,0)};
            assert_ne!(p,libc::MAP_FAILED);
            if hint!=0 {assert_eq!(p as u64,hint,"non-destructive mapping hint must be available");}
            Self(p,p as u64)
        }
        fn write(&self,words:&[u32]) {unsafe{std::ptr::copy_nonoverlapping(words.as_ptr(),self.1 as *mut u32,words.len());}}
    }
    impl Drop for Arena {fn drop(&mut self){invalidate_range(self.1,0x10000);unsafe{libc::munmap(self.0,0x10000);}}}
    fn expected_enabled()->bool {
        let has=|k|std::env::var(k).unwrap_or_default().split(',').any(|t|t=="sharedtrans");
        has("ATTESOR_JIT_ON")&&!has("ATTESOR_JIT_OFF")&&!cost::probes()&&["ATTESOR_JIT_VERIFY","ATTESOR_STATS","ATTESOR_PROFILE"].iter().all(|k|std::env::var_os(k).is_none())
    }
    #[test]
    fn g4_sharedtrans_policy() { assert_eq!(enabled(),expected_enabled()); }
    #[test]
    fn g4_sharedtrans_snapshot_policy_local_execution_and_source_guard() {
        assert_eq!(enabled(),expected_enabled());
        let a=Arena::new(0);a.write(&[0xd28000e0,0x91001400,0x14001ffe]); // MOV7; ADD5; B +0x8000
        let source=snapshot(a.1,a.1+12).expect("bounded valid source");
        assert_eq!(source.len(),12);assert!(snapshot(a.1,a.1+8).is_none());assert!(snapshot(a.1+1,a.1+12).is_none());
        let compiled=compile(a.1,&source);
        if cost::probes() {
            assert!(compiled.is_none(),"profiling probes are outside the Plain sharing policy");
            assert!(!enabled());return;
        }
        let code=compiled.expect("production Plain emitter must be usable, not always fallback");
        assert!(plain_code(&code));
        for bad in [vec![0xc3],vec![0x41,0xff,0xe7],vec![0x48,0xa1,0,0,0,0,0,0,0,0],vec![0xe9,0,0,0,0],vec![0x90]] {assert!(!plain_code(&bad));}
        let unit=Plain{pc:a.1,source:source.clone(),code,shared:true};
        assert!(source_unchanged(&unit));
        let host={let lock=TRANSLATE.lock().unwrap();register_plain_unit(&lock,&unit).unwrap()};
        {let ix=INDEX.lock().unwrap();let ix=ix.as_ref().unwrap();let id=ix.units.iter().position(|u|u.host==host).unwrap();
            let u=&ix.units[id];assert_eq!((u.lo,u.hi),(a.1,a.1+12));assert!(u.alive_guard.is_none());
            assert_eq!(u.source_ranges.as_deref(),Some(&[(a.1,a.1+12)][..]));assert!(ix.pages[&(a.1>>14)].contains(&(id as u32)));}
        let mut c=Cpu::new();setup_cpu(&mut c);c.set_pc(a.1);c.set_nzcv(11);
        unsafe{attesor_jit_enter(&mut *c,host);}
        assert_eq!((c.x[0],c.pc(),c.exit_reason,c.nzcv()),(12,a.1+0x8000,EXIT_CONTINUE,11));
        // Native incoming slots must be reset when an adopted unit is retired.
        let slot=Box::new(AtomicU64::new(host));let slot_at=&*slot as *const _ as u64;
        {let mut ix=INDEX.lock().unwrap();let ix=ix.as_mut().unwrap();ix.slot_cont.insert(slot_at,0x1234);ix.incoming.entry(host).or_default().push(IncomingLink::Indirect(slot_at));}
        let epoch=EPOCH.load(Ordering::SeqCst);invalidate_range(a.1+4,4);
        assert_eq!(lookup(a.1),None);assert_eq!(slot.load(Ordering::SeqCst),0x1234);assert!(EPOCH.load(Ordering::SeqCst)>epoch);
        a.write(&[0xd2800540,0x91001400,0x14001ffe]); // source now returns 47, not 12
        assert!(!source_unchanged(&unit),"stale source must be rejected");
        {let lock=TRANSLATE.lock().unwrap();assert!(register_plain_unit(&lock,&unit).is_none());}
        // Old allocation remains readable/executable, demonstrating a sensitive stale-code oracle.
        unsafe{attesor_jit_enter(&mut *c,host);}assert_eq!(c.x[0],12);assert_ne!(c.x[0],47);
        for w in [0xf9400000,0x94000000,0xd4001001,0x54000000,0xd65f03c0] {a.write(&[w]);assert!(snapshot(a.1,a.1+4).is_none());}
    }
    #[test]
    fn g4_sharedtrans_publication_corruption_identity_and_capacity() {
        let p=unsafe{libc::mmap(std::ptr::null_mut(),SIZE,3,libc::MAP_ANON|libc::MAP_SHARED,-1,0)};assert_ne!(p,libc::MAP_FAILED);
        let m=Mapping(p.cast());let key=b"directed-build-image-options";let source=b"1234";
        let a=Arena::new(0);a.write(&[0xd28000e0,0x14000001]);let source_bytes=snapshot(a.1,a.1+8).unwrap();
        let code=if let Some(code)=compile(a.1,&source_bytes) {code} else {
            assert!(cost::probes());
            // Codec/publication is independent of instrumented emitter eligibility.
            use iced_x86::code_asm::*;
            let mut asm=CodeAssembler::new(64).unwrap();
            asm.mov(rax,7u64).unwrap();asm.mov(qword_ptr(r15+off::X),rax).unwrap();
            asm.jmp(qword_ptr(r15+off::JIT_EXIT)).unwrap();
            let code=asm.assemble(0).unwrap();assert!(plain_code(&code));code
        };
        let state=unsafe{&*m.slot(0).cast::<AtomicU32>()};state.store(1,Ordering::Release);
        assert!(m.read(key,a.1,source).is_none());state.store(0,Ordering::Release);
        assert!(m.publish(key,a.1,source,&code));assert_eq!(m.read(key,a.1,source),Some(code.clone()));
        assert!(m.read(b"other-build",a.1,source).is_none());assert!(m.read(key,a.1+4,source).is_none());assert!(m.read(key,a.1,b"4321").is_none());
        unsafe{*m.slot(0).add(META)^=1;}assert!(m.read(key,a.1,source).is_none());unsafe{*m.slot(0).add(META)^=1;}
        assert_eq!(m.read(key,a.1,source),Some(code.clone()));
        // Every remaining writing/crashed slot consumes a bounded quota; never wait/recycle.
        for n in 1..SLOTS {unsafe{&*m.slot(n).cast::<AtomicU32>()}.store(1,Ordering::Release);}
        assert!(!m.publish(key,a.1,source,&code));
        unsafe{std::ptr::copy_nonoverlapping((MAX_KEY as u32+1).to_le_bytes().as_ptr(),m.slot(0).add(4),4);}
        assert!(m.read(key,a.1,source).is_none());
    }
    #[test]
    fn g4_sharedtrans_worker() {
        let Ok(role)=std::env::var("ATTESOR_TEST_WORKER") else{return};
        assert!(enabled());assert!(!tiered_enabled());
        crate::aot::set_enabled(false);
        let a=Arena::new(0x60_0000_0000);a.write(&[0xd28000e0,0x91001400,0x14001ffe]);
        crate::aot::register_main("/port-g4/main",[0x47;16],a.1,a.1+0x10000,vec![],(a.1,a.1+0x10000));
        let (identity, end)=crate::aot::shared_translation_identity(a.1).expect("registered app/build identity");
        assert!(identity.len()<=MAX_KEY);assert_eq!(end,a.1+0x10000);
        assert!(Mapping::open().is_some(),"shared mapping: {}",std::io::Error::last_os_error());
        if role=="consumer" {code_cache().alloc(0x20000);}
        let host=translate(a.1);
        assert_eq!(SHARED_HITS.load(Ordering::SeqCst),if role=="consumer"{1}else{0});
        assert_eq!(SHARED_LOCAL.load(Ordering::SeqCst),if role=="producer"{1}else{0});
        let mut c=Cpu::new();setup_cpu(&mut c);c.set_pc(a.1);c.set_nzcv(11);
        unsafe{attesor_jit_enter(&mut *c,host);}
        assert_eq!((c.x[0],c.pc(),c.exit_reason,c.nzcv()),(12,a.1+0x8000,EXIT_CONTINUE,11));
        eprintln!("G4_SHARED_HOST={host:x} ROLE={role}");
    }
    #[test]
    fn g4_sharedtrans_two_independent_processes_adopt_without_retranslation() {
        if !enabled()||tiered_enabled(){eprintln!("sharedtrans adoption disabled by policy");return;}
        let name=format!("/ag4-{}-{:x}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64);
        // The managed test sandbox denies POSIX shm creation. Use a private
        // file only in test builds; all validation/publication/adoption code is
        // identical. This is NOT verification of real named-shm attachment.
        let dir=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/port-g4");
        std::fs::create_dir_all(&dir).unwrap();
        let path=dir.join(format!("{}.pool",name.trim_start_matches('/')));
        struct Remove(std::path::PathBuf);impl Drop for Remove{fn drop(&mut self){let _=std::fs::remove_file(&self.0);}}
        let _cleanup=Remove(path.clone());
        let mut hosts=Vec::new();
        for role in ["producer","consumer"] {
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","jit::sharedtrans::tests::g4_sharedtrans_worker","--nocapture","--test-threads=1"])
                .env("ATTESOR_TEST_WORKER",role).env("ATTESOR_TEST_SHARED_NAME",&name)
                .env("ATTESOR_TEST_SHARED_FILE",&path).output().unwrap();
            let s=format!("{}{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
            assert!(output.status.success(),"{role}: {s}");
            let host=s.split("G4_SHARED_HOST=").nth(1).unwrap().split_whitespace().next().unwrap().to_owned();hosts.push(host);
            eprintln!("{s}");
        }
        assert_ne!(hosts[0],hosts[1],"adoption must work at different local code-cache addresses");
    }
}

/// The wire format stays pointer-free Plain. On adoption only, redirect its
/// dispatcher exits to one LOCAL mutable data slot. Never modify shared bytes.
/// Otherwise an eligible hot block pays a Rust dispatch on every direct edge.
pub(super) fn localize_exits(code: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    if !plain_code(code) { return None; }
    let mut local = code.to_vec();
    let tail = (local.len() + 7) & !7;
    let mut decoder = Decoder::new(64, code, DecoderOptions::NONE);
    let mut exits = 0;
    while decoder.can_decode() {
        let at = decoder.position();
        let i = decoder.decode();
        if i.mnemonic() == Mnemonic::Jmp && i.op0_kind() == OpKind::Memory {
            if i.memory_base() != Register::R15 || i.memory_displacement64() != off::JIT_EXIT as u64 || i.len() < 5 { return None; }
            let disp = i32::try_from(tail as i64 - (at + 5) as i64).ok()?;
            local[at] = 0xe9;
            local[at+1..at+5].copy_from_slice(&disp.to_le_bytes());
            local[at+5..at+i.len()].fill(0x90);
            exits += 1;
        }
    }
    if exits == 0 { return None; }
    local.resize(tail, 0xcc);
    // jmp [tail+32]; fallback: lea rax,[tail+32]; mov [Cpu.exit_data],rax;
    // jmp [Cpu.jit_exit]. All addresses below are resolved only in this process.
    local.extend_from_slice(&[0xff,0x25,26,0,0,0, 0x48,0x8d,0x05,19,0,0,0, 0x49,0x89,0x87]);
    local.extend_from_slice(&(off::EXIT_DATA as u32).to_le_bytes());
    local.extend_from_slice(&[0x41,0xff,0xa7]);
    local.extend_from_slice(&(off::JIT_EXIT as u32).to_le_bytes());
    local.resize(tail + 32, 0xcc);
    local.extend_from_slice(&[0; 8]);
    Some((local, tail + 32, tail + 6))
}

#[cfg(test)]
mod fix2_tests {
    use super::*;
    use super::super::*;
    use attesor_core::cpu::Cpu;

    #[test]
    fn fix2_shared_attachment_is_cached_including_failure() {
        let before = MAP_OPEN_CALLS.load(Ordering::Relaxed);
        let first = mapping().map(|m| m.0 as usize);
        for _ in 0..32 { assert_eq!(mapping().map(|m| m.0 as usize), first); }
        assert!(MAP_OPEN_CALLS.load(Ordering::Relaxed) - before <= 1, "one attachment attempt, including denied shm, not one per translation");
    }

    #[test]
    fn fix2_shared_snapshot_reads_once_per_page() {
        let raw = unsafe { libc::mmap(std::ptr::null_mut(), 0x2000, 3, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
        assert_ne!(raw, libc::MAP_FAILED);
        let pc = raw as u64;
        unsafe { std::ptr::copy_nonoverlapping([0xd28000e0u32, 0x91001400, 0x14000001].as_ptr(), raw.cast(), 3); }
        let before = SOURCE_READ_CALLS.load(Ordering::Relaxed);
        assert_eq!(snapshot(pc, pc + 12).unwrap().len(), 12);
        assert_eq!(SOURCE_READ_CALLS.load(Ordering::Relaxed) - before, 1);
        let at = pc + 0xffc;
        unsafe { std::ptr::copy_nonoverlapping([0xd28000e0u32, 0x14000001].as_ptr(), at as *mut u32, 2); }
        let before = SOURCE_READ_CALLS.load(Ordering::Relaxed);
        assert_eq!(snapshot(at, at + 8).unwrap().len(), 8);
        assert_eq!(SOURCE_READ_CALLS.load(Ordering::Relaxed) - before, 2);
        assert!(snapshot(at, at + 4).is_none());
        unsafe { libc::munmap(raw, 0x2000); }
    }

    #[test]
    fn fix2_shared_plain_exit_chains_without_dispatch_and_revokes() {
        if cost::probes() || tiered_enabled() || rel32chain::enabled() { return; }
        let raw = unsafe { libc::mmap(std::ptr::null_mut(), 0x10000, 3, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
        assert_ne!(raw, libc::MAP_FAILED);
        let pc = raw as u64; let target = pc + 0x8000;
        struct Cleanup(u64);
        impl Drop for Cleanup { fn drop(&mut self) { invalidate_range(self.0, 0x10000); unsafe { libc::munmap(self.0 as *mut _, 0x10000); } } }
        let _cleanup = Cleanup(pc);
        unsafe {
            std::ptr::copy_nonoverlapping([0xd28000e0u32, 0x14001fffu32].as_ptr(), pc as *mut u32, 2);
            std::ptr::copy_nonoverlapping([0x91001400u32, 0xd4001001u32].as_ptr(), target as *mut u32, 2);
        }
        let source = snapshot(pc, pc + 8).unwrap();
        let wire = compile(pc, &source).unwrap();
        let unit = Plain { pc, source, code: wire.clone(), shared: true };
        let host = { let lock = TRANSLATE.lock().unwrap(); register_plain_unit(&lock, &unit).unwrap() };
        let dest = translate(target);
        let mut c = Cpu::new(); setup_cpu(&mut c); c.set_nzcv(11);
        unsafe { attesor_jit_enter(&mut *c, host); }
        assert_eq!((c.pc(), c.x[0], c.exit_reason), (target, 7, EXIT_CONTINUE));
        let slot = c.exit_data;
        assert_ne!(slot, 0, "shared Plain must acquire a local chain slot, not dispatch on every edge");
        let fallback = unsafe { *(slot as *const u64) };
        chain_slot(slot, target, dest);
        unsafe { attesor_jit_enter(&mut *c, host); }
        assert_eq!((c.x[0], c.exit_reason, c.nzcv()), (12, EXIT_SVC, 11));
        assert_eq!(unit.code, wire, "wire bytes must remain pointer-free and unchanged");
        assert!(plain_code(&unit.code));
        invalidate_range(target, 4);
        assert_eq!(unsafe { *(slot as *const u64) }, fallback);
        unsafe { attesor_jit_enter(&mut *c, host); }
        assert_eq!((c.x[0], c.pc(), c.exit_reason), (7, target, EXIT_CONTINUE));
    }
}
