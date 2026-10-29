//! Diagnostics: map guest addresses to "image+offset" and walk frame chains.

use crate::cache;
use attesor_core::cpu::Cpu;
use std::sync::Mutex;

#[derive(Clone)]
struct Img {
    start: u64,
    end: u64,
    name: String,
    header: u64,
    symbols: Option<Vec<(u64, String)>>,
}

static IMAGES: Mutex<Vec<Img>> = Mutex::new(Vec::new());
static CACHE_IMAGES_LOADED: Mutex<bool> = Mutex::new(false);

pub fn add_image(start: u64, end: u64, name: &str) {
    IMAGES.lock().unwrap().push(Img { start, end, name: name.to_string(), header: start, symbols: None });
}

fn load_cache_images() {
    let mut done = CACHE_IMAGES_LOADED.lock().unwrap();
    if *done {
        return;
    }
    let Some(base) = cache::check_np() else { return };
    *done = true;
    unsafe {
        let hdr = base as *const u8;
        let rd32 = |o: usize| std::ptr::read_unaligned(hdr.add(o) as *const u32);
        let rd64 = |p: *const u8| std::ptr::read_unaligned(p as *const u64);
        let slide = cache::CACHE.lock().unwrap().as_ref().map(|c| c.slide).unwrap_or(0);
        let images_off = rd32(0x1c0) as usize;
        let images_cnt = rd32(0x1c4) as usize;
        if images_off == 0 || images_cnt == 0 || images_cnt > 10000 {
            return;
        }
        let mut v = IMAGES.lock().unwrap();
        for i in 0..images_cnt {
            let e = hdr.add(images_off + i * 32);
            let addr = rd64(e).wrapping_add(slide);
            let path_off = std::ptr::read_unaligned(e.add(24) as *const u32) as usize;
            let path = std::ffi::CStr::from_ptr(hdr.add(path_off) as *const libc::c_char).to_string_lossy().into_owned();
            // __TEXT size from the mach header's first segment
            let mh = addr as *const u8;
            let ncmds = std::ptr::read_unaligned(mh.add(16) as *const u32);
            let mut p = mh.add(32);
            let mut size = 0x1000u64;
            for _ in 0..ncmds {
                let cmd = std::ptr::read_unaligned(p as *const u32);
                let cmdsize = std::ptr::read_unaligned(p.add(4) as *const u32);
                if cmd == 0x19 {
                    let name = std::ffi::CStr::from_ptr(p.add(8) as *const libc::c_char);
                    if name.to_bytes() == b"__TEXT" {
                        size = std::ptr::read_unaligned(p.add(32) as *const u64);
                        break;
                    }
                }
                p = p.add(cmdsize as usize);
            }
            let short = path.rsplit('/').next().unwrap_or(&path).to_string();
            v.push(Img { start: addr, end: addr + size, name: short, header: addr, symbols: None });
        }
    }
}

pub fn describe(addr: u64) -> String {
    load_cache_images();
    let v = IMAGES.lock().unwrap();
    for i in v.iter() {
        if addr >= i.start && addr < i.end {
            return format!("{}+{:#x}", i.name, addr - i.start);
        }
    }
    "?".to_string()
}

pub fn backtrace(cpu: &Cpu) {
    let mut fp = cpu.x[29];
    for i in 0..32 {
        if fp == 0 || fp & 7 != 0 {
            break;
        }
        let (next, lr) = unsafe {
            let mut v = [0u64; 2];
            // Avoid faulting on a bogus chain.
            let mut len: libc::size_t = 16;
            let kr = mach_vm_read_overwrite(libc::mach_task_self(), fp, 16, v.as_mut_ptr() as u64, &mut len);
            if kr != 0 {
                break;
            }
            (v[0], v[1])
        };
        eprintln!("  #{i:<2} {:#x} {}", lr, describe(lr));
        fp = next;
    }
}

extern "C" {
    fn mach_vm_read_overwrite(task: libc::mach_port_t, addr: u64, size: u64, data: u64, outsize: *mut libc::size_t) -> libc::c_int;
}

// Profiling symbol lookup is deliberately separate from describe(): debug dumps
// keep their inexpensive image+offset format. Symbols are copied out of guest
// mappings before parsing, so unloading a dylib cannot fault the sampling thread.
fn u32_at(b: &[u8], p: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(p..p.checked_add(4)?)?.try_into().ok()?))
}
fn u64_at(b: &[u8], p: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(p..p.checked_add(8)?)?.try_into().ok()?))
}
fn string_at(b: &[u8], p: usize) -> Option<String> {
    let b = b.get(p..)?;
    let n = b.iter().position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&b[..n]).into_owned())
}
fn commands(b: &[u8]) -> impl Iterator<Item = (u32, &[u8])> {
    let mut p = 32usize;
    let mut left = u32_at(b, 16).unwrap_or(0).min(4096);
    std::iter::from_fn(move || {
        if left == 0 { return None; }
        left -= 1;
        let cmd = u32_at(b, p)?;
        let size = u32_at(b, p + 4)? as usize;
        if size < 8 { return None; }
        let end = p.checked_add(size)?;
        let bytes = b.get(p..end)?;
        p = end;
        Some((cmd, bytes))
    })
}

pub fn add_file_symbols(path: &str, m: &crate::macho::MachO, slide: u64) {
    if std::env::var_os("ATTESOR_PROFILE").is_none() { return; }
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else { return };
    let mut syms = Vec::new();
    for (cmd, b) in commands(&m.header_bytes) {
        if cmd != crate::macho::LC_SYMTAB { continue; }
        let (Some(off), Some(count), Some(stroff), Some(strsize)) = (u32_at(b, 8), u32_at(b, 12), u32_at(b, 16), u32_at(b, 20)) else { continue };
        if count > 4_000_000 || strsize > 128 << 20 { continue; }
        let mut table = vec![0; count as usize * 16];
        let mut strings = vec![0; strsize as usize];
        if file.seek(SeekFrom::Start(m.slice_offset + off as u64)).is_err() || file.read_exact(&mut table).is_err()
            || file.seek(SeekFrom::Start(m.slice_offset + stroff as u64)).is_err() || file.read_exact(&mut strings).is_err() { continue; }
        for n in table.chunks_exact(16) {
            // N_STAB is debug metadata; N_SECT defines an address in this image.
            if n[4] & 0xe0 != 0 || n[4] & 0x0e != 0x0e { continue; }
            let Some(name) = string_at(&strings, u32_at(n, 0).unwrap() as usize) else { continue };
            if !name.is_empty() { syms.push((u64_at(n, 8).unwrap().wrapping_add(slide), name)); }
        }
    }
    syms.sort();
    let start = m.vm_range().0.wrapping_add(slide);
    if let Some(img) = IMAGES.lock().unwrap().iter_mut().find(|i| i.start == start) {
        img.header = m.text_vmaddr().wrapping_add(slide);
        if !syms.is_empty() { img.symbols = Some(syms); }
    }
}

fn uleb(b: &[u8], p: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let c = *b.get(*p)?;
        *p += 1;
        if shift == 63 && c > 1 { return None; }
        v |= ((c & 127) as u64) << shift;
        if c & 128 == 0 { return Some(v); }
    }
    None
}
fn exports(b: &[u8], base: u64) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut todo = vec![(0usize, String::new())];
    let mut seen = std::collections::HashSet::new();
    while let Some((mut p, prefix)) = todo.pop() {
        if !seen.insert(p) || seen.len() > 1_000_000 || prefix.len() > 4096 { continue; }
        let Some(size) = uleb(b, &mut p) else { continue };
        let Some(end) = p.checked_add(size as usize).filter(|&end| end <= b.len()) else { continue };
        if size != 0 {
            let mut t = p;
            if let Some(flags) = uleb(&b[..end], &mut t) {
                // Ignore re-exports and absolute/thread-local definitions.
                if flags & 0x0b == 0 {
                    if let Some(addr) = uleb(&b[..end], &mut t) { out.push((base.wrapping_add(addr), prefix.clone())); }
                }
            }
        }
        p = end;
        let Some(&count) = b.get(p) else { continue };
        p += 1;
        for _ in 0..count {
            let Some(edge) = string_at(b, p) else { break };
            p += edge.len() + 1;
            let Some(child) = uleb(b, &mut p) else { break };
            todo.push((child as usize, format!("{prefix}{edge}")));
        }
    }
    out.sort();
    out
}
fn mapped_exports(header: u64) -> Vec<(u64, String)> {
    let mut h = [0; 32];
    if !crate::guestmem::read(header, &mut h) || u32_at(&h, 0) != Some(0xfeedfacf) { return vec![]; }
    let size = u32_at(&h, 20).unwrap() as usize;
    if size > 1 << 20 { return vec![]; }
    let mut h = vec![0; 32 + size];
    if !crate::guestmem::read(header, &mut h) { return vec![]; }
    let (mut text, mut link, mut off, mut starts) = (None, None, None, None);
    for (cmd, b) in commands(&h) {
        if cmd == 0x19 {
            let name = string_at(b, 8).unwrap_or_default();
            if name == "__TEXT" { text = u64_at(b, 24); }
            if name == "__LINKEDIT" { link = u64_at(b, 24).zip(u64_at(b, 40)); }
        } else if cmd == 0x26 {
            starts = u32_at(b, 8).zip(u32_at(b, 12));
        } else if cmd == 0x80000033 {
            off = u32_at(b, 8).zip(u32_at(b, 12));
        } else if (cmd == 0x22 || cmd == 0x80000022) && off.is_none() {
            off = u32_at(b, 40).zip(u32_at(b, 44));
        }
    }
    let (Some(text), Some((vm, file)), Some((off, size))) = (text, link, off) else { return vec![] };
    if size == 0 || size > 64 << 20 { return vec![]; }
    let slide = header.wrapping_sub(text);
    let addr = vm.wrapping_add(slide).wrapping_sub(file).wrapping_add(off as u64);
    let mut bytes = vec![0; size as usize];
    if !crate::guestmem::read(addr, &mut bytes) { return vec![]; }
    let mut syms = exports(&bytes, header);
    // Bound nearest-export attribution at every real function start, rather
    // than assigning stripped internal functions to an unrelated public API.
    if let Some((off, size)) = starts.filter(|&(_, size)| size <= 16 << 20) {
        let addr = vm.wrapping_add(slide).wrapping_sub(file).wrapping_add(off as u64);
        let mut bytes = vec![0; size as usize];
        if crate::guestmem::read(addr, &mut bytes) {
            let named: std::collections::HashSet<_> = syms.iter().map(|s| s.0).collect();
            for addr in crate::macho::decode_function_starts(&bytes, header) {
                if !named.contains(&addr) { syms.push((addr, format!("<internal+{:#x}>", addr.wrapping_sub(header)))); }
            }
            syms.sort();
        }
    }
    syms
}

/// An unmapped-image PC is runtime-generated/anonymous code, not necessarily V8.
pub fn is_image_pc(pc: u64) -> bool {
    load_cache_images();
    IMAGES.lock().unwrap().iter().any(|i| pc >= i.start && pc < i.end)
}

pub fn attribute(pc: u64) -> (String, String) {
    load_cache_images();
    let mut images = IMAGES.lock().unwrap();
    if let Some(img) = images.iter_mut().find(|i| pc >= i.start && pc < i.end) {
        let syms = img.symbols.get_or_insert_with(|| mapped_exports(img.header));
        let n = syms.partition_point(|(addr, _)| *addr <= pc);
        let symbol = if n != 0 && syms[n - 1].0 >= img.start {
            syms[n - 1].1.clone()
        } else { format!("+{:#x}", pc - img.start) };
        return (img.name.clone(), symbol);
    }
    drop(images);
    ("?".into(), describe(pc))
}

#[cfg(test)]
mod profile_tests {
    use super::*;
    #[test]
    fn export_trie_bounds_and_names() {
        // root -> _foo -> regular export at offset 0x20
        let data = [0, 1, b'_', b'f', b'o', b'o', 0, 8, 2, 0, 0x20, 0];
        assert_eq!(exports(&data, 0x1000), vec![(0x1020, "_foo".into())]);
        for n in 0..data.len() { let _ = exports(&data[..n], 0); }
        assert!(uleb(&[255; 20], &mut 0).is_none());
        assert!(exports(&[0, 1, b'x', 0, 0], 0).is_empty()); // cyclic trie
    }
}

/// mmap's executable first segment identifies non-cache images, even with AOT off.
pub fn register_mapped_file(addr: u64, fd: i32) {
    if !crate::profile::ENABLED.load(std::sync::atomic::Ordering::Relaxed) { return; }
    let mut path = [0u8; 1024];
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, path.as_mut_ptr()) } != 0 { return; }
    let Some(path) = string_at(&path, 0) else { return };
    let Ok(m) = crate::macho::MachO::open_arm64(&path, true) else { return };
    let slide = addr.wrapping_sub(m.text_vmaddr());
    let (lo, hi) = m.vm_range();
    let start = lo.wrapping_add(slide);
    {
        let mut images = IMAGES.lock().unwrap();
        images.retain(|img| img.start != start);
        images.push(Img { start, end: hi.wrapping_add(slide),
            name: path.rsplit('/').next().unwrap_or(&path).to_string(), header: addr, symbols: None });
    }
    add_file_symbols(&path, &m, slide);
}
