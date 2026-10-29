//! Opt-in native chained fixups, using dyld's final (resolved/interposed) binds.
//! Hook only dyld's in-process fallback after map_with_linking_np returned
//! ENOTSUP. Symbol resolution, cache patching and segment protections stay guest.
//! The fallback's private mwl_info ABI is pinned by UUID, symbol and code hash.
//! Unknown dyld builds / formats execute the original guest implementation.
use attesor_core::cpu::Cpu;
use std::cell::RefCell;
use std::collections::BTreeMap;
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
const PAGE: usize = 4096; // host pages on the supported x86-64 macOS runtime
static HOOK: AtomicU64 = AtomicU64::new(0);
static RETURN: AtomicU64 = AtomicU64::new(0);
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

struct Page {
    fixed: Vec<u8>,
    dirty: Vec<bool>,
}
struct Plan {
    pages: BTreeMap<u64, Page>,
    rebases: usize,
    binds: usize,
    header: u64,
}
impl Plan {
    fn writable(&self) -> Result<()> {
        extern "C" {
            fn mach_port_deallocate(task: u32, name: u32) -> i32;
            fn mach_vm_region(task: u32, addr: *mut u64, size: *mut u64, flavor: i32,
                              info: *mut i32, count: *mut u32, object: *mut u32) -> i32;
        }
        let mut region_end = 0;
        for (&addr, page) in &self.pages {
            let end = addr.checked_add(page.fixed.len() as u64).ok_or("page end overflow")?;
            let mut at = addr;
            while at < end {
                if at < region_end { at = end.min(region_end); continue; }
                let (mut base, mut size, mut count, mut object) = (at, 0, 16, 0);
                let mut info = [0i32; 16];
                let task = unsafe { libc::mach_task_self() };
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
    fn write_page(addr: u64, page: &Page, bytes: &[u8]) {
        // Never overwrite untouched bytes: another thread may be using the
        // rest of a data page during dlopen. Coalesce adjacent pointer slots.
        let mut p = 0;
        while p < bytes.len() {
            if !page.dirty[p] { p += 1; continue; }
            let start = p;
            while p < bytes.len() && page.dirty[p] { p += 1; }
            // Full VM preflight completed before the first store. dyld owns
            // these not-yet-initialized image fixup slots under its loader lock.
            // Like the original guest loop, this assumes no concurrent unmap /
            // mprotect of that image. Never broaden page permissions.
            unsafe { std::ptr::copy_nonoverlapping(bytes[start..p].as_ptr(), (addr + start as u64) as *mut u8, p-start); }
        }
    }
    fn apply(&self) {
        for (&addr, page) in &self.pages { Self::write_page(addr, page, &page.fixed); }
    }
    fn compare(&self) -> Result<()> {
        for (&addr, page) in &self.pages {
            let actual = read(addr, page.fixed.len())?;
            if actual != page.fixed { return Err("fixed-up page differs from guest dyld"); }
        }
        Ok(())
    }
}

fn decode(raw: u64, format: u16, header: u64, preferred: u64, binds: &[u64]) -> Result<(u64, bool)> {
    if !matches!(format, 2 | 6) { return Err("unsupported pointer format"); }
    if raw >> 63 != 0 {
        if (raw >> 32) & 0x7ffff != 0 { return Err("reserved bind bits"); }
        let ordinal = (raw & 0xffffff) as usize;
        let target = *binds.get(ordinal).ok_or("out of range bind ordinal")?;
        Ok((target.wrapping_add((raw >> 24) & 255), true))
    } else {
        if (raw >> 44) & 127 != 0 { return Err("reserved rebase bits"); }
        let low = raw & 0xfffffffff;
        let base = if format == 2 { header.wrapping_sub(preferred) } else { header };
        Ok((low.wrapping_add(base).wrapping_add(((raw >> 36) & 255) << 56), false))
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

    let mut plan = Plan { pages: BTreeMap::new(), rebases: 0, binds: 0, header };
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
            let end = base.checked_add(page_size).ok_or("page end overflow")?;
            if base & (PAGE as u64 - 1) != 0
                || plan.pages.range(..=base).next_back().is_some_and(|(&at, p)| at + p.fixed.len() as u64 > base)
                || plan.pages.range(base..).next().is_some_and(|(&at, _)| at < end) {
                return Err("unaligned/overlapping chain pages");
            }
            let mut data = Page { fixed: read(base, page_size as usize)?, dirty: vec![false; page_size as usize] };
            let mut off = start as usize;
            loop {
                if off & 3 != 0 || off + 8 > page_size as usize { return Err("chain outside page"); }
                let raw = u64_at(&data.fixed, off)?;
                let (value, bind) = decode(raw, format, header, preferred, binds)?;
                if data.dirty[off..off+8].iter().any(|&v| v) { return Err("overlapping fixups"); }
                data.fixed[off..off+8].copy_from_slice(&value.to_le_bytes());
                data.dirty[off..off+8].fill(true);
                if bind { plan.binds += 1; } else { plan.rebases += 1; }
                if plan.binds + plan.rebases > 2_000_000 { return Err("too many fixups"); }
                let next = ((raw >> 51) & 0xfff) as usize * 4;
                if next == 0 { break; }
                off += next; // monotone, bounded by page; cannot cycle
            }
            plan.pages.insert(base, data);
        }
    }
    plan.writable()?;
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

struct Pending { plan: Plan, lr: u64, started: Instant, native_ns: u128 }
fn log(plan: &Plan, mode: &str, native_ns: u128, guest_ns: u128) {
    if std::env::var_os("ATTESOR_STATS").is_some() || mode == "verified" {
        eprintln!("[attesor] nativefixups: mode={mode} image={:#x} pages={} rebases={} binds={} native_ns={native_ns} guest_ns={guest_ns}",
                  plan.header, plan.pages.values().map(|p| p.fixed.len() / PAGE).sum::<usize>(), plan.rebases, plan.binds);
    }
}

/// SVCs are accepted only at our exact patched PC / private return trampoline.
/// Unsupported inputs execute the saved PACIBSP then continue original dyld.
pub fn handle_svc(cpu: &mut Cpu, imm: u16) -> bool {
    let entry = HOOK.load(Ordering::Relaxed);
    if entry == 0 { return false; }
    if imm == RETURN_SVC && cpu.pc() == RETURN.load(Ordering::Relaxed) + 4 {
        let pending = PENDING.with(|p| p.borrow_mut().pop()).expect("nativefixups: unmatched verification return");
        let guest_ns = pending.started.elapsed().as_nanos();
        pending.plan.compare().unwrap_or_else(|e| panic!("nativefixups: {e} image={:#x}", pending.plan.header));
        log(&pending.plan, "verified", pending.native_ns, guest_ns);
        cpu.x[30] = pending.lr;
        cpu.set_pc(pending.lr);
        return true;
    }
    if imm != SVC || cpu.pc() != entry + 4 { return false; }
    let started = Instant::now();
    let result = from_blob(cpu.x[0], cpu.x[1]);
    match result {
        Ok(plan) => {
            let mut native_ns = started.elapsed().as_nanos();
            if std::env::var_os("ATTESOR_NATIVEFIXUPS_VERIFY").is_some() {
                // Exercise the actual native writer, snapshot its full pages,
                // restore only the encoded slots, then let unmodified guest
                // dyld run on the identical input. No ASLR normalization/hashes.
                let originals: Vec<_> = plan.pages.iter().map(|(&addr, p)|
                    read(addr, p.fixed.len()).expect("nativefixups: verification snapshot")).collect();
                let apply_start = Instant::now();
                plan.apply();
                native_ns += apply_start.elapsed().as_nanos();
                plan.compare().expect("nativefixups: native writer differs from plan");
                for ((&addr, page), original) in plan.pages.iter().zip(&originals) {
                    Plan::write_page(addr, page, original);
                }
                PENDING.with(|p| p.borrow_mut().push(Pending { plan, lr: cpu.x[30], started: Instant::now(), native_ns }));
                cpu.x[30] = RETURN.load(Ordering::Relaxed);
                // PACIBSP is identity; resume after the patched instruction.
            } else {
                plan.apply();
                log(&plan, "native", started.elapsed().as_nanos(), 0);
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
        fn plan(&self) -> Result<Plan> { from_blob(self.blob.as_ptr() as u64, self.blob.len() as u64) }
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
