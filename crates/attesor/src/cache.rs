//! Emulation of the kernel's shared-region syscalls for the arm64e dyld
//! shared cache.
//!
//! dyld asks the kernel to map the cache (`shared_region_map_and_slide_2_np`)
//! and later asks where it is (`shared_region_check_np`). We perform the
//! mapping ourselves at a fixed address inside our process (so AOT
//! translations of cache code stay valid across runs) and apply the slide
//! info (rebasing; pointer-auth signing is the identity in Attesor).

use crate::hostsys;
use std::sync::Mutex;

pub(crate) mod slid;

/// Where we place the arm64 shared region (unslid start 0x180000000).
pub const CACHE_BASE: u64 = 0x6_0000_0000;

pub static CACHE: Mutex<Option<MappedCache>> = Mutex::new(None);

#[derive(Debug, Clone)]
pub struct MappedCache {
    pub base: u64,
    pub slide: u64,
    pub ranges: Vec<(u64, u64)>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct SharedFileNp {
    fd: i32,
    mappings_count: u32,
    slide: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct MappingSlideNp {
    address: u64,
    size: u64,
    file_offset: u64,
    slide_size: u64,
    slide_start: u64,
    max_prot: u32,
    init_prot: u32,
}

const VM_PROT_ZF: u32 = 0x10;

#[allow(dead_code)]
fn pread_all(fd: i32, buf: &mut [u8], off: u64) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let r = unsafe { libc::pread(fd, buf[done..].as_mut_ptr() as *mut _, buf.len() - done, (off + done as u64) as i64) };
        if r <= 0 {
            return false;
        }
        done += r as usize;
    }
    true
}

/// Handle `shared_region_map_and_slide_2_np(files_count, files, mappings_count, mappings)`.
/// Returns 0 or an errno.
pub fn map_and_slide_2(files_count: u32, files: u64, mappings_count: u32, mappings: u64) -> i64 {
    let files: &[SharedFileNp] = unsafe { std::slice::from_raw_parts(files as *const SharedFileNp, files_count as usize) };
    let maps: &[MappingSlideNp] = unsafe { std::slice::from_raw_parts(mappings as *const MappingSlideNp, mappings_count as usize) };
    if maps.is_empty() || files.iter().map(|f| f.mappings_count as u64).sum::<u64>() != maps.len() as u64 {
        return libc::EINVAL as i64;
    }
    let mut guard = CACHE.lock().unwrap();
    if guard.is_some() {
        return libc::EEXIST as i64;
    }
    let unslid_base = maps[0].address;
    let slide = CACHE_BASE.wrapping_sub(unslid_base);
    let stats = std::env::var_os("ATTESOR_DSC_STATS").is_some();
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let lookup_start = std::time::Instant::now();
    let identity = if std::env::var_os("ATTESOR_DSC_DISABLE").is_none() {
        slid::Identity::new(files, maps, slide, &crate::aot::cache_dir())
    } else { None };
    let cached = identity.as_ref().and_then(|id| id.load(maps).ok());
    let lookup_time = lookup_start.elapsed();
    let mmap_start = std::time::Instant::now();
    let hit = match map_with_cache(files, maps, slide, cached.as_ref()) {
        Ok(hit) => hit,
        Err(errno) => return errno,
    };
    let mmap_time = mmap_start.elapsed();
    let slide_start = std::time::Instant::now();
    let mut extents = Vec::new();
    let mut dirtied_pages = 0u64;
    // Second pass: apply slide info only on a miss. It lives in __LINKEDIT.
    if !hit {
        for (index, m) in maps.iter().enumerate() {
            if m.slide_size == 0 { continue; }
            let addr = m.address.wrapping_add(slide);
            let info = unsafe { std::slice::from_raw_parts(m.slide_start.wrapping_add(slide) as *const u8, m.slide_size as usize) };
            let mut dirty = DirtyPages::new(addr, m.size, page);
            if let Err(e) = unsafe { apply_slide_info(info, addr, m.size, slide, unslid_base, &mut dirty) } {
                eprintln!("attesor: slide info: {e}");
                return libc::EINVAL as i64;
            }
            let changed = dirty.extents(index as u64);
            dirtied_pages += changed.iter().map(|e| e.size / page).sum::<u64>();
            extents.extend(changed);
            let prot = (m.init_prot & 3) as i32;
            unsafe { crate::profile::host_call(|| libc::mprotect(addr as *mut _, m.size as usize, prot)) };
        }
    }
    let slide_time = slide_start.elapsed();
    let write_start = std::time::Instant::now();
    let mut file_size = cached.as_ref().filter(|_| hit).map_or(0, |c| c.size() as u64);
    let mut published = false;
    if !hit {
        if let Some(id) = &identity {
            // Gate only publication, NOT profile::host_call above: fork takes
            // profiling locks before FILE_LOCKS. No lock-order inversion, and
            // no child can inherit a live writer's flock descriptor.
            let _file_gate = crate::aot::FILE_LOCKS.read().unwrap();
            if let Ok(_writer) = id.writer() {
                // A different starter may have published while we rebased.
                // Recheck under the lock; only one process writes a valid key.
                if let Ok(existing) = id.load(maps) {
                    file_size = existing.size() as u64;
                } else {
                    match unsafe { id.publish(maps, slide, &extents) } {
                        Ok(size) => { file_size = size; published = true; }
                        Err(e) if stats => eprintln!("[attesor] dscslide: write skipped: {e}"),
                        Err(_) => {}
                    }
                }
            }
        }
    }
    let write_time = write_start.elapsed();
    if let Some(path) = std::env::var_os("ATTESOR_DSC_SAMPLE") {
        if let Err(e) = sample_slid_pages(maps, slide, std::path::Path::new(&path)) {
            eprintln!("[attesor] dscslide: sample: {e}");
        }
    }
    if stats {
        let stored_pages = if hit { cached.as_ref().unwrap().pages(page) } else { dirtied_pages };
        eprintln!("[attesor] dscslide: status={} bytes={} lookup_ms={:.3} mmap_ms={:.3} slide_ms={:.3} write_ms={:.3} dirtied_pages={} page_size={} stored_pages={} file_bytes={}",
            if hit { "hit" } else if published { "built" } else { "fallback" },
            maps.iter().filter(|m| m.slide_size != 0).map(|m| m.size).sum::<u64>(),
            lookup_time.as_secs_f64()*1000.0, mmap_time.as_secs_f64()*1000.0,
            slide_time.as_secs_f64()*1000.0, write_time.as_secs_f64()*1000.0,
            dirtied_pages, page, stored_pages, file_size);
    }
    let ranges: Vec<_> = maps.iter().map(|m| (m.address.wrapping_add(slide), m.size)).collect();
    if std::env::var_os("ATTESOR_TRACE").is_some() {
        eprintln!("[attesor] mapped arm64 shared cache at {:#x} (slide {:#x}), {} regions", CACHE_BASE, slide, ranges.len());
    }
    let end = ranges.iter().map(|(a, s)| a + s).max().unwrap_or(CACHE_BASE);
    *guard = Some(MappedCache { base: CACHE_BASE, slide, ranges });
    drop(guard);
    let mut uuid = [0u8; 16];
    unsafe { std::ptr::copy_nonoverlapping((CACHE_BASE + 88) as *const u8, uuid.as_mut_ptr(), 16) };
    crate::aot::register_cache(uuid, CACHE_BASE, end);
    0
}

/// Cache overlays may fail after some extents have already been installed.
/// Replace every original mapping before applying slides, never cached pointers.
fn map_with_cache(files: &[SharedFileNp], maps: &[MappingSlideNp], slide: u64, cached: Option<&slid::Cached>) -> Result<bool, i64> {
    map_regions(files, maps, slide, cached.is_some())?;
    if let Some(c) = cached {
        if unsafe { c.map(maps, slide) }.is_ok() { return Ok(true); }
        map_regions(files, maps, slide, false)?;
    }
    Ok(false)
}

/// The original mapping pass, also used to recover from a failed cache overlay.
fn map_regions(files: &[SharedFileNp], maps: &[MappingSlideNp], slide: u64, cached: bool) -> Result<(), i64> {
    let mut mi = 0;
    for f in files {
        for _ in 0..f.mappings_count {
            let m = maps[mi];
            mi += 1;
            let addr = m.address.wrapping_add(slide);
            let size = m.size as usize;
            let prot = (m.init_prot & 3) as i32; // never executable on the host
            unsafe {
                let p = if f.fd < 0 {
                    let p = crate::profile::host_call(|| libc::mmap(addr as *mut _, size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_FIXED, -1, 0));
                    if p != libc::MAP_FAILED { std::ptr::copy_nonoverlapping(m.file_offset as *const u8, p as *mut u8, size); }
                    p
                } else if m.max_prot & VM_PROT_ZF != 0 {
                    crate::profile::host_call(|| libc::mmap(addr as *mut _, size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE | libc::MAP_FIXED, -1, 0))
                } else {
                    let wprot = if m.slide_size != 0 && !cached { libc::PROT_READ | libc::PROT_WRITE } else { prot.max(libc::PROT_READ) };
                    crate::profile::host_call(|| libc::mmap(addr as *mut _, size, wprot, libc::MAP_PRIVATE | libc::MAP_FIXED, f.fd, m.file_offset as i64))
                };
                if p == libc::MAP_FAILED || p as u64 != addr {
                    eprintln!("attesor: failed to map shared cache region {addr:#x}+{size:#x}: {}", std::io::Error::last_os_error());
                    return Err(libc::ENOMEM as i64);
                }
                if m.slide_size == 0 || cached { crate::profile::host_call(|| libc::mprotect(addr as *mut _, size, prot)); }
            }
        }
    }
    Ok(())
}

/// Count the host pages receiving stores, including an unaligned pointer that
/// straddles two pages. Coalesce adjacent dirty pages to keep mmap count small.
struct DirtyPages { base: u64, page: u64, shift: u32, bits: Vec<bool> }
impl DirtyPages {
    fn new(base: u64, size: u64, page: u64) -> Self {
        Self { base, page, shift: page.trailing_zeros(), bits: vec![false; size.div_ceil(page) as usize] }
    }
    fn mark(&mut self, addr: u64) {
        let offset = addr - self.base;
        self.bits[(offset >> self.shift) as usize] = true;
        self.bits[((offset + 7) >> self.shift) as usize] = true;
    }
    fn extents(&self, mapping: u64) -> Vec<slid::Extent> {
        let mut result = Vec::new();
        let mut i = 0;
        while i < self.bits.len() {
            if !self.bits[i] { i += 1; continue; }
            let start = i;
            while i < self.bits.len() && self.bits[i] { i += 1; }
            result.push(slid::Extent { mapping, offset: start as u64 * self.page, size: (i - start) as u64 * self.page });
        }
        result
    }
}

/// Diagnostic byte-for-byte evidence, taken before dyld/guest mutates data.
/// Select first/middle/last slid dyld pages in every slid mapping.
fn sample_slid_pages(maps: &[MappingSlideNp], slide: u64, path: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::fs::File::create(path)?;
    for (index, m) in maps.iter().enumerate().filter(|(_, m)| m.slide_size != 0) {
        let info = unsafe { std::slice::from_raw_parts(m.slide_start.wrapping_add(slide) as *const u8, m.slide_size as usize) };
        let page = rd32(info, 4) as u64;
        let count = rd32(info, 8) as usize;
        let selected: Vec<_> = (0..count).filter(|&i| i as u64 * page + page <= m.size
            && u16::from_le_bytes([info[24 + i*2], info[25 + i*2]]) != 0xffff).collect();
        if selected.is_empty() { continue; }
        for i in [selected[0], selected[selected.len()/2], *selected.last().unwrap()] {
            for v in [index as u64, i as u64 * page, page] { out.write_all(&v.to_le_bytes())?; }
            let bytes = unsafe { std::slice::from_raw_parts((m.address.wrapping_add(slide) + i as u64 * page) as *const u8, page as usize) };
            out.write_all(bytes)?;
        }
    }
    Ok(())
}

pub fn check_np() -> Option<u64> {
    CACHE.lock().unwrap().as_ref().map(|c| c.base)
}

fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Apply dyld cache slide info (v3 or v5) to a mapped region.
unsafe fn apply_slide_info(info: &[u8], region: u64, size: u64, slide: u64, unslid_base: u64, dirty: &mut DirtyPages) -> Result<(), String> {
    let version = rd32(info, 0);
    match version {
        3 => {
            let page_size = rd32(info, 4) as u64;
            let count = rd32(info, 8) as usize;
            let auth_value_add = rd64(info, 16);
            for i in 0..count {
                let start = u16::from_le_bytes([info[24 + i * 2], info[25 + i * 2]]);
                if start == 0xFFFF {
                    continue;
                }
                let page = region + i as u64 * page_size;
                if page >= region + size {
                    break;
                }
                let mut loc = page + start as u64;
                loop {
                    let raw = std::ptr::read_unaligned(loc as *const u64);
                    let delta = (raw >> 51) & 0x7ff;
                    let auth = raw >> 63 != 0;
                    let v = if auth {
                        (raw & 0xffff_ffff).wrapping_add(auth_value_add).wrapping_add(slide)
                    } else {
                        let v51 = raw & ((1u64 << 51) - 1);
                        let top8 = v51 & 0x0007_F800_0000_0000;
                        let bottom43 = v51 & 0x0000_07FF_FFFF_FFFF;
                        ((top8 << 13) | bottom43).wrapping_add(slide)
                    };
                    dirty.mark(loc);
                    std::ptr::write_unaligned(loc as *mut u64, v);
                    if delta == 0 {
                        break;
                    }
                    loc += delta * 8;
                }
            }
            Ok(())
        }
        5 => {
            let page_size = rd32(info, 4) as u64;
            let count = rd32(info, 8) as usize;
            let value_add = rd64(info, 16);
            let _ = unslid_base;
            for i in 0..count {
                let start = u16::from_le_bytes([info[24 + i * 2], info[25 + i * 2]]);
                if start == 0xFFFF {
                    continue;
                }
                let page = region + i as u64 * page_size;
                if page >= region + size {
                    break;
                }
                let mut loc = page + start as u64;
                loop {
                    let raw = std::ptr::read_unaligned(loc as *const u64);
                    let next = (raw >> 52) & 0x7ff;
                    let auth = raw >> 63 != 0;
                    let target = (raw & ((1u64 << 34) - 1)).wrapping_add(value_add).wrapping_add(slide);
                    let v = if auth { target } else { target | (((raw >> 34) & 0xff) << 56) };
                    dirty.mark(loc);
                    std::ptr::write_unaligned(loc as *mut u64, v);
                    if next == 0 {
                        break;
                    }
                    loc += next * 8;
                }
            }
            Ok(())
        }
        v => Err(format!("unsupported slide info version {v}")),
    }
}

/// Keep `hostsys` referenced for callers that map via raw syscalls.
#[allow(dead_code)]
fn _unused() {
    let _ = hostsys::CLASS_UNIX;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dirty_pages_coalesce_and_include_straddling_stores() {
        let mut dirty = DirtyPages::new(0x10000, 0x4000, 0x1000);
        dirty.mark(0x10ffc);
        dirty.mark(0x13000);
        assert_eq!(dirty.extents(7), vec![slid::Extent { mapping: 7, offset: 0, size: 0x2000 },
            slid::Extent { mapping: 7, offset: 0x3000, size: 0x1000 }]);
    }
    #[test]
    fn slide_v3_v5_bytes_and_dirty_page_accounting() {
        for version in [3u32, 5] {
            let mut words = [0u64; 4];
            words[0] = (1 << 63) | 42;
            let mut info = vec![0; 26];
            info[..4].copy_from_slice(&version.to_le_bytes());
            info[4..8].copy_from_slice(&4096u32.to_le_bytes());
            info[8..12].copy_from_slice(&1u32.to_le_bytes());
            info[16..24].copy_from_slice(&0x180000000u64.to_le_bytes());
            let addr = words.as_mut_ptr() as u64;
            let mut dirty = DirtyPages::new(addr, 32, 4096);
            unsafe { apply_slide_info(&info, addr, 32, 0x480000000, 0x180000000, &mut dirty).unwrap(); }
            assert_eq!(words[0], CACHE_BASE + 42);
            assert_eq!(dirty.extents(0).len(), 1);
        }
    }
}
