//! Native-library bridges with host-only FFT companion state. Guest and host share an address space and
//! Darwin arm64/x86-64 use LP64 zlib/vImage APIs. No guest callbacks or library
//! state crosses the boundary. Callers retain the libraries' valid-buffer contract.
use attesor_core::cpu::Cpu;
use std::sync::atomic::{AtomicU64, Ordering};

pub const SYS_NATIVE_CALL: u64 = 0x4154;
const SHIM: &str = "libattesornative.dylib";
const VIMAGE_SHIM: &str = "libattesorvimage.dylib";
static CALLS: AtomicU64 = AtomicU64::new(0);
const VDSP_SHIM: &str = "libattesorvdsp.dylib";
const COMPRESSION_SHIM: &str = "libattesorcompression.dylib";
const CRYPTO_SHIM: &str = "libattesorcrypto.dylib";
const IMAGEIO_SHIM: &str = "libattesorimageio.dylib";
static IMAGEIO_CALLS: AtomicU64 = AtomicU64::new(0);
static VDSP_CALLS: AtomicU64 = AtomicU64::new(0);
static COMPRESSION_CALLS: AtomicU64 = AtomicU64::new(0);
static CRYPTO_CALLS: AtomicU64 = AtomicU64::new(0);
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
    fn vImageScale_PlanarF(src: *const ImageBuffer, dst: *const ImageBuffer, scratch: *mut libc::c_void, flags: u32) -> i64;
    fn vImageScale_ARGBFFFF(src: *const ImageBuffer, dst: *const ImageBuffer, scratch: *mut libc::c_void, flags: u32) -> i64;
    fn vImageConvert_Planar8toPlanarF(src: *const ImageBuffer, dst: *const ImageBuffer, max: f32, min: f32, flags: u32) -> i64;
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

#[link(name = "compression")]
extern "C" {
    fn compression_encode_buffer(dst: *mut u8, capacity: usize, src: *const u8, size: usize, scratch: *mut libc::c_void, algorithm: u32) -> usize;
    fn compression_decode_buffer(dst: *mut u8, capacity: usize, src: *const u8, size: usize, scratch: *mut libc::c_void, algorithm: u32) -> usize;
}
extern "C" {
    fn attesor_buffer_imageio(op: u32, slots: *const u64) -> i32;
    fn CCCrypt(op: u32, algorithm: u32, options: u32, key: *const u8, key_len: usize, iv: *const u8,
        src: *const u8, size: usize, dst: *mut u8, capacity: usize, written: *mut usize) -> i32;
}

pub(crate) fn ptr(p: u64) -> u64 { p & 0x00ff_ffff_ffff_ffff }

// The guest shims only accelerate default FPCR. Do not let an unrelated host
// MXCSR rounding/FTZ/DAZ setting leak into that contract, or leak status out.
pub(crate) struct FloatEnv(u32);
impl FloatEnv {
    pub(crate) unsafe fn new() -> Self {
        let mut old = 0u32;
        std::arch::asm!("stmxcsr [{}]", in(reg) &mut old, options(nostack, preserves_flags));
        let ieee = 0x1f80u32;
        std::arch::asm!("ldmxcsr [{}]", in(reg) &ieee, options(nostack, preserves_flags));
        Self(old)
    }
}
impl Drop for FloatEnv {
    fn drop(&mut self) { unsafe {
        std::arch::asm!("ldmxcsr [{}]", in(reg) &self.0, options(nostack, preserves_flags));
    } }
}

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
    let counter = match a[0] {
        0..=4 => &CALLS,
        17..=21 => &VIMAGE_CALLS,
        32..=42 => &VDSP_CALLS,
        48..=50 => &COMPRESSION_CALLS,
        56..=57 => &CRYPTO_CALLS,
        64..=66 => &IMAGEIO_CALLS,
        _ => { cpu.x[0] = libc::ENOSYS as u64; cpu.cf = 1; return; }
    };
    if matches!(a[0], 0 | 18 | 32 | 48 | 56 | 64) {
        cpu.x[0] = counter.load(Ordering::Relaxed);
        cpu.cf = 0;
        return;
    }
    if (33..=42).contains(&a[0]) {
        let (r, work) = unsafe { crate::native_dsp::call(&a) };
        if work { counter.fetch_add(1, Ordering::Relaxed); }
        cpu.x[0] = r; cpu.cf = 0; return;
    }
    let r = unsafe {
        match a[0] {
            1 => crc32(a[1], ptr(a[2]) as *const u8, a[3] as u32),
            2 => adler32(a[1], ptr(a[2]) as *const u8, a[3] as u32),
            3 => compress2(ptr(a[1]) as *mut u8, ptr(a[2]) as *mut u64, ptr(a[3]) as *const u8, a[4], a[5] as i32) as i64 as u64,
            4 => uncompress(ptr(a[1]) as *mut u8, ptr(a[2]) as *mut u64, ptr(a[3]) as *const u8, a[4]) as i64 as u64,
            17 => vimage_call(&a) as u64,
            19..=21 => {
                let _env = FloatEnv::new();
                let src = image_buffer(a[1]);
                let dst = image_buffer(a[2]);
                (match a[0] {
                    19 => vImageScale_PlanarF(&src, &dst, std::ptr::null_mut(), a[3] as u32),
                    20 => vImageScale_ARGBFFFF(&src, &dst, std::ptr::null_mut(), a[3] as u32),
                    _ => vImageConvert_Planar8toPlanarF(&src, &dst, f32::from_bits(a[3] as u32), f32::from_bits(a[4] as u32), a[5] as u32),
                }) as u64
            }
            49 | 50 => {
                let f = if a[0] == 49 { compression_encode_buffer } else { compression_decode_buffer };
                f(ptr(a[1]) as *mut u8, a[2] as usize, ptr(a[3]) as *const u8, a[4] as usize, std::ptr::null_mut(), a[5] as u32) as u64
            }
            65 | 66 => {
                let _env = FloatEnv::new();
                let mut v = std::ptr::read_unaligned(ptr(a[1]) as *const [u64; 7]);
                for i in if a[0] == 65 { &[0, 2, 4][..] } else { &[0, 1, 4, 6][..] } { v[*i] = ptr(v[*i]); }
                attesor_buffer_imageio(a[0] as u32, v.as_ptr()) as i64 as u64
            }
            57 => {
                let v = std::ptr::read_unaligned(ptr(a[1]) as *const [u64; 11]);
                CCCrypt(v[0] as u32, v[1] as u32, v[2] as u32, ptr(v[3]) as *const u8, v[4] as usize, ptr(v[5]) as *const u8,
                    ptr(v[6]) as *const u8, v[7] as usize, ptr(v[8]) as *mut u8, v[9] as usize, ptr(v[10]) as *mut usize) as i64 as u64
            }
            _ => { cpu.x[0] = libc::ENOSYS as u64; cpu.cf = 1; return; }
        }
    };
    counter.fetch_add(1, Ordering::Relaxed);
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
    std::path::Path::new(path).file_name().is_some_and(|n| [SHIM, VIMAGE_SHIM, VDSP_SHIM, COMPRESSION_SHIM, CRYPTO_SHIM, IMAGEIO_SHIM].iter().any(|s| n == *s))
}

fn libraries(value: &str) -> Result<Vec<&'static str>, &'static str> {
    if value == "0" || value.is_empty() { return Ok(Vec::new()); }
    if value == "1" { return Ok(vec![SHIM]); } // Preserve the legacy zlib opt-in.
    let mut names = Vec::new();
    for item in value.split(',') {
        let name = match item.trim() {
            "zlib" => SHIM,
            "vimage" => VIMAGE_SHIM,
            "vdsp" => VDSP_SHIM,
            "compression" => COMPRESSION_SHIM,
            "crypto" => CRYPTO_SHIM,
            "imageio" => IMAGEIO_SHIM,
            _ => return Err("ATTESOR_NATIVE_LIBS: expected 0, 1 (zlib), or comma-separated zlib,vimage,vdsp,compression,crypto,imageio"),
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
        let shim = shim_path(name).ok_or(match name {
            SHIM => "ATTESOR_NATIVE_LIBS: libattesornative.dylib missing; run native/build.sh and set ATTESOR_NATIVE_DIR",
            VIMAGE_SHIM => "ATTESOR_NATIVE_LIBS: libattesorvimage.dylib missing; run native/build.sh and set ATTESOR_NATIVE_DIR",
            VDSP_SHIM => "ATTESOR_NATIVE_LIBS: libattesorvdsp.dylib missing; run native/build.sh and set ATTESOR_NATIVE_DIR",
            COMPRESSION_SHIM => "ATTESOR_NATIVE_LIBS: libattesorcompression.dylib missing; run native/build.sh and set ATTESOR_NATIVE_DIR",
            CRYPTO_SHIM => "ATTESOR_NATIVE_LIBS: libattesorcrypto.dylib missing; run native/build.sh and set ATTESOR_NATIVE_DIR",
            _ => "ATTESOR_NATIVE_LIBS: libattesorimageio.dylib missing; run native/build.sh and set ATTESOR_NATIVE_DIR",
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
        assert_eq!(libraries("vdsp,compression,crypto,imageio").unwrap(), [VDSP_SHIM, COMPRESSION_SHIM, CRYPTO_SHIM, IMAGEIO_SHIM]);
        for name in [VDSP_SHIM, COMPRESSION_SHIM, CRYPTO_SHIM, IMAGEIO_SHIM] { assert!(is_shim(name)); }
        for invalid in ["all", "2", "vimage,", "zlib,bogus"] { assert!(libraries(invalid).is_err()); }
    }
    #[test]
    fn host_float_controls_are_scoped() {
        unsafe {
            let mut initial = 0u32;
            std::arch::asm!("stmxcsr [{}]", in(reg) &mut initial, options(nostack, preserves_flags));
            let altered = initial | (1 << 15) | (1 << 6) | (1 << 13);
            std::arch::asm!("ldmxcsr [{}]", in(reg) &altered, options(nostack, preserves_flags));
            {
                let _guard = FloatEnv::new();
                let mut current = 0u32;
                std::arch::asm!("stmxcsr [{}]", in(reg) &mut current, options(nostack, preserves_flags));
                assert_eq!(current, 0x1f80);
            }
            let mut restored = 0u32;
            std::arch::asm!("stmxcsr [{}]", in(reg) &mut restored, options(nostack, preserves_flags));
            std::arch::asm!("ldmxcsr [{}]", in(reg) &initial, options(nostack, preserves_flags));
            assert_eq!(restored, altered);
        }
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
