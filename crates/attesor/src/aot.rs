//! Ahead-of-time translation cache.
//!
//! Images that Attesor maps at fixed addresses (the main executable and the
//! arm64 shared cache) get a persistent translation file in
//! `~/Library/Caches/Attesor/aot/`. Each file holds position-independent x86
//! code for a set of guest blocks plus an index.
//!
//! Where the blocks come from:
//!  * static discovery: `LC_FUNCTION_STARTS` + following direct branches
//!    (main executable only; the shared cache is too large to translate whole);
//!  * feedback: every block the JIT translated during a run is recorded in a
//!    profile, and the next AOT build includes it.
//!
//! On exit, if new blocks were seen, a forked child rebuilds the AOT files in
//! the background, so the next launch starts with warm translations.

use crate::jit::emit::{self, StubKind, Unit};
use std::collections::{BTreeSet, HashSet};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

const MAGIC: &[u8; 8] = b"MCLAOT05";
/// Bump when the translator's output changes.
const TRANSLATOR_VERSION: u32 = 99;

mod layout;

#[derive(Clone)]
struct Image {
    name: String,
    key: String,
    start: u64,
    end: u64,
    source: (u64, u64),
    /// Static entry points (function starts), if known.
    entries: Vec<u64>,
    loaded: bool,
}

static IMAGES: Mutex<Vec<Image>> = Mutex::new(Vec::new());
/// Blocks translated by the JIT this run (per image index).
static SEEN: Mutex<Vec<HashSet<u64>>> = Mutex::new(Vec::new());
static ENABLED: AtomicBool = AtomicBool::new(true);
static SAVED: AtomicBool = AtomicBool::new(false);
static CHECKPOINT_STARTED: AtomicBool = AtomicBool::new(false);
// Avoid inheriting another thread's open flock descriptor across fork. Taking
// the write side waits only for profile/cache publication, not translation.
pub(crate) static FILE_LOCKS: RwLock<()> = RwLock::new(());

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::SeqCst);
}

pub(crate) fn cache_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("ATTESOR_CACHE") {
        return d.into();
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::Path::new(&home).join("Library/Caches/Attesor/aot")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// FNV-1a, enough to name cache files.
fn fnv(parts: &[&[u8]]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for &b in *p {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Code generation options baked into translations: commpage checks and the optional host ISA
/// extensions the JIT uses when present (a cache built with FMA must not load on a host without it).
/// The high bit isolates the effective opt-in code-cache allocation policy;
/// it does not indicate that the kernel granted actual superpage backing.
fn flags_word() -> u32 {
    crate::jit::commpage_checks() as u32
        | (emit::has_fma() as u32) << 1
        | (emit::has_f16c() as u32) << 2
        | (emit::has_aes() as u32) << 3
        | (emit::has_pclmul() as u32) << 4
        | (emit::has_sha() as u32) << 5
        | (emit::has_avx512() as u32) << 6
        | (emit::avx512_enabled() as u32) << 7
        | (emit::avx512_nan_enabled() as u32) << 8
        | (crate::jit::fastenc_enabled() as u32) << 9
        | (emit::ibtc2_enabled() as u32) << 10
        | (crate::jit::flagreg::enabled() as u32) << 11
        | crate::jit::isel2_mask() << 12
        | (crate::jit::hugepages_enabled() as u32) << 31
}

/// Identifies the code generator: TRANSLATOR_VERSION mixed with this attesor binary's LC_UUID, so a
/// cache written by any other build is ignored even if someone forgot to bump TRANSLATOR_VERSION.
fn codegen_id() -> u32 {
    static ID: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *ID.get_or_init(|| {
        extern "C" {
            fn _dyld_get_image_header(i: u32) -> *const u8;
        }
        let mut uuid = [0u8; 16];
        unsafe {
            let h = _dyld_get_image_header(0);
            if !h.is_null() && *(h as *const u32) == 0xfeed_facf {
                let ncmds = *(h.add(16) as *const u32);
                let mut p = h.add(32);
                for _ in 0..ncmds {
                    let (cmd, size) = (*(p as *const u32), *(p.add(4) as *const u32));
                    if cmd == 0x1b {
                        uuid.copy_from_slice(std::slice::from_raw_parts(p.add(8), 16));
                        break;
                    }
                    p = p.add(size as usize);
                }
            }
        }
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in TRANSLATOR_VERSION.to_le_bytes().iter().chain(uuid.iter()) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        // These switches change emitted code too. Keep diagnostic/experimental
        // translations separate from default translations of this same binary.
        for name in ["ATTESOR_JIT_ON", "ATTESOR_JIT_OFF", "ATTESOR_JIT_DISABLE"] {
            let value = std::env::var(name).unwrap_or_default();
            for &b in name.as_bytes().iter().chain(value.as_bytes()).chain([0u8].iter()) {
                h ^= b as u64;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        }
        // Disk translations always use full Units; runtime quick code is never persisted.
        for &b in b"aot-full-only;tiered-quick-v1" {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        // Same immutable, validated weights used later for placement.
        if let Some(identity) = layout::identity() {
            for byte in b"offline-layout-profile-v1".iter().copied().chain(identity.to_le_bytes()) {
                h ^= byte as u64;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        }
        // Automatic opt-ins are process-local, not inherited environment. Include
        // their effective set so app policies cannot reuse incompatible blobs.
        for token in crate::policy::tokens() {
            for &b in token.as_bytes().iter().chain([0u8].iter()) {
                h ^= b as u64;
                h = h.wrapping_mul(0x100_0000_01b3);
            }
        }
        // Stats-enabled Plain regions contain an r15-relative entry counter.
        // Keep those blobs separate so normal runs pay no instrumentation cost.
        h ^= crate::jit::stats_enabled() as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
        (h ^ (h >> 32)) as u32 | 1
    })
}

/// Live-sharing identity is obtained before taking JIT TRANSLATE. Unlike the
/// truncated disk identity it retains the complete host build LC_UUID and the
/// exact option strings, app/image UUID keys and mapped guest bounds.
pub(crate) fn shared_translation_identity(pc: u64) -> Option<(String, u64)> {
    extern "C" { fn _dyld_get_image_header(i: u32) -> *const u8; }
    let mut build = [0u8; 16];
    unsafe {
        let h = _dyld_get_image_header(0);
        if h.is_null() || std::ptr::read_unaligned(h.cast::<u32>()) != 0xfeed_facf { return None; }
        let n = std::ptr::read_unaligned(h.add(16).cast::<u32>()) as usize;
        let size = std::ptr::read_unaligned(h.add(20).cast::<u32>()) as usize;
        if n > 4096 || size > 16 * 1024 * 1024 { return None; }
        let mut offset = 32usize;
        for _ in 0..n {
            if offset.checked_add(8)? > 32 + size { return None; }
            let p = h.add(offset);
            let cmd = std::ptr::read_unaligned(p.cast::<u32>());
            let len = std::ptr::read_unaligned(p.add(4).cast::<u32>()) as usize;
            if len < 8 || offset.checked_add(len)? > 32 + size { return None; }
            if cmd == 0x1b && len >= 24 {
                build.copy_from_slice(std::slice::from_raw_parts(p.add(8), 16));
                break;
            }
            offset += len;
        }
    }
    if build == [0; 16] { return None; }
    let imgs = IMAGES.lock().unwrap();
    let app = imgs.iter().find(|i| i.key.starts_with("main-"))?;
    let image = imgs.iter().find(|i| pc >= i.start && pc < i.end)?;
    if app.key.contains(&"0".repeat(32)) || image.key.contains(&"0".repeat(32)) { return None; }
    let options: Vec<_> = ["ATTESOR_JIT_ON", "ATTESOR_JIT_OFF", "ATTESOR_JIT_DISABLE"]
        .into_iter().map(|n| (n, std::env::var(n).unwrap_or_default())).collect();
    let key = format!("live-plain-v1;uid={};app={:?}@{:x}-{:x};image={:?}@{:x}-{:x};build={};translator={};flags={:x};avx={};opts={:?}",
        unsafe { libc::geteuid() }, app.key, app.start, app.end, image.key, image.start, image.end,
        hex(&build), TRANSLATOR_VERSION, flags_word(), std::is_x86_feature_detected!("avx"), options);
    Some((key, image.end))
}

/// Register the main executable (mapped at a fixed address).
pub fn register_main(path: &str, uuid: [u8; 16], start: u64, end: u64, entries: Vec<u64>, source: (u64, u64)) {
    let key = format!("main-{}-{}", hex(&uuid), fnv(&[path.as_bytes()]));
    add_image(Image { name: path.rsplit('/').next().unwrap_or(path).to_string(), key, start, end, source, entries, loaded: false });
}

/// Register dyld (also mapped at a fixed address).
pub fn register_dyld(path: &str, uuid: [u8; 16], start: u64, end: u64, source: (u64, u64)) {
    let key = format!("dyld-{}-{}", hex(&uuid), fnv(&[path.as_bytes()]));
    add_image(Image { name: "dyld".into(), key, start, end, source, entries: vec![], loaded: false });
}

/// Register the shared cache once dyld has had us map it.
pub fn register_cache(uuid: [u8; 16], start: u64, end: u64) {
    let key = format!("dsc-{}", hex(&uuid));
    add_image(Image { name: "dyld_shared_cache".into(), key, start, end, source: (start, end), entries: vec![], loaded: false });
    ensure_loaded();
}

/// Register the arm64 Mach-O whose header is mapped at `addr` (a dyld-loaded library or
/// helper image). Keyed by UUID and load address, so it only hits the cache when the
/// library loads at the same place again.
pub(crate) fn source_bounds(m: &crate::macho::MachO, slide: u64) -> (u64, u64) {
    let (lo, hi) = m.vm_range();
    if !crate::jit::precise_invalidation_enabled() { return (lo.wrapping_add(slide), hi.wrapping_add(slide)); }
    let mut bounds = None::<(u64, u64)>;
    for s in &m.segments {
        if s.initprot & 4 == 0 || s.vmsize == 0 || s.name == "__PAGEZERO" { continue; }
        let Some(end) = s.vmaddr.checked_add(s.vmsize) else { return (lo.wrapping_add(slide), hi.wrapping_add(slide)); };
        bounds = Some(bounds.map_or((s.vmaddr, end), |(a,b)| (a.min(s.vmaddr), b.max(end))));
    }
    let (lo, hi) = bounds.unwrap_or((lo, hi));
    (lo.wrapping_add(slide), hi.wrapping_add(slide))
}

pub fn register_image_at(addr: u64) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let rd = |o: u64| unsafe { std::ptr::read_unaligned((addr + o) as *const u32) };
    // mach_header_64: magic, cputype, ..., ncmds at +16, sizeofcmds at +20
    if rd(0) != 0xfeed_facf || rd(4) != 0x0100_000c {
        if std::env::var_os("ATTESOR_STATS").is_some() {
            eprintln!("[attesor] aot: mmap exec at {addr:#x} not an arm64 mach-o ({:#x} {:#x})", rd(0), rd(4));
        }
        return;
    }
    let ncmds = rd(16);
    let mut off = 32u64;
    let mut uuid = [0u8; 16];
    let (mut text_vm, mut lo, mut hi) = (u64::MAX, u64::MAX, 0u64);
    let (mut code_lo, mut code_hi) = (u64::MAX, 0u64);
    for _ in 0..ncmds.min(512) {
        let cmd = rd(off);
        let size = rd(off + 4) as u64;
        if size < 8 || off + size > 0x4000 {
            return;
        }
        if cmd == 0x1b {
            for i in 0..16 {
                uuid[i] = unsafe { *((addr + off + 8 + i as u64) as *const u8) };
            }
        } else if cmd == 0x19 {
            let mut name = [0u8; 16];
            for i in 0..16 {
                name[i] = unsafe { *((addr + off + 8 + i as u64) as *const u8) };
            }
            let vmaddr = unsafe { std::ptr::read_unaligned((addr + off + 24) as *const u64) };
            let vmsize = unsafe { std::ptr::read_unaligned((addr + off + 32) as *const u64) };
            if name.starts_with(b"__PAGEZERO") {
                off += size;
                continue;
            }
            if name.starts_with(b"__TEXT\0") {
                text_vm = vmaddr;
            }
            lo = lo.min(vmaddr);
            hi = hi.max(vmaddr + vmsize);
            if rd(off + 60) & 4 != 0 && vmsize != 0 {
                code_lo = code_lo.min(vmaddr);
                code_hi = code_hi.max(vmaddr + vmsize);
            }
        }
        off += size;
    }
    if text_vm == u64::MAX || uuid == [0u8; 16] || hi <= lo {
        return;
    }
    let slide = addr.wrapping_sub(text_vm);
    let (start, end) = (lo.wrapping_add(slide), hi.wrapping_add(slide));
    let key = format!("lib-{}-{:x}", hex(&uuid), start);
    if std::env::var_os("ATTESOR_STATS").is_some() {
        eprintln!("[attesor] aot: registered image {key} [{start:#x}, {end:#x})");
    }
    let source = if crate::jit::precise_invalidation_enabled() && code_lo < code_hi {
        (code_lo.wrapping_add(slide), code_hi.wrapping_add(slide))
    } else { (start, end) };
    add_image(Image { name: format!("lib-{}", &hex(&uuid)[..8]), key, start, end, source, entries: vec![], loaded: false });
    ensure_loaded();
}

fn add_image(img: Image) {
    let mut v = IMAGES.lock().unwrap();
    if v.iter().any(|i| i.key == img.key) {
        return;
    }
    v.push(img);
    SEEN.lock().unwrap().push(HashSet::new());
}

thread_local! {
    /// (start, end, image index) of the image this thread last recorded a block in.
    static LAST_IMG: std::cell::Cell<(u64, u64, usize)> = const { std::cell::Cell::new((1, 0, 0)) };
    static PENDING: Arc<PendingBatch> = {
        let batch = Arc::new(PendingBatch(Mutex::new(Vec::new())));
        PENDING_THREADS.lock().unwrap().push(Arc::downgrade(&batch));
        batch
    };
}

// Checkpoints must see even a sleeping thread's partial batch. A weak registry
// preserves batching without retaining terminated threads; Drop publishes their
// final tail. No registry lock is held while taking a batch or SEEN lock.
struct PendingBatch(Mutex<Vec<(usize, u64)>>);
static PENDING_THREADS: Mutex<Vec<Weak<PendingBatch>>> = Mutex::new(Vec::new());

fn publish_batch(batch: Vec<(usize, u64)>) {
    if batch.is_empty() {
        return;
    }
    let mut seen = SEEN.lock().unwrap();
    for (i, pc) in batch {
        seen[i].insert(pc);
    }
}

impl Drop for PendingBatch {
    fn drop(&mut self) {
        publish_batch(std::mem::take(self.0.get_mut().unwrap()));
    }
}

fn flush_all_pending() {
    let batches: Vec<_> = {
        let mut threads = PENDING_THREADS.lock().unwrap();
        let live: Vec<_> = threads.iter().filter_map(Weak::upgrade).collect();
        threads.retain(|t| t.strong_count() != 0);
        live
    };
    for batch in batches {
        publish_batch(std::mem::take(&mut *batch.0.lock().unwrap()));
    }
}

/// Record that the guest block at `pc` executed (JIT-translated or cold-interpreted), so the
/// next AOT build covers it.
pub fn note_translated(pc: u64) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let (start, end, idx) = LAST_IMG.with(|l| l.get());
    let idx = if pc >= start && pc < end {
        idx
    } else {
        let imgs = IMAGES.lock().unwrap();
        match imgs.iter().position(|im| pc >= im.start && pc < im.end) {
            Some(i) => {
                LAST_IMG.with(|l| l.set((imgs[i].start, imgs[i].end, i)));
                i
            }
            None => return,
        }
    };
    // Batch inserts so the global lock is taken once per 256 blocks.
    let flush = PENDING.with(|p| {
        let mut p = p.0.lock().unwrap();
        p.push((idx, pc));
        if p.len() >= 256 {
            Some(std::mem::take(&mut *p))
        } else {
            None
        }
    });
    if let Some(batch) = flush {
        publish_batch(batch);
    }
}

/// Publish this thread's not-yet-flushed blocks (called before saving the profile).
pub fn flush_pending() {
    let batch = PENDING.with(|p| std::mem::take(&mut *p.0.lock().unwrap()));
    publish_batch(batch);
}

fn file_for(img: &Image, ext: &str) -> std::path::PathBuf {
    let suffix = if crate::jit::fastenc_enabled() { "-fastenc" } else { "" };
    cache_dir().join(format!("{}{suffix}.{ext}", img.key))
}

/// Load AOT code for any registered image not loaded yet.
pub fn ensure_loaded() {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let mut imgs = IMAGES.lock().unwrap();
    for img in imgs.iter_mut() {
        if img.loaded {
            continue;
        }
        img.loaded = true;
        match load_file(&file_for(img, "aot"), Some(img.source)) {
            Ok(n) if n > 0 => {
                // in use: keep tidy_cache's 30-day rule from removing it (atime is not reliable)
                for kind in ["aot", "profile"] {
                    if let Ok(c) = std::ffi::CString::new(file_for(img, kind).as_os_str().as_encoded_bytes()) {
                        unsafe { libc::utimes(c.as_ptr(), std::ptr::null()) };
                    }
                }
                if std::env::var_os("ATTESOR_TRACE").is_some() || std::env::var_os("ATTESOR_STATS").is_some() {
                    eprintln!("[attesor] aot: loaded {n} blocks for {}", img.name);
                }
            }
            _ => {}
        }
    }
}

const CODE_ALIGN: usize = 0x4000;

fn checked_code_offset(n: usize) -> Option<usize> {
    Some(n.checked_mul(16)?.checked_add(40)?.checked_add(CODE_ALIGN - 1)? & !(CODE_ALIGN - 1))
}

fn code_offset(n: usize) -> usize {
    checked_code_offset(n).expect("AOT index too large")
}

fn load_file(path: &std::path::Path, image: Option<(u64, u64)>) -> std::io::Result<usize> {
    let mut f = std::fs::File::open(path)?;
    let mut head = [0u8; 40];
    if f.read_exact(&mut head).is_err() || &head[..8] != MAGIC {
        return Ok(0);
    }
    let rd32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let rd64 = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    if rd32(&head, 8) != codegen_id() || rd32(&head, 12) != flags_word() {
        return Ok(0);
    }
    let n = rd64(&head, 16) as usize;
    let code_len = rd64(&head, 24) as usize;
    let Some(code_off) = checked_code_offset(n) else { return Ok(0) };
    let Some(end) = code_off.checked_add(code_len) else { return Ok(0) };
    if n == 0 || code_len == 0 || f.metadata()?.len() < end as u64 {
        return Ok(0);
    }
    let mut idx = vec![0u8; n * 16];
    f.read_exact(&mut idx)?;
    // Reject corrupted offsets before registering even one executable block.
    if (0..n).any(|i| rd64(&idx, i * 16 + 8) >= code_len as u64) {
        return Ok(0);
    }
    let precise = crate::jit::precise_invalidation_enabled();
    if precise && image.is_some_and(|(lo, hi)| (0..n).any(|i| {
        let pc = rd64(&idx, i * 16);
        pc < lo || pc >= hi || pc & 3 != 0
    })) { return Ok(0); }
    // Avoid copying a blob whose source was already invalidated. Publication
    // still rechecks under BLOCKS, closing an invalidation race after this test.
    if precise && !crate::jit::aot_image_allowed(image) { return Ok(0); }
    // Copy the code into the JIT code cache. (Mapping the file itself PROT_EXEC makes macOS
    // run a Gatekeeper assessment on every cache file and pop up "could not verify" dialogs.)
    let cc = crate::jit::code_cache();
    let base = cc.alloc(code_len as u64);
    {
        use std::io::Seek;
        f.seek(std::io::SeekFrom::Start(code_off as u64))?;
        let dst = unsafe { std::slice::from_raw_parts_mut(base as *mut u8, code_len) };
        f.read_exact(dst)?;
    }
    let mut published = 0;
    for i in 0..n {
        let pc = rd64(&idx, i * 16);
        let off = rd64(&idx, i * 16 + 8);
        published += crate::jit::register_aot_block(pc, base + off, image) as usize;
    }
    Ok(if precise { published } else { n })
}

/// Run `f` holding an exclusive advisory lock for `img`'s cache files. Processes of one app map the same
/// library at the same base and so share its profile and translation file; without the lock a
/// read-merge-write of one process could drop the blocks another had just merged.
fn with_image_lock<T>(img: &Image, f: impl FnOnce() -> T) -> T {
    let _files = FILE_LOCKS.read().unwrap();
    use std::os::unix::io::AsRawFd;
    let lock = std::fs::OpenOptions::new().create(true).write(true).open(file_for(img, "lock"));
    if let Ok(l) = &lock {
        unsafe { libc::flock(l.as_raw_fd(), libc::LOCK_EX) };
    }
    let r = f();
    drop(lock); // closing releases the lock
    r
}

fn read_profile(path: &std::path::Path) -> BTreeSet<u64> {
    let mut s = BTreeSet::new();
    if let Ok(d) = std::fs::read(path) {
        for c in d.chunks_exact(8) {
            s.insert(u64::from_le_bytes(c.try_into().unwrap()));
        }
    }
    s
}

/// Save this run's profile; if it contains new blocks, rebuild AOT files in
/// a background (forked) process.
/// Merge the blocks seen so far into each image's on-disk profile; returns the images whose profile
/// grew (or that have no translation file yet), i.e. the ones worth (re)building.
fn update_profiles() -> Vec<(Image, BTreeSet<u64>)> {
    flush_all_pending();
    let imgs = IMAGES.lock().unwrap().clone();
    let seen = SEEN.lock().unwrap().clone();
    let _ = std::fs::create_dir_all(cache_dir());
    let mut rebuild = Vec::new();
    for (i, img) in imgs.iter().enumerate() {
        let path = file_for(img, "profile");
        let merged = with_image_lock(img, || {
            let mut prof = read_profile(&path);
            let before = prof.len();
            prof.extend(seen[i].iter().copied());
            let cached = compatible_header(&file_for(img, "aot"));
            // A profile can have been published just before a builder was killed.
            // File existence/count does not prove that those blocks were built.
            let aot_stale = cached.is_none_or(|(_, hash)| hash != profile_fingerprint(&prof));
            if prof.len() == before && !(aot_stale && (!prof.is_empty() || !img.entries.is_empty())) {
                return None;
            }
            let mut buf = Vec::with_capacity(prof.len() * 8);
            for pc in &prof {
                buf.extend_from_slice(&pc.to_le_bytes());
            }
            // atomic: other processes of the same app read these profiles concurrently
            let tmp = path.with_extension(format!("profile.{}", std::process::id()));
            if std::fs::write(&tmp, buf).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
            Some(prof)
        });
        if let Some(prof) = merged {
            rebuild.push((img.clone(), prof));
        }
    }
    rebuild
}

/// Cache housekeeping, once per process (on the checkpoint thread): removes temporaries of builders
/// that died (a killed process leaves a full-size `.aot.tmp.<pid>.<n>`), translation files this code
/// generator cannot load (older translator versions), and files of images not used for 30 days (an
/// app or system update gives its libraries new UUIDs, so their old files would never be read again).
fn tidy_cache() { tidy_cache_in(&cache_dir()); }

pub(crate) fn tidy_cache_in(root: &std::path::Path) {
    let Ok(dir) = std::fs::read_dir(root) else { return };
    let month = std::time::Duration::from_secs(30 * 86400);
    for e in dir.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let pid = name
            .split_once(".aot.tmp.")
            .and_then(|(_, rest)| rest.split('.').next())
            .or_else(|| name.split_once(".dscslide.tmp.").and_then(|(_, rest)| rest.split('.').next()))
            .or_else(|| name.rsplit_once(".profile.").map(|(_, p)| p))
            .and_then(|p| p.parse::<i32>().ok());
        let stale = if let Some(pid) = pid {
            pid != std::process::id() as i32
                && unsafe { libc::kill(pid, 0) } != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        } else if name.ends_with(".aot") || name.ends_with(".profile") || name.ends_with(".lock") || name.ends_with(".dscslide") {
            let unused = e.metadata().and_then(|m| m.modified()).ok()
                .and_then(|t| t.elapsed().ok()).is_some_and(|age| age > month);
            unused || (name.ends_with(".aot") && compatible_header(&path).is_none())
                || (name.ends_with(".dscslide") && !crate::cache::slid::compatible(&path))
        } else {
            false
        };
        if stale && name.contains(".dscslide") {
            // Stable lock inodes: unlinking a lock file can admit two writers.
            if name.ends_with(".lock") { continue; }
            let _files = FILE_LOCKS.read().unwrap();
            use std::os::unix::io::AsRawFd;
            let lock_path = if let Some((stem, _)) = name.split_once(".dscslide.tmp.") {
                path.with_file_name(format!("{stem}.dscslide.lock"))
            } else { path.with_extension("dscslide.lock") };
            if let Ok(lock) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(lock_path) {
                if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    // A builder may have replaced the stale inode since the
                    // directory scan. Re-evaluate under the publication lock.
                    let expired = std::fs::metadata(&path).and_then(|m| m.modified()).ok()
                        .and_then(|t| t.elapsed().ok()).is_some_and(|age| age > month);
                    if pid.is_some() || expired || !crate::cache::slid::compatible(&path) {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        } else if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Periodic checkpoints: a background thread updates the profiles and rebuilds the changed images
/// (first after ATTESOR_AOT_CHECKPOINT seconds, default 60, 0 = off; then every 10 minutes), so a session
/// that is force-quit or crashes still leaves translations for the next launch. In-process at background QoS.
pub fn start_checkpoints() {
    if !ENABLED.load(Ordering::Relaxed) || std::env::var_os("ATTESOR_NO_AOT_BUILD").is_some() {
        return;
    }
    let first: u64 = std::env::var("ATTESOR_AOT_CHECKPOINT").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    if first == 0 || CHECKPOINT_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new().name("attesor-aot".into()).spawn(move || {
        extern "C" {
            fn pthread_set_qos_class_self_np(qos: u32, relpri: i32) -> i32;
        }
        unsafe { pthread_set_qos_class_self_np(0x09, 0) }; // QOS_CLASS_BACKGROUND
        tidy_cache();
        std::thread::sleep(std::time::Duration::from_secs(first));
        loop {
            if SAVED.load(Ordering::SeqCst) {
                return; // exiting: save_profile has taken over
            }
            let changed = update_profiles();
            if std::env::var_os("ATTESOR_STATS").is_some() {
                eprintln!("[attesor] aot checkpoint: {} image(s) to rebuild", changed.len());
            }
            for (img, prof) in changed {
                let r = build(&img, &prof);
                if std::env::var_os("ATTESOR_STATS").is_some() {
                    eprintln!("[attesor] aot checkpoint: {} ({} profiled blocks): {:?} pid={}", img.name, prof.len(), r.as_ref().map(|_| ()), std::process::id());
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(600));
        }
    });
    if spawned.is_err() {
        CHECKPOINT_STARTED.store(false, Ordering::SeqCst);
    }
}

// Lock order: registry -> all batches -> IMAGES -> SEEN -> FILE_LOCKS.
// Recording releases its batch before SEEN; profile updates release all memory
// locks before opening a file lock. Drop every guard in BOTH fork branches
// before running guest code or starting a new checkpoint worker.
fn fork_safely(wait_background: bool) -> libc::pid_t {
    // Guest fork pauses new jobs and coordinates one bounded background unit
    // before any existing fork locks. Only optional async AOT saving skips a
    // busy emission. No worker queue mutex is acquired by this fork path.
    let Ok(_bgtrans) = crate::jit::bgtrans_fork_guard(wait_background) else {
        unsafe { *libc::__error() = libc::EAGAIN; }
        return -1;
    };
    let mut profile = crate::profile::fork_guard();
    let cpus = crate::engine::fork_guard();
    let (pid, error) = {
        let registry = PENDING_THREADS.lock().unwrap();
        let batches: Vec<_> = registry.iter().filter_map(Weak::upgrade).collect();
        let mut guards: Vec<_> = batches.iter().map(|b| b.0.lock().unwrap()).collect();
        let _images = IMAGES.lock().unwrap();
        let mut seen = SEEN.lock().unwrap();
        for batch in &mut guards {
            for (i, pc) in batch.drain(..) {
                seen[i].insert(pc);
            }
        }
        let _files = FILE_LOCKS.write().unwrap();
        profile.host_begin();
        let pid = unsafe { libc::fork() };
        if pid == 0 { crate::jit::bgtrans_after_fork_child(); }
        let error = if pid < 0 { unsafe { *libc::__error() } } else { 0 };
        profile.host_end();
        (pid, error)
    };
    if pid == 0 { profile.child(); }
    cpus(pid == 0);
    drop(profile);
    if pid < 0 {
        unsafe { *libc::__error() = error };
    }
    pid
}

/// A guest fork is not a process exit: do not set SAVED or stop the parent's
/// checkpoint worker. Only the calling thread survives in the child.
pub fn fork_guest() -> libc::pid_t {
    let pid = fork_safely(true);
    if pid == 0 {
        PENDING_THREADS.lock().unwrap().clear();
        PENDING.with(|batch| PENDING_THREADS.lock().unwrap().push(Arc::downgrade(batch)));
        SAVED.store(false, Ordering::SeqCst);
        CHECKPOINT_STARTED.store(false, Ordering::SeqCst);
        start_checkpoints();
        crate::profile::after_fork();
    }
    pid
}

pub fn save_profile() {
    save_profile_inner(true);
}

/// Exec may fail and return to the guest: persist its translations without
/// declaring this process finished or disabling future checkpoints.
pub fn save_before_exec() {
    save_profile_inner(false);
}

fn save_profile_inner(exiting: bool) {
    if !ENABLED.load(Ordering::Relaxed) || (exiting && SAVED.swap(true, Ordering::SeqCst)) {
        return;
    }
    flush_pending();
    let rebuild = update_profiles();
    if rebuild.is_empty() || std::env::var_os("ATTESOR_NO_AOT_BUILD").is_some() {
        return;
    }
    let sync = std::env::var_os("ATTESOR_AOT_SYNC").is_some();
    // Fork: the child still has the guest's memory mapped, so it can read
    // the code it needs to translate.
    let pid = if sync { 0 } else { fork_safely(false) };
    if pid != 0 {
        return;
    }
    if !sync {
        // Only the detached fork child is a dedicated translator. In sync mode
        // (including save-before-exec) this is still the guest's own thread.
        crate::process_qos::background_translation_worker("aot-exit");
        unsafe {
            libc::setsid();
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            libc::dup2(null, 0);
            libc::dup2(null, 1);
            libc::dup2(null, 2);
        }
    }
    for (img, prof) in rebuild {
        let _ = build(&img, &prof);
    }
    if !sync {
        unsafe { libc::_exit(0) };
    }
}

/// Collect block starts reachable from `entries` via direct branches.
fn discover(entries: &[u64], start: u64, end: u64, read: &dyn Fn(u64) -> Option<u32>, limit: usize) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    let mut work: Vec<u64> = entries.to_vec();
    while let Some(pc) = work.pop() {
        if pc < start || pc >= end || pc & 3 != 0 || !out.insert(pc) || out.len() > limit {
            continue;
        }
        let mut cur = pc;
        for _ in 0..emit::MAX_BLOCK_INSNS {
            let Some(insn) = read(cur) else { break };
            if insn == 0 {
                break;
            }
            let next = cur + 4;
            let sext = |v: u64, n: u32| (((v << (64 - n)) as i64) >> (64 - n)) as u64;
            if insn & 0x7C00_0000 == 0x1400_0000 {
                let t = cur.wrapping_add(sext((insn & 0x03ff_ffff) as u64, 26) << 2);
                work.push(t); // B target or BL callee, including binaries without function starts
                if insn & 0x8000_0000 != 0 {
                    work.push(next); // return point after BL
                }
                break;
            } else if insn & 0xFF00_0000 == 0x5400_0000 || insn & 0x7E00_0000 == 0x3400_0000 {
                work.push(cur.wrapping_add(sext(((insn >> 5) & 0x7ffff) as u64, 19) << 2));
                work.push(next);
                break;
            } else if insn & 0x7E00_0000 == 0x3600_0000 {
                work.push(cur.wrapping_add(sext(((insn >> 5) & 0x3fff) as u64, 14) << 2));
                work.push(next);
                break;
            } else if emit::is_terminator(insn) {
                if insn & 0xFE00_0000 == 0xD600_0000 && (insn >> 21) & 0xf == 1 {
                    work.push(next); // BLR returns here
                }
                if insn & 0xFFE0_001F == 0xD400_0001 || emit::is_isb(insn) || emit::is_fpcr_write(insn) {
                    work.push(next); // SVC/ISB/FPCR writes continue through the dispatcher
                }
                break;
            }
            cur = next;
        }
    }
    out
}

fn image_word(start: u64, end: u64, bounded: bool, pc: u64,
              read: &dyn Fn(u64) -> Option<u32>) -> Option<u32> {
    if bounded && (pc < start || pc.checked_add(4).is_none_or(|hi| hi > end)) { return None; }
    read(pc)
}

fn build(img: &Image, profile: &BTreeSet<u64>) -> std::io::Result<()> {
    let reader = crate::jit::guest_reader();
    let bounded = crate::jit::precise_invalidation_enabled();
    // This invariant lets the loader conservatively register the whole image
    // without pretending that an entry PC describes an arbitrary AOT unit.
    // It covers lookahead, loops, and inline direct targets as well as emission.
    let read = |pc| image_word(img.source.0, img.source.1, bounded, pc, &reader);
    let mut blocks: BTreeSet<u64> = profile.clone();
    if !img.entries.is_empty() {
        blocks.extend(discover(&img.entries, img.start, img.end, &read, 400_000));
    }
    // Profile-guided expansion: also follow direct branches from hot blocks.
    let hot: Vec<u64> = profile.iter().copied().collect();
    blocks.extend(discover(&hot, img.start, img.end, &read, profile.len() * 4 + 1000));
    let mut blocks: Vec<u64> = blocks.into_iter().filter(|&pc| pc >= img.start && pc < img.end && pc & 3 == 0 && read(pc).is_some()).collect();
    if blocks.is_empty() {
        return Ok(());
    }
    // Translate in chunks so a single assembler run stays manageable.
    let mut code: Vec<u8> = Vec::new();
    let mut index: Vec<(u64, u64)> = Vec::new();
    let mut offline_counts = crate::jit::offline::Counts::default();
    let weighted_hot = layout::order(img, &mut blocks, profile);
    for chunk in layout::chunks(&blocks, weighted_hot) {
        let mut u = Unit::new(StubKind::Plain, crate::jit::commpage_checks());
        for &pc in chunk {
            let l = u.a.create_label();
            u.labels.insert(pc, l);
        }
        for &pc in chunk {
            emit::emit_block(&mut u, pc, &read);
        }
        u.finish();
        let base = code.len() as u64;
        let res = match crate::jit::offline::assemble(&mut u, base) {
            Ok((r, counts)) => {
                offline_counts.before += counts.before;
                offline_counts.removed += counts.removed;
                r
            },
            Err(_) => continue,
        };
        for &pc in chunk {
            if let Ok(ip) = res.label_ip(&u.labels[&pc]) {
                index.push((pc, ip));
            }
        }
        code.extend_from_slice(&res.inner.code_buffer);
        while code.len() % 16 != 0 {
            code.push(0xcc);
        }
    }
    if crate::jit::offline::enabled() && crate::jit::stats_enabled() {
        eprintln!("[attesor] offline-dse: {} host_insns={} removed_stores={} remaining_insns={}",
                  img.name, offline_counts.before, offline_counts.removed,
                  offline_counts.before - offline_counts.removed);
    }
    let path = file_for(img, "aot");
    // per-process temporary: several processes may build the same image concurrently
    // A checkpoint and the exit builder can overlap in one process as well.
    static NEXT_BUILD: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!("aot.tmp.{}.{}", std::process::id(), NEXT_BUILD.fetch_add(1, Ordering::Relaxed)));
    let mut f = std::fs::File::create(&tmp)?;
    let mut hdr = Vec::new();
    hdr.extend_from_slice(MAGIC);
    hdr.extend_from_slice(&codegen_id().to_le_bytes());
    hdr.extend_from_slice(&flags_word().to_le_bytes());
    hdr.extend_from_slice(&(index.len() as u64).to_le_bytes());
    hdr.extend_from_slice(&(code.len() as u64).to_le_bytes());
    hdr.extend_from_slice(&profile_fingerprint(profile).to_le_bytes());
    for (pc, off) in &index {
        hdr.extend_from_slice(&pc.to_le_bytes());
        hdr.extend_from_slice(&off.to_le_bytes());
    }
    while hdr.len() < code_offset(index.len()) {
        hdr.push(0);
    }
    f.write_all(&hdr)?;
    f.write_all(&code)?;
    drop(f);
    // keep whichever translation covers more blocks: a builder that started from a smaller profile
    // and finishes last must not replace a larger file another process wrote meanwhile
    with_image_lock(img, || {
        let latest = read_profile(&file_for(img, "profile"));
        let hash = profile_fingerprint(profile);
        let existing = compatible_header(&path);
        // A newer profile arrived while translating. Never replace a file that
        // already covers it with this older build. Otherwise partial coverage
        // is useful now; the fingerprint mismatch schedules another rebuild.
        let latest_hash = profile_fingerprint(&latest);
        if (hash != latest_hash && existing.is_some_and(|(_, old_hash)| old_hash == latest_hash))
            || existing.is_some_and(|(n, old_hash)| old_hash == hash && n > index.len())
        {
            let _ = std::fs::remove_file(&tmp);
            Ok(())
        } else {
            std::fs::rename(&tmp, &path)
        }
    })?;
    Ok(())
}

/// Result of a static prewarm. These blocks use the same fixed guest addresses,
/// UUID/path keys and code-generation compatibility checks as normal execution.
pub struct PrewarmImage {
    pub path: String,
    pub kind: &'static str,
    pub blocks: usize,
}

/// Reserve before MAP_FIXED so offline translation never clobbers unrelated
/// mappings. Each image is unmapped afterwards, allowing a batch to reuse the
/// runtime's main-image base without leaking previous images into discovery.
struct PrewarmMapping { start: u64, bytes: usize }
impl Drop for PrewarmMapping {
    fn drop(&mut self) { unsafe { libc::munmap(self.start as *mut _, self.bytes); } }
}

fn map_prewarm_text(path: &str, m: &crate::macho::MachO, base: u64) -> std::io::Result<PrewarmMapping> {
    use std::os::unix::io::AsRawFd;
    let (lo, hi) = m.vm_range();
    let bytes = hi.checked_sub(lo).and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n > 0 && n <= isize::MAX as usize)
        .ok_or_else(|| std::io::Error::other(format!("{path}: invalid image range")))?;
    let file = std::fs::File::open(path)?;
    let file_bytes = file.metadata()?.len();
    let p = unsafe { libc::mmap(base as *mut _, bytes, libc::PROT_NONE,
                                libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
    if p as u64 != base {
        if p != libc::MAP_FAILED { unsafe { libc::munmap(p, bytes); } }
        return Err(std::io::Error::other(format!("{path}: prewarm address {base:#x} unavailable")));
    }
    let mapping = PrewarmMapping { start: base, bytes };
    let mut code_segments = 0;
    for seg in &m.segments {
        if seg.name == "__PAGEZERO" || seg.initprot & 4 == 0 || seg.filesize == 0 { continue; }
        let relative = seg.vmaddr.checked_sub(lo)
            .ok_or_else(|| std::io::Error::other("prewarm segment precedes its image"))?;
        if relative.checked_add(seg.vmsize).is_none_or(|end| end > bytes as u64) {
            return Err(std::io::Error::other(format!("{path}: executable segment outside reserved image")));
        }
        let addr = base.checked_add(relative)
            .ok_or_else(|| std::io::Error::other("prewarm address overflow"))?;
        let offset = m.slice_offset.checked_add(seg.fileoff)
            .ok_or_else(|| std::io::Error::other("prewarm file offset overflow"))?;
        if addr & 0xfff != 0 || offset & 0xfff != 0 || seg.filesize > seg.vmsize
            || offset.checked_add(seg.filesize).is_none_or(|end| end > file_bytes)
        {
            return Err(std::io::Error::other(format!("{path}: invalid executable segment {}", seg.name)));
        }
        let mapped = unsafe { libc::mmap(addr as *mut _, seg.filesize as usize, libc::PROT_READ,
                                         libc::MAP_PRIVATE | libc::MAP_FIXED, file.as_raw_fd(), offset as i64) };
        if mapped as u64 != addr { return Err(std::io::Error::last_os_error()); }
        code_segments += 1;
    }
    if code_segments == 0 { return Err(std::io::Error::other(format!("{path}: no executable segments"))); }
    Ok(mapping)
}

fn prewarm_image(path: &str, dyld: bool) -> std::io::Result<PrewarmImage> {
    let m = crate::macho::MachO::open_arm64(path, dyld)?;
    let expected_type = if dyld { crate::macho::MH_DYLINKER } else { crate::macho::MH_EXECUTE };
    if m.cputype != crate::macho::CPU_TYPE_ARM64 || m.filetype != expected_type {
        return Err(std::io::Error::other(format!("{path}: not an arm64 {}", if dyld { "dyld" } else { "executable" })));
    }
    let (lo, hi) = m.vm_range();
    let base = if dyld { crate::loader::DYLD_BASE } else { crate::loader::MAIN_BASE };
    let slide = base.wrapping_sub(lo);
    let _mapping = map_prewarm_text(path, &m, base)?;
    if dyld { crate::nativefixups::install(path, &m, slide); }
    let mut entries = function_starts(path, &m, slide);
    if let Some(off) = m.main_entryoff { entries.push(m.text_vmaddr().wrapping_add(off).wrapping_add(slide)); }
    if let Some(pc) = m.thread_pc { entries.push(pc.wrapping_add(slide)); }
    entries.sort_unstable();
    entries.dedup();
    let kind = if dyld { "dyld" } else { "main" };
    let key = format!("{kind}-{}-{}", hex(&m.uuid), fnv(&[path.as_bytes()]));
    let img = Image { name: path.to_string(), key, start: base, end: hi.wrapping_add(slide),
        source: source_bounds(&m, slide), entries, loaded: false };
    std::fs::create_dir_all(cache_dir())?;
    let prof = read_profile(&file_for(&img, "profile"));
    build(&img, &prof)?;
    let blocks = compatible_count(&file_for(&img, "aot")).filter(|&n| n > 0)
        .ok_or_else(|| std::io::Error::other(format!("{path}: no discoverable executable blocks")))?;
    Ok(PrewarmImage { path: path.to_string(), kind, blocks })
}

fn collect_bundle_executables(dir: &std::path::Path, out: &mut BTreeSet<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let path = entry.path();
        if ty.is_dir() {
            collect_bundle_executables(&path, out)?;
        } else if ty.is_file() {
            // Avoid trying to parse arbitrary resources as Mach-O load commands.
            let mut magic = [0u8; 4];
            let mut f = std::fs::File::open(&path)?;
            if f.read(&mut magic)? != 4 { continue; }
            if !matches!(u32::from_be_bytes(magic), crate::macho::FAT_MAGIC | crate::macho::FAT_MAGIC_64)
                && u32::from_le_bytes(magic) != crate::macho::MH_MAGIC_64 { continue; }
            let path = std::fs::canonicalize(path)?.to_string_lossy().into_owned();
            if let Ok(m) = crate::macho::MachO::open_arm64(&path, false) {
                if m.cputype == crate::macho::CPU_TYPE_ARM64 && m.filetype == crate::macho::MH_EXECUTE {
                    out.insert(path);
                }
            }
        }
        // Do not follow symlink directories into cycles or outside the bundle.
        // Their real targets inside the bundle are visited through normal paths.
    }
    Ok(())
}

/// Static ahead-of-time translation of executable targets and dyld. App bundles
/// include ARM executables in nested helper bundles; libraries have unknown
/// runtime slides until dyld maps them and are deliberately not claimed here.
pub fn prewarm_paths(targets: &[String], dyld_path: &str) -> std::io::Result<Vec<PrewarmImage>> {
    if !ENABLED.load(Ordering::Relaxed) || !crate::jit::JIT_ENABLED.load(Ordering::Relaxed) {
        return Err(std::io::Error::other("--prewarm cannot be combined with --no-aot, --interp or debugger mode"));
    }
    let mut executables = BTreeSet::new();
    for target in targets {
        let path = std::fs::canonicalize(target)?;
        if path.is_dir() {
            if !path.extension().is_some_and(|s| s.eq_ignore_ascii_case("app")) {
                return Err(std::io::Error::other(format!("{target}: expected an executable or .app bundle")));
            }
            let mut bundle = BTreeSet::new();
            collect_bundle_executables(&path, &mut bundle)?;
            if bundle.is_empty() {
                return Err(std::io::Error::other(format!("{target}: no arm64 executables in bundle")));
            }
            executables.extend(bundle);
        } else {
            executables.insert(path.to_string_lossy().into_owned());
        }
    }
    if executables.is_empty() { return Err(std::io::Error::other("no prewarm targets")); }
    let mut images = Vec::new();
    for path in executables { images.push(prewarm_image(&path, false)?); }
    images.push(prewarm_image(dyld_path, true)?);
    Ok(images)
}

// An older/incompatible file must not suppress a rebuild or win the
// larger-profile race: it cannot be loaded by this code generator anyway.
fn compatible_header(path: &std::path::Path) -> Option<(usize, u64)> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = [0u8; 40];
    f.read_exact(&mut h).ok()?;
    if &h[..8] != MAGIC
        || u32::from_le_bytes(h[8..12].try_into().unwrap()) != codegen_id()
        || u32::from_le_bytes(h[12..16].try_into().unwrap()) != flags_word()
    {
        return None;
    }
    let n = usize::try_from(u64::from_le_bytes(h[16..24].try_into().unwrap())).ok()?;
    let bytes = usize::try_from(u64::from_le_bytes(h[24..32].try_into().unwrap())).ok()?;
    let end = checked_code_offset(n)?.checked_add(bytes)?;
    let hash = u64::from_le_bytes(h[32..40].try_into().unwrap());
    (f.metadata().ok()?.len() >= end as u64).then_some((n, hash))
}

fn compatible_count(path: &std::path::Path) -> Option<usize> {
    compatible_header(path).map(|(n, _)| n)
}

fn profile_fingerprint(profile: &BTreeSet<u64>) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for pc in profile {
        for byte in pc.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }
    hash
}

/// Function entry points from LC_FUNCTION_STARTS (runtime addresses).
pub fn function_starts(path: &str, m: &crate::macho::MachO, slide: u64) -> Vec<u64> {
    let Some((off, size)) = m.function_starts else { return vec![] };
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    use std::io::Seek;
    let mut buf = vec![0u8; size as usize];
    if f.seek(std::io::SeekFrom::Start(m.slice_offset + off as u64)).is_err() || f.read_exact(&mut buf).is_err() {
        return vec![];
    }
    crate::macho::decode_function_starts(&buf, m.text_vmaddr()).into_iter().map(|a| a.wrapping_add(slide)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_discovery_keeps_fpcr_mode_successor() {
        const PC: u64 = 0x1000_0000;
        for rt in 0..32 {
            let words = [0x91000400, 0xd51b4400 | rt, 0x1e61c020, 0xd65f03c0];
            let read = |p: u64| p.checked_sub(PC).and_then(|d| words.get((d/4) as usize)).copied();
            let found = discover(&[PC], PC, PC+16, &read, 32);
            assert!(found.contains(&PC)); assert!(found.contains(&(PC+8)));
            assert_eq!(found.len(), 2);
        }
    }

    #[test]
    fn static_discovery_keeps_isb_successor() {
        const PC: u64 = 0x1000_0000;
        let words = [0xd503_3fdf, 0x9100_0400, 0xd65f_03c0]; // ISB; ADD; RET
        let read = |pc: u64| {
            pc.checked_sub(PC).and_then(|d| words.get((d / 4) as usize)).copied()
        };
        let found = discover(&[PC], PC, PC + 12, &read, 32);
        assert!(found.contains(&PC));
        assert!(found.contains(&(PC + 4)), "static prewarm must continue beyond ISB");
    }

    #[test]
    fn prewarm_mapping_does_not_overwrite_an_existing_region() {
        let p = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE,
                                    libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
        assert_ne!(p, libc::MAP_FAILED);
        let occupied = PrewarmMapping { start: p as u64, bytes: 4096 };
        unsafe { *(p as *mut u64) = 0xfeed_beef_1234_5678; }
        let m = crate::macho::MachO {
            slice_offset: 0, slice_size: 4096, cputype: crate::macho::CPU_TYPE_ARM64,
            cpusubtype: 0, filetype: crate::macho::MH_EXECUTE,
            segments: vec![crate::macho::Segment { name: "__TEXT".into(), vmaddr: 0x100000000,
                vmsize: 4096, fileoff: 0, filesize: 4096, maxprot: 5, initprot: 5 }],
            main_entryoff: None, thread_pc: None, dylinker: None, function_starts: None,
            uuid: [0; 16], header_bytes: Vec::new(),
        };
        let exe = std::env::current_exe().unwrap();
        assert!(map_prewarm_text(exe.to_str().unwrap(), &m, occupied.start).is_err());
        assert_eq!(unsafe { *(p as *const u64) }, 0xfeed_beef_1234_5678);
    }

    #[test]
    fn checkpoints_publish_idle_and_exited_thread_tails() {
        let start = 0x1234_0000;
        add_image(Image { name: "tail-test".into(), key: "tail-test".into(),
            start, end: start + 0x1000, source: (start, start + 0x1000), entries: vec![], loaded: false });
        let idx = IMAGES.lock().unwrap().iter().position(|i| i.key == "tail-test").unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            note_translated(start);
            ready_tx.send(()).unwrap();
            done_rx.recv().unwrap();
            note_translated(start + 4);
        });
        ready_rx.recv().unwrap();
        assert!(!SEEN.lock().unwrap()[idx].contains(&start));
        flush_all_pending();
        assert!(SEEN.lock().unwrap()[idx].contains(&start));
        done_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(SEEN.lock().unwrap()[idx].contains(&(start + 4)));
    }

    // The integration runner compares this across fresh ON/OFF processes using
    // the same test binary, so UUID/version are held constant.
    #[test]
    fn g4_cache_identity_bits_and_incompatible_headers() {
        assert_eq!(TRANSLATOR_VERSION,99);
        assert_eq!((flags_word()>>11)&1,crate::jit::flagreg::enabled() as u32);
        assert_eq!((flags_word()>>12)&3,crate::jit::isel2_mask());
        assert_eq!((flags_word()>>31)&1,crate::jit::hugepages_enabled() as u32);
        eprintln!("G4_ID={:08x} VERSION={} FLAGS={:x}",codegen_id(),TRANSLATOR_VERSION,flags_word());
        let dir=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/port-g4");
        std::fs::create_dir_all(&dir).unwrap();
        let path=dir.join(format!("header-{}.aot",std::process::id()));
        let mut h=Vec::new();h.extend_from_slice(MAGIC);h.extend_from_slice(&codegen_id().to_le_bytes());
        h.extend_from_slice(&flags_word().to_le_bytes());h.extend_from_slice(&1u64.to_le_bytes());h.extend_from_slice(&16u64.to_le_bytes());h.resize(code_offset(1)+16,0);
        for bit in [11,12,13,31] {
            std::fs::write(&path,&h).unwrap();assert_eq!(compatible_count(&path),Some(1));
            h[12+bit/8]^=1<<(bit%8);std::fs::write(&path,&h).unwrap();
            assert_eq!(compatible_count(&path),None,"effective bit {bit} mismatch must reject AOT");
            h[12+bit/8]^=1<<(bit%8);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn port_g2_codegen_identity() {
        eprintln!("PORT_G2_ID={:08x} VERSION={} FLAGS={:x}", codegen_id(), TRANSLATOR_VERSION, flags_word());
        assert_eq!((flags_word() >> 10) & 1, emit::ibtc2_enabled() as u32);
    }

    #[test]
    fn fastenc_cache_bit_and_namespace() {
        assert_eq!((flags_word() >> 9) & 1, crate::jit::fastenc_enabled() as u32);
        let img = Image { name: "test".into(), key: "private-test".into(), start: 0, end: 4,
            source: (0, 4), entries: vec![], loaded: false };
        let expected = if crate::jit::fastenc_enabled() { "private-test-fastenc.aot" } else { "private-test.aot" };
        assert_eq!(file_for(&img, "aot").file_name().unwrap(), expected);
    }

    #[test]
    fn avx512_selection_is_part_of_cache_compatibility() {
        assert_eq!((flags_word() >> 6) & 1, emit::has_avx512() as u32);
        assert_eq!((flags_word() >> 7) & 1, emit::avx512_enabled() as u32);
        assert!(!emit::avx512_enabled() || emit::has_avx512());
        assert_eq!((flags_word() >> 8) & 1, emit::avx512_nan_enabled() as u32);
        assert!(!emit::avx512_nan_enabled() || emit::has_avx512());
        let path = std::env::temp_dir().join(format!("attesor-aot-avx512-{}", std::process::id()));
        let mut h = Vec::new();
        h.extend_from_slice(MAGIC);
        h.extend_from_slice(&codegen_id().to_le_bytes());
        h.extend_from_slice(&flags_word().to_le_bytes());
        h.extend_from_slice(&1u64.to_le_bytes());
        h.extend_from_slice(&16u64.to_le_bytes());
        h.resize(code_offset(1) + 16, 0);
        for bit in [6, 7, 8] {
            std::fs::write(&path, &h).unwrap();
            assert_eq!(compatible_count(&path), Some(1));
            h[12 + bit / 8] ^= 1 << (bit % 8);
            std::fs::write(&path, &h).unwrap();
            assert_eq!(compatible_count(&path), None, "host/selected AVX-512 choice must reject cache");
            h[12 + bit / 8] ^= 1 << (bit % 8);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn incompatible_larger_cache_does_not_block_replacement() {
        let path = std::env::temp_dir().join(format!("attesor-aot-header-{}", std::process::id()));
        let mut h = Vec::new();
        h.extend_from_slice(MAGIC);
        h.extend_from_slice(&codegen_id().to_le_bytes());
        h.extend_from_slice(&flags_word().to_le_bytes());
        h.extend_from_slice(&100u64.to_le_bytes());
        h.extend_from_slice(&16u64.to_le_bytes());
        h.resize(code_offset(100) + 16, 0);
        std::fs::write(&path, &h).unwrap();
        assert_eq!(compatible_count(&path), Some(100));
        h[8] ^= 0x80;
        std::fs::write(&path, &h).unwrap();
        assert_eq!(compatible_count(&path), None);
        h[8] ^= 0x80;
        h[12] ^= 0x80;
        std::fs::write(&path, &h).unwrap();
        assert_eq!(compatible_count(&path), None);
        h[12] ^= 0x80;
        h[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        std::fs::write(&path, &h).unwrap();
        assert_eq!(compatible_count(&path), None);
        assert_eq!(load_file(&path, None).unwrap(), 0);
        std::fs::remove_file(path).unwrap();
    }
}

/// Directed differential CLI suite: serialize and actually reload a private
/// page through exactly the production builder/header/index/loader path.
/// The caller owns ATTESOR_CACHE and the mapped page for the whole operation.
pub(crate) fn selftest_page(start: u64, words: usize) -> std::io::Result<usize> {
    let img = Image { name: "offline-selftest".into(), key: "offline-selftest".into(),
        start, end: start + words as u64 * 4,
        source: (start, start + words as u64 * 4), entries: (0..words).map(|n| start + n as u64 * 4).collect(), loaded: false };
    std::fs::create_dir_all(cache_dir())?;
    let path = file_for(&img, "aot");
    if path.exists() { std::fs::remove_file(&path)?; }
    build(&img, &BTreeSet::new())?;
    load_file(&path, Some(img.source))
}
    #[test]
    fn precise_aot_reader_never_observes_outside_image() {
        let calls = std::cell::Cell::new(0);
        let read = |_| { calls.set(calls.get() + 1); Some(0xd503201f) };
        assert_eq!(image_word(0x1000, 0x1008, true, 0x1000, &read), Some(0xd503201f));
        assert_eq!(image_word(0x1000, 0x1008, true, 0x1004, &read), Some(0xd503201f));
        for pc in [0xffc, 0x1005, 0x1008, u64::MAX] {
            assert_eq!(image_word(0x1000, 0x1008, true, pc, &read), None);
        }
        assert_eq!(calls.get(), 2);
        assert_eq!(image_word(0x1000, 0x1008, false, 0x1008, &read), Some(0xd503201f));
    }
