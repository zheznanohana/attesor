//! Opt-in native chained fixups, using dyld's final (resolved/interposed) binds.
//! Hook only dyld's in-process fallback after map_with_linking_np returned
//! ENOTSUP. Symbol resolution, cache patching and segment protections stay guest.
//! The fallback's private mwl_info ABI is pinned by UUID, symbol and code hash.
//! Unknown dyld builds / formats execute the original guest implementation.
use attesor_core::cpu::Cpu;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

type Result<T> = std::result::Result<T, &'static str>;
const UUID: [u8; 16] = [0x57,0xf4,0xe7,0xe7,0xa4,0x97,0x38,0xbb,0xa3,0xd2,0xa4,0x59,0xc6,0x22,0x46,0x0b];
const ENTRY: u64 = 0x4dfbc;
const FALLBACK_HASH: u64 = 0xbb1b74bf045843f7;
const SYMBOL: &[u8] = b"__ZN5dyld4L24dyld_map_with_linking_npEPK10mwl_regionjPK12mwl_info_hdrj";
const SVC: u16 = 0x7f30;
const RETURN_SVC: u16 = 0x7f31;
const PACIBSP: u32 = 0xd503237f; // identity under the existing guest PAC model
const WORKERS: usize = 4;
const PARALLEL_PAGES: usize = 128;
const PAGE: usize = 4096; // host pages on the supported x86-64 macOS runtime
static HOOK: AtomicU64 = AtomicU64::new(0);
static RETURN: AtomicU64 = AtomicU64::new(0);
thread_local! { static TIMING: RefCell<Vec<(u64, u64, Instant)>> = const { RefCell::new(Vec::new()) }; }
thread_local! { static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) }; }

fn selected(on: &str, off: &str) -> bool {
    on.split(',').any(|s| s == "nativefixups") && !off.split(',').any(|s| s == "nativefixups")
}
fn enabled() -> bool {
    selected(&std::env::var("ATTESOR_JIT_ON").unwrap_or_default(),
             &std::env::var("ATTESOR_JIT_OFF").unwrap_or_default())
}
fn hash(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &v| (h ^ v as u64).wrapping_mul(0x100_0000_01b3))
}
fn u16_at(b: &[u8], p: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(b.get(p..p.checked_add(2).ok_or("overflow")?).ok_or("short u16")?.try_into().unwrap()))
}
fn u32_at(b: &[u8], p: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(b.get(p..p.checked_add(4).ok_or("overflow")?).ok_or("short u32")?.try_into().unwrap()))
}
fn u64_at(b: &[u8], p: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(b.get(p..p.checked_add(8).ok_or("overflow")?).ok_or("short u64")?.try_into().unwrap()))
}
fn read(addr: u64, len: usize) -> Result<Vec<u8>> {
    let mut b = vec![0; len];
    if len != 0 && !crate::guestmem::read(addr, &mut b) { return Err("unreadable guest memory"); }
    Ok(b)
}
fn commands(b: &[u8]) -> Result<Vec<(u32, &[u8])>> {
    if u32_at(b, 0)? != 0xfeedfacf || u32_at(b, 4)? != 0x100000c { return Err("not arm64 Mach-O"); }
    let count = u32_at(b, 16)? as usize;
    if count > 4096 || u32_at(b, 20)? as usize != b.len() - 32 { return Err("bad load commands"); }
    let mut out = Vec::new();
    let mut p = 32;
    for _ in 0..count {
        let size = u32_at(b, p+4)? as usize;
        if size < 8 { return Err("short load command"); }
        let end = p.checked_add(size).ok_or("load command overflow")?;
        out.push((u32_at(b, p)?, b.get(p..end).ok_or("load command outside header")?));
        p = end;
    }
    if p != b.len() { return Err("load command size mismatch"); }
    Ok(out)
}

fn audited(m: &crate::macho::MachO, slide: u64) -> Result<u64> {
    if m.uuid != UUID || m.filetype != crate::macho::MH_DYLINKER { return Err("unaudited dyld UUID"); }
    let addr = ENTRY.checked_add(slide).ok_or("entry overflow")?;
    let b = read(addr, 0x28c)?;
    if hash(&b) != FALLBACK_HASH || u32_at(&b, 0)? != PACIBSP { return Err("fallback bytes differ"); }
    Ok(addr)
}

/// Called before AOT registration/execution, and on the offline prewarm mapping.
/// COW/mprotect touches only this process's mapping, never the dyld file.
pub fn install(path: &str, m: &crate::macho::MachO, slide: u64) {
    if !enabled() { return; }
    let result = (|| -> Result<u64> {
        let addr = audited(m, slide)?;
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path).map_err(|_| "dyld file open")?;
        let mut found = false;
        for (cmd, b) in commands(&m.header_bytes)? {
            if cmd != crate::macho::LC_SYMTAB { continue; }
            let off = u32_at(b, 8)? as u64;
            let count = u32_at(b, 12)? as usize;
            let stroff = u32_at(b, 16)? as u64;
            let strsize = u32_at(b, 20)? as usize;
            if count > 100_000 || strsize > 16 << 20 { return Err("large dyld symtab"); }
            let mut table = vec![0; count*16];
            let mut strings = vec![0; strsize];
            file.seek(SeekFrom::Start(m.slice_offset + off)).map_err(|_| "symtab seek")?;
            file.read_exact(&mut table).map_err(|_| "symtab read")?;
            file.seek(SeekFrom::Start(m.slice_offset + stroff)).map_err(|_| "strtab seek")?;
            file.read_exact(&mut strings).map_err(|_| "strtab read")?;
            for n in table.chunks_exact(16) {
                let p = u32_at(n, 0)? as usize;
                if n[4] & 0xe0 != 0 || n[4] & 0x0e != 0x0e { continue; }
                if strings.get(p..).is_some_and(|s| s.starts_with(SYMBOL) && s.get(SYMBOL.len()) == Some(&0))
                    && u64_at(n, 8)? == ENTRY { found = true; }
            }
        }
        if !found { return Err("fallback symbol differs"); }

        let page = addr & !(PAGE as u64 - 1);
        unsafe {
            if libc::mprotect(page as *mut _, PAGE, libc::PROT_READ | libc::PROT_WRITE) != 0 { return Err("patch mprotect"); }
            std::ptr::write_unaligned(addr as *mut u32, 0xd4000001 | ((SVC as u32) << 5));
            if libc::mprotect(page as *mut _, PAGE, libc::PROT_READ) != 0 { panic!("nativefixups: restore text protection"); }
        }
        let p = unsafe { libc::mmap(std::ptr::null_mut(), PAGE, libc::PROT_READ | libc::PROT_WRITE,
                                   libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0) };
        if p == libc::MAP_FAILED { panic!("nativefixups: return trampoline allocation"); }
        unsafe {
            std::ptr::write(p as *mut u32, 0xd4000001 | ((RETURN_SVC as u32) << 5));
            if libc::mprotect(p, PAGE, libc::PROT_READ) != 0 { panic!("nativefixups: return protection"); }
        }
        RETURN.store(p as u64, Ordering::Relaxed);
        HOOK.store(addr, Ordering::Relaxed);
        Ok(addr)
    })();
    match result {
        Ok(addr) => eprintln!("[attesor] nativefixups: installed entry={addr:#x} verify={}", std::env::var_os("ATTESOR_NATIVEFIXUPS_VERIFY").is_some()),
        Err(why) => eprintln!("[attesor] nativefixups: disabled: {why}"),
    }
}

// Metadata only in normal mode: no full-page copies/dirty bitmaps. The VM
// preflight covers every page BEFORE any direct read or store. As in guest dyld,
// the loader lock must exclude concurrent unmap/mprotect of this image.
struct Page {
    addr: u64,
    len: usize,
    start: usize,
    delta: u64,
}
struct Plan {
    pages: Vec<Page>,
    targets: Vec<u64>,
    // Only verification/tests retain full pages (including untouched bytes).
    expected: Vec<Vec<u8>>,
    rebases: usize,
    binds: usize,
    header: u64,
    vm_ns: u128,
}
impl Plan {
    fn writable(&self) -> Result<()> {
        extern "C" {
            fn mach_port_deallocate(task: u32, name: u32) -> i32;
            fn mach_vm_region(task: u32, addr: *mut u64, size: *mut u64, flavor: i32,
                              info: *mut i32, count: *mut u32, object: *mut u32) -> i32;
        }
        let mut region_end = 0;
        let task = unsafe { libc::mach_task_self() };
        for page in &self.pages {
            let addr = page.addr;
            let end = addr.checked_add(page.len as u64).ok_or("page end overflow")?;
            let mut at = addr;
            while at < end {
                if at < region_end { at = end.min(region_end); continue; }
                let (mut base, mut size, mut count, mut object) = (at, 0, 16, 0);
                let mut info = [0i32; 16];
                let rc = unsafe { crate::profile::host_call(|| mach_vm_region(task, &mut base, &mut size, 9, info.as_mut_ptr(), &mut count, &mut object)) };
                if object != 0 { unsafe { mach_port_deallocate(task, object); } }
                region_end = base.checked_add(size).ok_or("VM region overflow")?;
                if rc != 0 || base > at || region_end <= at || count < 9 || info[0] & 3 != 3 {
                    return Err("fixup pages not readable/writable");
                }
            }
        }
        Ok(())
    }
    fn apply(&self) {
        if self.pages.len() >= PARALLEL_PAGES {
            std::thread::scope(|scope| {
                for pages in self.pages.chunks(self.pages.len().div_ceil(WORKERS)) {
                    // No persistent pool/locks to inherit across guest fork.
                    // Disjoint pages, loader lock held until all workers join.
                    let work = move || {
                        for page in pages { unsafe { apply_page(page, &self.targets); } }
                    };
                    if std::thread::Builder::new().stack_size(256*1024)
                        .spawn_scoped(scope, work).is_err() { work(); }
                }
            });
        } else {
            for page in &self.pages { unsafe { apply_page(page, &self.targets); } }
        }
    }
    fn restore(&self, originals: &[Vec<u8>]) {
        for (page, bytes) in self.pages.iter().zip(originals) {
            let mut off = page.start;
            loop {
                let raw = u64::from_le_bytes(bytes[off..off+8].try_into().unwrap());
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr().add(off), (page.addr as *mut u8).add(off), 8); }
                let next = ((raw >> 51) & 0xfff) as usize * 4;
                if next == 0 { break; }
                off += next;
            }
        }
    }
    fn compare(&self) -> Result<()> {
        assert_eq!(self.pages.len(), self.expected.len(), "comparison requires verification snapshots");
        for (page, expected) in self.pages.iter().zip(&self.expected) {
            if unsafe { std::slice::from_raw_parts(page.addr as *const u8, page.len) } != expected.as_slice() { return Err("fixed-up page differs from guest dyld"); }
        }
        Ok(())
    }
    fn decode_pages(&mut self, binds: &[u64]) -> Result<()> {
        self.targets = binds.to_vec();
        let counts = if self.pages.len() >= PARALLEL_PAGES {
            std::thread::scope(|scope| {
                let mut workers = Vec::with_capacity(WORKERS);
                let mut counts = (0, 0);
                for pages in self.pages.chunks(self.pages.len().div_ceil(WORKERS)) {
                    match std::thread::Builder::new().stack_size(256*1024)
                        .spawn_scoped(scope, move || validate_pages(pages, binds)) {
                        Ok(worker) => workers.push(worker),
                        Err(_) => {
                            let (rebases, binds) = validate_pages(pages, binds)?;
                            counts.0 += rebases; counts.1 += binds;
                        }
                    }
                }
                for worker in workers {
                    let (rebases, binds) = worker.join().expect("nativefixups validation worker")?;
                    counts.0 += rebases; counts.1 += binds;
                }
                Ok(counts)
            })?
        } else { validate_pages(&self.pages, binds)? };
        if counts.0 + counts.1 > 2_000_000 { return Err("too many fixups"); }
        self.rebases = counts.0; self.binds = counts.1;
        Ok(())
    }

    fn snapshot_expected(&mut self) -> Result<()> {
        // Do not use mach_vm_read_overwrite here: a VM-assisted snapshot can
        // introduce COW state and make the first native write unlike dyld's.
        self.expected = self.pages.iter().map(|p| unsafe {
            std::slice::from_raw_parts(p.addr as *const u8, p.len).to_vec()
        }).collect();
        for (page, bytes) in self.pages.iter().zip(&mut self.expected) {
            let mut off = page.start;
            loop {
                let raw = u64::from_le_bytes(bytes[off..off+8].try_into().unwrap());
                let (fixed, _) = decode_delta(raw, page.delta, &self.targets)?;
                bytes[off..off+8].copy_from_slice(&fixed.to_le_bytes());
                let next = ((raw >> 51) & 0xfff) as usize * 4;
                if next == 0 { break; }
                off += next;
            }
        }
        Ok(())
    }
}

fn validate_pages(pages: &[Page], binds: &[u64]) -> Result<(usize, usize)> {
    let (mut rebases, mut binds_count) = (0, 0);
    // One metadata record per chain page, not per pointer/run. Validate
    // the complete image before the first store, then replay its chains.
    for page in pages {
        let mut off = page.start;
        loop {
            if off & 3 != 0 || off + 8 > page.len { return Err("chain outside page"); }
            let raw = unsafe { std::ptr::read_unaligned((page.addr as *const u8).add(off).cast::<u64>()) };
            let stride = ((raw >> 51) & 0xfff) as usize * 4;
            if raw >> 63 == 0 && stride >= 8 && off + 3*stride + 8 <= page.len {
                let mut different = 0u64;
                for i in 0..4 {
                    let word = unsafe { std::ptr::read_unaligned((page.addr as *const u8).add(off+i*stride).cast::<u64>()) };
                    different |= word ^ ((stride as u64 / 4) << 51);
                }
                if different & (u64::MAX << 44) == 0 {
                    rebases += 4;
                    off += 4*stride;
                    continue;
                }
            }
            decode_delta(raw, page.delta, binds)?;
            if raw >> 63 != 0 { binds_count += 1; } else { rebases += 1; }
            let next = ((raw >> 51) & 0xfff) as usize * 4;
            if next == 0 { break; }
            if next < 8 { return Err("overlapping fixups"); }
            off += next;
        }
        if rebases + binds_count > 2_000_000 { return Err("too many fixups"); }
    }
    Ok((rebases, binds_count))
}

// All slots and ordinals have been validated, with no stores on failure.
// Four equal-stride rebases remove the serial chain dependency: dense stores
// are a 32-byte copy and independent arithmetic is available to LLVM's SIMD
// vectorizer. Sparse batches write only their four slots, never gap bytes.
// A scalar tail handles binds, changing strides, and unaligned pointer slots.
unsafe fn apply_page(page: &Page, binds: &[u64]) {
    let mut off = page.start;
    loop {
        let p = (page.addr as *mut u8).add(off).cast::<u64>();
        let raw = std::ptr::read_unaligned(p);
        let stride = ((raw >> 51) & 0xfff) as usize * 4;
        if raw >> 63 == 0 && stride >= 8 && off + 3*stride + 8 <= page.len {
            let mut words = [0u64; 4];
            for i in 0..4 { words[i] = std::ptr::read_unaligned((page.addr as *const u8).add(off+i*stride).cast::<u64>()); }
            let mut different = 0u64;
            for word in &words { different |= *word ^ ((stride as u64 / 4) << 51); }
            if different & (u64::MAX << 44) == 0 {
                if stride == 8 {
                    // Extend a validated dense prefix. A runtime-length loop
                    // (rather than four scalar array expressions) lets LLVM
                    // vectorize unaligned mapped-page loads/arithmetic/stores.
                    let mut end = off + 32;
                    while end + 8 <= page.len {
                        let raw = std::ptr::read_unaligned((page.addr as *const u8).add(end).cast::<u64>());
                        if raw >> 44 != (2 << 7) { break; }
                        end += 8;
                    }
                    for i in 0..(end-off)/8 {
                        let slot = p.cast::<u8>().add(i*8).cast::<u64>();
                        let raw = std::ptr::read_unaligned(slot);
                        let value = (raw & 0xfffffffff).wrapping_add(page.delta)
                            .wrapping_add(((raw >> 36) & 255) << 56);
                        std::ptr::write_unaligned(slot, value);
                    }
                    off = end;
                    continue;
                } else {
                    for i in 0..4 {
                        let raw = words[i];
                        let value = (raw & 0xfffffffff).wrapping_add(page.delta)
                            .wrapping_add(((raw >> 36) & 255) << 56);
                        std::ptr::write_unaligned((page.addr as *mut u8).add(off+i*stride).cast::<u64>(), value);
                    }
                }
                off += 4*stride;
                continue;
            }
        }
        let value = if raw >> 63 != 0 {
            binds.get_unchecked((raw & 0xffffff) as usize).wrapping_add((raw >> 24) & 255)
        } else {
            (raw & 0xfffffffff).wrapping_add(page.delta).wrapping_add(((raw >> 36) & 255) << 56)
        };
        std::ptr::write_unaligned(p, value);
        let next = ((raw >> 51) & 0xfff) as usize * 4;
        if next == 0 { break; }
        off += next;
    }
}

#[cfg(test)]
fn decode(raw: u64, format: u16, header: u64, preferred: u64, binds: &[u64]) -> Result<(u64, bool)> {
    if !matches!(format, 2 | 6) { return Err("unsupported pointer format"); }
    decode_delta(raw, if format == 2 { header.wrapping_sub(preferred) } else { header }, binds)
}

#[inline(always)]
fn decode_delta(raw: u64, delta: u64, binds: &[u64]) -> Result<(u64, bool)> {
    if raw >> 63 != 0 {
        if (raw >> 32) & 0x7ffff != 0 { return Err("reserved bind bits"); }
        let ordinal = (raw & 0xffffff) as usize;
        let target = *binds.get(ordinal).ok_or("out of range bind ordinal")?;
        Ok((target.wrapping_add((raw >> 24) & 255), true))
    } else {
        if (raw >> 44) & 127 != 0 { return Err("reserved rebase bits"); }
        let low = raw & 0xfffffffff;
        Ok((low.wrapping_add(delta).wrapping_add(((raw >> 36) & 255) << 56), false))
    }
}

// Parse and preflight the ENTIRE image before changing a byte. Refuse malformed,
// unsupported and multi-start chains, leaving dyld untouched.
fn plan(header: u64, binds: &[u64], starts: &[u8], slide: u64) -> Result<Plan> {
    let first = read(header, 32)?;
    let size = u32_at(&first, 20)? as usize;
    if size > 1 << 20 { return Err("large Mach-O header"); }
    let bytes = read(header, 32 + size)?;
    let cmds = commands(&bytes)?;
    let mut segs = Vec::new();
    let mut preferred = None;
    for (cmd, b) in cmds {
        if cmd == 0x19 {
            let vm = u64_at(b, 24)?;
            if b.get(8..24).is_some_and(|n| n.starts_with(b"__TEXT\0")) { preferred = Some(vm); }
            segs.push((vm, u64_at(b, 32)?, u64_at(b, 40)?, u64_at(b, 48)?, u32_at(b, 60)?));
        }
    }
    let preferred = preferred.ok_or("missing text")?;
    if header.wrapping_sub(preferred) != slide { return Err("mwl slide disagrees with image"); }
    let count = u32_at(starts, 0)? as usize;
    if count > segs.len() { return Err("too many fixup segments"); }

    let mut plan = Plan { pages: Vec::new(), targets: Vec::new(), expected: Vec::new(), rebases: 0, binds: 0, header, vm_ns: 0 };
    for i in 0..count {
        let off = u32_at(starts, 4 + i*4)? as usize;
        if off == 0 { continue; }
        let p = off;
        let size = u32_at(starts, p)? as usize;
        let b = starts.get(p..p.checked_add(size).ok_or("segment info overflow")?).ok_or("segment info bounds")?;
        let page_size = u16_at(b, 4)? as u64;
        let format = u16_at(b, 6)?;
        let segment_offset = u64_at(b, 8)?;
        let pages = u16_at(b, 20)? as usize;
        if !matches!(format, 2 | 6) || !matches!(page_size, 4096 | 16384) { return Err("unsupported chain format/page size"); }
        let seg = segs.iter().find(|s| s.0.wrapping_sub(preferred) == segment_offset).ok_or("chain segment not mapped")?;
        if seg.4 & 2 == 0 || seg.4 & 4 != 0 { return Err("fixups outside writable non-code segment"); }
        if segment_offset != seg.0.wrapping_sub(preferred) || pages as u64 * page_size > seg.1
            || 22 + pages*2 > b.len() { return Err("segment starts bounds"); }
        for page in 0..pages {
            let start = u16_at(b, 22 + page*2)?;
            if start == 0xffff { continue; }
            if start & 0x8000 != 0 { return Err("multi-start page"); }
            let base = header.checked_add(segment_offset).and_then(|v| v.checked_add(page as u64 * page_size)).ok_or("page address overflow")?;
            base.checked_add(page_size).ok_or("page end overflow")?;
            if base & (PAGE as u64 - 1) != 0 { return Err("unaligned/overlapping chain pages"); }
            plan.pages.push(Page { addr: base, len: page_size as usize, start: start as usize,
                                   delta: if format == 2 { slide } else { header } });
        }
    }
    plan.pages.sort_unstable_by_key(|p| p.addr);
    if plan.pages.windows(2).any(|p| p[0].addr + p[0].len as u64 > p[1].addr) {
        return Err("unaligned/overlapping chain pages");
    }
    let vm = Instant::now();
    plan.writable()?;
    plan.vm_ns = vm.elapsed().as_nanos();
    plan.decode_pages(binds)?;
    Ok(plan)
}

fn from_blob(addr: u64, size: u64) -> Result<Plan> {
    // Audited fallback actually takes (mwl_info_hdr*, blob_size),
    // despite retaining an older four-argument name in its local symbol.
    let size = size as usize;
    if !(40..=32 << 20).contains(&size) { return Err("bad mwl blob size"); }
    let blob = read(addr, size)?;
    if u32_at(&blob, 0)? != 7 { return Err("unsupported mwl version"); }
    let format = u16_at(&blob, 6)?;
    if !matches!(format, 2 | 6) { return Err("unsupported mwl pointer format"); }
    let ptr = u32_at(&blob, 8)? as usize;
    let count = u32_at(&blob, 12)? as usize;
    let start = u32_at(&blob, 16)? as usize;
    let len = u32_at(&blob, 20)? as usize;
    let header = u64_at(&blob, 32)?;
    let bytes = blob.get(ptr..ptr.checked_add(count.checked_mul(8).ok_or("bind count overflow")?).ok_or("bind size overflow")?).ok_or("bind array bounds")?;
    let binds: Vec<u64> = bytes.chunks_exact(8).map(|b| u64::from_le_bytes(b.try_into().unwrap())).collect();
    let starts = blob.get(start..start.checked_add(len).ok_or("starts size overflow")?).ok_or("starts bounds")?;
    // Each segment must agree with the blob-wide format and page size.
    let n = u32_at(starts, 0)? as usize;
    if n > 4096 { return Err("too many segments"); }
    for i in 0..n {
        let p = u32_at(starts, 4 + i*4)? as usize;
        if p != 0 && (u16_at(starts, p+6)? != format || u16_at(starts, p+4)? != u16_at(&blob, 4)?) {
            return Err("mwl segment format disagreement");
        }
    }
    plan(header, &binds, starts, u64_at(&blob, 24)?)
}

struct Pending {
    plan: Plan, lr: u64, started: Instant, native_ns: u128,
    // Optional timing control: guest gets first-write COW cost, then native
    // replans/writes the restored encoding on the already-private pages.
    guest_first: Option<(u64, u64, Vec<Vec<u8>>)>,
}
fn log(plan: &Plan, mode: &str, native_ns: u128, guest_ns: u128) {
    if std::env::var_os("ATTESOR_STATS").is_some() || mode == "verified" {
        eprintln!("[attesor] nativefixups: mode={mode} image={:#x} pages={} rebases={} binds={} native_ns={native_ns} guest_ns={guest_ns}",
                  plan.header, plan.pages.iter().map(|p| p.len / PAGE).sum::<usize>(), plan.rebases, plan.binds);
    }
}

/// SVCs are accepted only at our exact patched PC / private return trampoline.
/// Unsupported inputs execute the saved PACIBSP then continue original dyld.
pub fn handle_svc(cpu: &mut Cpu, imm: u16) -> bool {
    let entry = HOOK.load(Ordering::Relaxed);
    if entry == 0 { return false; }
    if imm == RETURN_SVC && cpu.pc() == RETURN.load(Ordering::Relaxed) + 4 {
        if let Some((lr, header, start)) = TIMING.with(|p| p.borrow_mut().pop()) {
            eprintln!("[attesor] nativefixups: mode=guest-timed image={header:#x} pages=0 rebases=0 binds=0 native_ns=0 guest_ns={}", start.elapsed().as_nanos());
            cpu.x[30] = lr;
            cpu.set_pc(lr);
            return true;
        }
        let pending = PENDING.with(|p| p.borrow_mut().pop()).expect("nativefixups: unmatched verification return");
        let guest_ns = pending.started.elapsed().as_nanos();
        pending.plan.compare().unwrap_or_else(|e| panic!("nativefixups: {e} image={:#x}", pending.plan.header));
        let native_ns = if let Some((addr, size, originals)) = &pending.guest_first {
            pending.plan.restore(originals);
            let start = Instant::now();
            let plan = from_blob(*addr, *size).expect("nativefixups: replan restored input");
            let planned_ns = start.elapsed().as_nanos();
            let write = Instant::now();
            plan.apply();
            let write_ns = write.elapsed().as_nanos();
            eprintln!("[attesor] nativefixups-phases: plan_ns={} vm_ns={} write_ns={write_ns}", planned_ns - plan.vm_ns, plan.vm_ns);
            pending.plan.compare().expect("nativefixups: guest-first native writer");
            planned_ns + write_ns
        } else { pending.native_ns };
        log(&pending.plan, "verified", native_ns, guest_ns);
        cpu.x[30] = pending.lr;
        cpu.set_pc(pending.lr);
        return true;
    }
    if imm != SVC || cpu.pc() != entry + 4 { return false; }
    // Timing control: no planning, snapshots or page touches before guest
    // dyld. It pays its own page-in/COW costs just as the native fast path does.
    if std::env::var_os("ATTESOR_NATIVEFIXUPS_GUEST_TIMING").is_some() {
        let header = read(cpu.x[0], 40).and_then(|b| u64_at(&b, 32)).unwrap_or(0);
        TIMING.with(|p| p.borrow_mut().push((cpu.x[30], header, Instant::now())));
        cpu.x[30] = RETURN.load(Ordering::Relaxed);
        return true;
    }
    let started = Instant::now();
    let result = from_blob(cpu.x[0], cpu.x[1]);
    let planned_ns = started.elapsed().as_nanos();
    match result {
        Ok(mut plan) => {
            let mut native_ns = planned_ns;
            if std::env::var_os("ATTESOR_NATIVEFIXUPS_VERIFY").is_some() {
                // Exercise the actual native writer, snapshot its full pages,
                // restore only the encoded slots, then let unmodified guest
                // dyld run on the identical input. No ASLR normalization/hashes.
                plan.snapshot_expected().expect("nativefixups: verification expected pages");
                let originals: Vec<_> = plan.pages.iter().map(|p|
                    unsafe { std::slice::from_raw_parts(p.addr as *const u8, p.len).to_vec() }).collect();
                let guest_first = if std::env::var("ATTESOR_NATIVEFIXUPS_VERIFY").as_deref() == Ok("guest-first") {
                    Some((cpu.x[0], cpu.x[1], originals))
                } else {
                    let apply_start = Instant::now();
                    plan.apply();
                    let write_ns = apply_start.elapsed().as_nanos();
                    eprintln!("[attesor] nativefixups-phases: plan_ns={} vm_ns={} write_ns={write_ns}", planned_ns - plan.vm_ns, plan.vm_ns);
                    native_ns += write_ns;
                    plan.compare().expect("nativefixups: native writer differs from plan");
                    plan.restore(&originals);
                    None
                };
                PENDING.with(|p| p.borrow_mut().push(Pending { plan, lr: cpu.x[30], started: Instant::now(), native_ns, guest_first }));
                cpu.x[30] = RETURN.load(Ordering::Relaxed);
                // PACIBSP is identity; resume after the patched instruction.
            } else {
                let write = Instant::now();
                plan.apply();
                let write_ns = write.elapsed().as_nanos();
                if std::env::var_os("ATTESOR_STATS").is_some() { eprintln!("[attesor] nativefixups-phases: plan_ns={} vm_ns={} write_ns={write_ns}", planned_ns - plan.vm_ns, plan.vm_ns); }
                log(&plan, "native", planned_ns + write_ns, 0);
                cpu.x[0] = 0; // fallback returns success
                cpu.set_pc(cpu.x[30]);
            }
        }
        Err(why) => {
            if std::env::var_os("ATTESOR_STATS").is_some() { eprintln!("[attesor] nativefixups: fallback: {why}"); }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    fn put16(b: &mut [u8], p: usize, x: u16) { b[p..p+2].copy_from_slice(&x.to_le_bytes()); }
    fn put32(b: &mut [u8], p: usize, x: u32) { b[p..p+4].copy_from_slice(&x.to_le_bytes()); }
    fn put64(b: &mut [u8], p: usize, x: u64) { b[p..p+8].copy_from_slice(&x.to_le_bytes()); }
    struct Fixture { addr: u64, blob: Vec<u8> }
    impl Fixture {
        fn new() -> Self { Self::with_page(4096) }
        fn with_page(page: usize) -> Self {
            let addr = unsafe { libc::mmap(std::ptr::null_mut(), 32768, 3, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0) };
            assert_ne!(addr, libc::MAP_FAILED);
            let b = unsafe { std::slice::from_raw_parts_mut(addr as *mut u8, 32768) };
            put32(b, 0, 0xfeedfacf); put32(b, 4, 0x100000c); put32(b, 16, 2); put32(b, 20, 144);
            for (p, name, vm) in [(32, b"__TEXT\0".as_slice(), 0x100000000), (104, b"__DATA\0".as_slice(), 0x100000000 + page as u64)] {
                put32(b, p, 0x19); put32(b, p+4, 72); b[p+8..p+8+name.len()].copy_from_slice(name);
                put64(b, p+24, vm); put64(b, p+32, page as u64); put32(b, p+60, 3);
            }
            b[page..page*2].fill(0xa5);
            put64(b, page, 0x100000123 | (2 << 51));
            put64(b, page+8, (1 << 63) | (255 << 24));
            let mut blob = vec![0; 80];
            put32(&mut blob, 0, 7); put16(&mut blob, 4, page as u16); put16(&mut blob, 6, 2);
            put32(&mut blob, 8, 72); put32(&mut blob, 12, 1); put32(&mut blob, 16, 40); put32(&mut blob, 20, 32);
            put64(&mut blob, 24, (addr as u64).wrapping_sub(0x100000000)); put64(&mut blob, 32, addr as u64);
            put32(&mut blob, 40, 1); put32(&mut blob, 44, 8);
            put32(&mut blob, 48, 24); put16(&mut blob, 52, page as u16); put16(&mut blob, 54, 2);
            put64(&mut blob, 56, page as u64); put16(&mut blob, 68, 1); put16(&mut blob, 70, 0);
            put64(&mut blob, 72, 0x12340000);
            Self { addr: addr as u64, blob }
        }
        fn plan(&self) -> Result<Plan> {
            let mut p = from_blob(self.blob.as_ptr() as u64, self.blob.len() as u64)?;
            p.snapshot_expected()?;
            Ok(p)
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { unsafe { libc::munmap(self.addr as *mut _, 32768); } } }

    #[test] fn flag_off_wins_and_exact_tokens() {
        assert!(selected("x,nativefixups", "other"));
        assert!(!selected("nativefixups", "x,nativefixups"));
        assert!(!selected("nativefixups-check", ""));
        assert!(!selected("", ""));
    }
    #[test] fn pointer_formats_high8_addends_and_bounds() {
        assert_eq!(decode((255 << 24) | (1 << 63), 2, 0, 0, &[u64::MAX-128]), Ok((126, true)));
        assert_eq!(decode((1 << 63) | 1, 2, 0, 0, &[0]), Err("out of range bind ordinal"));
        assert_eq!(decode((0xab << 36) | 0x123, 2, 0x500000000, 0x100000000, &[]), Ok((0xab00000400000123, false)));
        assert_eq!(decode((0xab << 36) | 0x123, 6, 0x500000000, 0x100000000, &[]), Ok((0xab00000500000123, false)));
        assert!(decode(1 << 44, 2, 0, 0, &[]).is_err());
        assert!(decode((1 << 63) | (1 << 32), 2, 0, 0, &[0]).is_err());
        assert!(decode(0, 12, 0, 0, &[]).is_err());
    }
    #[test] fn all_bytes_match_and_untouched_bytes_are_not_written() {
        let f = Fixture::new(); let p = f.plan().unwrap();
        assert_eq!((p.rebases, p.binds), (1, 1));
        assert!(p.compare().is_err()); // encoded input is NOT a fixed page
        // A concurrent update outside pointer slots must not be overwritten.
        assert!(crate::guestmem::write(f.addr+4200, &[0x11]));
        p.apply();
        assert_eq!(crate::guestmem::read_u64(f.addr+4096), Some(f.addr+0x123));
        assert_eq!(crate::guestmem::read_u64(f.addr+4104), Some(0x123400ff));
        assert_eq!(read(f.addr+4200, 1).unwrap(), [0x11]);
        assert!(p.compare().is_err()); // detects non-pointer byte mismatch too
        assert!(crate::guestmem::write(f.addr+4200, &[0xa5]));
        p.compare().unwrap();
    }
    #[test] fn full_16k_chain_page_and_readonly_fallback() {
        let f = Fixture::with_page(16384);
        let p = f.plan().unwrap();
        p.apply(); p.compare().unwrap();
        // A mismatch beyond the first host 4KB must also be detected.
        assert!(crate::guestmem::write(f.addr+30000, &[0x11]));
        assert!(p.compare().is_err());
        let f = Fixture::new();
        assert_eq!(unsafe { libc::mprotect((f.addr+4096) as *mut _, 4096, 1) }, 0);
        assert!(matches!(f.plan(), Err("fixup pages not readable/writable")));
    }
    #[test] fn invalid_second_pointer_is_transactional() {
        let f = Fixture::new();
        let before = read(f.addr+4096, 4096).unwrap();
        assert!(crate::guestmem::write_u64(f.addr+4104, (1 << 63) | 99));
        let encoded = read(f.addr+4096, 4096).unwrap();
        assert!(f.plan().is_err());
        assert_eq!(read(f.addr+4096, 4096).unwrap(), encoded);
        assert_eq!(&encoded[..8], &before[..8]);
    }
    #[test] fn malformed_blobs_and_chains_fall_back() {
        for (at, value) in [(0, 8), (8, u32::MAX), (16, u32::MAX), (40, 4097), (48, 1)] {
            let mut f = Fixture::new(); put32(&mut f.blob, at, value); assert!(f.plan().is_err(), "at {at}");
        }
        for (at, value) in [(6, 12), (54, 6), (70, 0x8000), (70, 4092), (68, 2)] {
            let mut f = Fixture::new(); put16(&mut f.blob, at, value); assert!(f.plan().is_err(), "at {at}");
        }
        let f = Fixture::new();
        assert!(crate::guestmem::write_u64(f.addr+4096, 0x100000123 | (1 << 51)));
        assert!(f.plan().is_err()); // overlapping four-byte-stride pointer slots
        assert!(from_blob(0, 80).is_err());
        assert!(from_blob(0, u64::MAX).is_err());
    }
    #[test] fn sparse_and_four_aligned_runs_preserve_gaps() {
        let f = Fixture::new();
        // Rebase, gap of four bytes, bind, gap, rebase. Not u64-aligned.
        let mut f = f;
        put16(&mut f.blob, 70, 4);
        assert!(crate::guestmem::write_u64(f.addr+4100, 0x100000123 | (3 << 51)));
        assert!(crate::guestmem::write_u64(f.addr+4112, (1 << 63) | (3 << 51) | (7 << 24)));
        assert!(crate::guestmem::write_u64(f.addr+4124, 0x100000456));
        let p = f.plan().unwrap();
        assert_eq!((p.rebases, p.binds), (2, 1));
        p.apply(); p.compare().unwrap();
        assert_eq!(crate::guestmem::read_u64(f.addr+4100), Some(f.addr+0x123));
        assert_eq!(crate::guestmem::read_u64(f.addr+4112), Some(0x12340007));
        assert_eq!(crate::guestmem::read_u64(f.addr+4124), Some(f.addr+0x456));
    }
    #[test] fn dense_and_strided_batches_validate_every_encoding() {
        for (stride, start, format) in [(8usize, 0usize, 2u16), (16, 4, 6)] {
            let mut f = Fixture::new();
            put16(&mut f.blob, 6, format); put16(&mut f.blob, 54, format);
            put16(&mut f.blob, 70, start as u16);
            for i in 0..20 {
                let low = if format == 2 { 0x100000123 } else { 0x123 };
                let next = if i == 19 { 0 } else { (stride as u64/4) << 51 };
                assert!(crate::guestmem::write_u64(f.addr+4096+(start+i*stride) as u64,
                    low | (0xabu64 << 36) | next));
            }
            let p = f.plan().unwrap();
            assert_eq!((p.rebases, p.binds), (20, 0));
            p.apply(); p.compare().unwrap();
            for i in 0..20 {
                assert_eq!(crate::guestmem::read_u64(f.addr+4096+(start+i*stride) as u64),
                    Some((f.addr+0x123).wrapping_add(0xab00000000000000)));
            }
            // Restore encoding, then corrupt an interior word in a bulk batch.
            for i in 0..20 {
                let next = if i == 19 { 0 } else { (stride as u64/4) << 51 };
                assert!(crate::guestmem::write_u64(f.addr+4096+(start+i*stride) as u64, 0x123 | next));
            }
            assert!(crate::guestmem::write_u64(f.addr+4096+(start+5*stride) as u64, (1<<44) | (2<<51)));
            let encoded = read(f.addr+4096, 4096).unwrap();
            assert!(f.plan().is_err());
            assert_eq!(read(f.addr+4096, 4096).unwrap(), encoded);
        }
    }
    #[test] fn batch_boundaries_mixed_binds_and_changing_strides() {
        for count in 1..=33 {
            for start in [0usize, 4] {
                for stride in [8usize, 12, 16, 28] {
                    let mut f = Fixture::new();
                    put16(&mut f.blob, 70, start as u16);
                    let mut off = start;
                    for i in 0..count {
                        // Break both the type and stride assumptions inside
                        // batches; the last link must never be speculated past.
                        let step = if i % 9 == 8 { 8 } else { stride };
                        let next = if i+1 == count { 0 } else { (step as u64/4) << 51 };
                        let raw = if i % 7 == 6 { (1u64<<63) | (255<<24) }
                            else { 0x100000123 | (0xabu64<<36) };
                        assert!(crate::guestmem::write_u64(f.addr+4096+off as u64, raw|next));
                        off += step;
                    }
                    let p = f.plan().unwrap();
                    assert_eq!(p.rebases + p.binds, count);
                    let original = read(f.addr+4096, 4096).unwrap();
                    p.apply(); p.compare().unwrap();
                    p.restore(&[original.clone()]);
                    assert_eq!(read(f.addr+4096, 4096).unwrap(), original);
                }
            }
        }
    }
    #[test] fn parallel_pages_join_before_store_and_restore() {
        const N: usize = 128;
        let addr = unsafe { libc::mmap(std::ptr::null_mut(), N*PAGE, 3, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0) };
        assert_ne!(addr, libc::MAP_FAILED);
        let addr = addr as u64;
        let mut plan = Plan { pages: (0..N).map(|i| Page { addr: addr+(i*PAGE) as u64,
            len: PAGE, start: 4, delta: 0x100000000 }).collect(), targets: Vec::new(), expected: Vec::new(),
            rebases: 0, binds: 0, header: addr, vm_ns: 0 };
        for page in &plan.pages {
            for i in 0..16 {
                let raw = if i == 15 { 1u64<<63 } else { 0x123 | (2<<51) };
                assert!(crate::guestmem::write_u64(page.addr+4+i*8, raw));
            }
        }
        plan.writable().unwrap();
        let before = read(addr, N*PAGE).unwrap();
        // Last worker rejects an invalid ordinal; no other worker may store.
        assert!(crate::guestmem::write_u64(addr+((N-1)*PAGE+4+15*8) as u64, (1<<63)|99));
        assert!(matches!(plan.decode_pages(&[0xdeadbeef]), Err("out of range bind ordinal")));
        assert_eq!(read(addr, (N-1)*PAGE).unwrap(), before[..(N-1)*PAGE]);
        assert!(crate::guestmem::write_u64(addr+((N-1)*PAGE+4+15*8) as u64, 1<<63));
        plan.decode_pages(&[0xdeadbeef]).unwrap();
        assert_eq!((plan.rebases, plan.binds), (N*15, N));
        plan.snapshot_expected().unwrap();
        plan.apply(); plan.compare().unwrap();
        let originals: Vec<_> = before.chunks(PAGE).map(|b| b.to_vec()).collect();
        plan.restore(&originals);
        assert_eq!(read(addr, N*PAGE).unwrap(), before);
        unsafe { libc::munmap(addr as *mut _, N*PAGE); }
    }
    #[test] fn later_page_vm_failure_precedes_every_store() {
        let mut f = Fixture::new();
        // Extend data segment and starts to two pages, then make page 2
        // inaccessible. Even the valid page 1 must remain encoded.
        assert!(crate::guestmem::write_u64(f.addr+104+32, 8192));
        f.blob.resize(82, 0);
        put32(&mut f.blob, 8, 74); put32(&mut f.blob, 20, 34);
        put32(&mut f.blob, 48, 26); put16(&mut f.blob, 68, 2);
        put16(&mut f.blob, 72, 0); put64(&mut f.blob, 74, 0x12340000);
        let before = read(f.addr+4096, 4096).unwrap();
        assert_eq!(unsafe { libc::mprotect((f.addr+8192) as *mut _, 4096, 0) }, 0);
        assert!(matches!(f.plan(), Err("fixup pages not readable/writable")));
        assert_eq!(read(f.addr+4096, 4096).unwrap(), before);
    }
    #[test] fn wrong_uuid_and_forged_svc_are_not_hooks() {
        let f = Fixture::new();
        let b = read(f.addr, 176).unwrap();
        let m = crate::macho::MachO::parse_header(&b).unwrap();
        assert_eq!(audited(&m, 0), Err("unaudited dyld UUID"));
        let mut cpu = Cpu::new(); cpu.set_pc(0x1234);
        assert!(!handle_svc(&mut cpu, SVC));
        assert!(!handle_svc(&mut cpu, RETURN_SVC));
    }
}
