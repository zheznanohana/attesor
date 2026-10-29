//! Stateless native-library bridge. Guest and host share an address space and
//! Darwin arm64/x86-64 use LP64 zlib/vImage APIs. No guest callbacks or library
//! state crosses the boundary. Callers retain the libraries' valid-buffer contract.
use attesor_core::cpu::Cpu;
use std::sync::atomic::{AtomicU64, Ordering};

pub const SYS_NATIVE_CALL: u64 = 0x4154;
const SHIM: &str = "libattesornative.dylib";
const VIMAGE_SHIM: &str = "libattesorvimage.dylib";
static CALLS: AtomicU64 = AtomicU64::new(0);
static VIMAGE_CALLS: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
struct ImageBuffer {
    data: *mut u8,
    height: u64,
    width: u64,
    row_bytes: u64,
}

#[link(name = "Accelerate", kind = "framework")]
extern "C" {
    fn vImageConvolve_Planar8(src: *const ImageBuffer, dst: *const ImageBuffer, scratch: *mut libc::c_void, x: u64, y: u64,
        kernel: *const i16, kh: u32, kw: u32, divisor: i32, background: u8, flags: u32) -> i64;
}

#[link(name = "z")]
extern "C" {
    fn crc32(crc: libc::c_ulong, buf: *const u8, len: libc::c_uint) -> libc::c_ulong;
    fn adler32(adler: libc::c_ulong, buf: *const u8, len: libc::c_uint) -> libc::c_ulong;
    fn compress2(dest: *mut u8, destlen: *mut libc::c_ulong, source: *const u8, sourcelen: libc::c_ulong, level: libc::c_int) -> libc::c_int;
    fn uncompress(dest: *mut u8, destlen: *mut libc::c_ulong, source: *const u8, sourcelen: libc::c_ulong) -> libc::c_int;
}

fn ptr(p: u64) -> u64 { p & 0x00ff_ffff_ffff_ffff }

unsafe fn image_buffer(p: u64) -> ImageBuffer {
    let mut b = std::ptr::read_unaligned(ptr(p) as *const ImageBuffer);
    b.data = ptr(b.data as u64) as *mut u8;
    b
}

unsafe fn vimage_call(a: &[u64; 6]) -> i64 {
    let v = std::ptr::read_unaligned(ptr(a[1]) as *const [u64; 10]);
    let src = image_buffer(v[0]);
    let dst = image_buffer(v[1]);
    vImageConvolve_Planar8(&src, &dst, std::ptr::null_mut(), v[2], v[3], ptr(v[4]) as *const i16,
        v[5] as u32, v[6] as u32, v[7] as i32, v[8] as u8, v[9] as u32)
}

pub fn hostcall(cpu: &mut Cpu) {
    let a = [cpu.x[0], cpu.x[1], cpu.x[2], cpu.x[3], cpu.x[4], cpu.x[5]];
    if a[0] == 0 || a[0] == 18 {
        cpu.x[0] = if a[0] == 0 { &CALLS } else { &VIMAGE_CALLS }.load(Ordering::Relaxed);
        cpu.cf = 0;
        return;
    }
    let r = unsafe {
        match a[0] {
            1 => crc32(a[1], ptr(a[2]) as *const u8, a[3] as u32),
            2 => adler32(a[1], ptr(a[2]) as *const u8, a[3] as u32),
            3 => compress2(ptr(a[1]) as *mut u8, ptr(a[2]) as *mut u64, ptr(a[3]) as *const u8, a[4], a[5] as i32) as i64 as u64,
            4 => uncompress(ptr(a[1]) as *mut u8, ptr(a[2]) as *mut u64, ptr(a[3]) as *const u8, a[4]) as i64 as u64,
            17 => vimage_call(&a) as u64,
            _ => { cpu.x[0] = libc::ENOSYS as u64; cpu.cf = 1; return; }
        }
    };
    if a[0] == 17 { &VIMAGE_CALLS } else { &CALLS }.fetch_add(1, Ordering::Relaxed);
    cpu.x[0] = r;
    cpu.cf = 0;
}

fn shim_path(name: &str) -> Option<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("ATTESOR_NATIVE_DIR") {
        let path = std::path::PathBuf::from(d).join(name);
        return path.is_file().then_some(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() { dirs.push(d.join("native")); dirs.push(d.to_path_buf()); }
    }
    if let Some(h) = std::env::var_os("HOME") { dirs.push(std::path::Path::new(&h).join(".attesor/native")); }
    dirs.into_iter().map(|d: std::path::PathBuf| d.join(name)).find(|p| p.is_file())
}

pub fn is_shim(path: &str) -> bool {
    std::path::Path::new(path).file_name().is_some_and(|n| n == SHIM || n == VIMAGE_SHIM)
}

fn libraries(value: &str) -> Result<Vec<&'static str>, &'static str> {
    if value == "0" || value.is_empty() { return Ok(Vec::new()); }
    if value == "1" { return Ok(vec![SHIM]); } // Preserve the legacy zlib opt-in.
    let mut names = Vec::new();
    for item in value.split(',') {
        let name = match item.trim() {
            "zlib" => SHIM,
            "vimage" => VIMAGE_SHIM,
            _ => return Err("ATTESOR_NATIVE_LIBS: expected 0, 1 (zlib), or comma-separated zlib,vimage"),
        };
        if !names.contains(&name) { names.push(name); }
    }
    Ok(names)
}

pub fn inject(envp: &mut Vec<String>) -> Result<(), &'static str> {
    // Remove inherited shim first: setting the switch back to 0 must disable it,
    // including in a child launched by an already-accelerated guest.
    envp.retain_mut(|e| {
        let Some(v) = e.strip_prefix("DYLD_INSERT_LIBRARIES=") else { return true };
        let kept: Vec<_> = v.split(':').filter(|p| !p.is_empty() && !is_shim(p)).collect();
        if kept.is_empty() { return false; }
        *e = format!("DYLD_INSERT_LIBRARIES={}", kept.join(":")); true
    });
    for name in libraries(&std::env::var("ATTESOR_NATIVE_LIBS").unwrap_or_default())? {
        let shim = shim_path(name).ok_or(if name == SHIM {
            "ATTESOR_NATIVE_LIBS: libattesornative.dylib missing; run native/build.sh and set ATTESOR_NATIVE_DIR"
        } else {
            "ATTESOR_NATIVE_LIBS: libattesorvimage.dylib missing; run native/build.sh and set ATTESOR_NATIVE_DIR"
        })?;
        let shim = shim.to_string_lossy();
        if let Some(e) = envp.iter_mut().find(|e| e.starts_with("DYLD_INSERT_LIBRARIES=")) {
            e.push(':'); e.push_str(&shim);
        } else { envp.push(format!("DYLD_INSERT_LIBRARIES={shim}")); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn library_selection() {
        assert!(libraries("").unwrap().is_empty());
        assert!(libraries("0").unwrap().is_empty());
        assert_eq!(libraries("1").unwrap(), [SHIM]);
        assert_eq!(libraries("vimage").unwrap(), [VIMAGE_SHIM]);
        assert_eq!(libraries(" zlib ,vimage,zlib").unwrap(), [SHIM, VIMAGE_SHIM]);
        for invalid in ["all", "2", "vimage,", "zlib,bogus"] { assert!(libraries(invalid).is_err()); }
    }
    #[test]
    fn shim_names_and_layout() {
        assert!(is_shim("/somewhere/libattesorvimage.dylib"));
        assert!(is_shim("/somewhere/libattesornative.dylib"));
        assert!(!is_shim("/somewhere/libz.dylib"));
        assert_eq!(std::mem::size_of::<ImageBuffer>(), 32);
        assert_eq!(std::mem::offset_of!(ImageBuffer, row_bytes), 24);
    }
}
