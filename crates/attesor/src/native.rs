//! Stateless native-library bridge. Guest and host share an address space and
//! Darwin arm64/x86-64 both use LP64 zlib APIs. No guest callbacks or z_stream
//! state crosses the boundary. Callers retain zlib's normal valid-buffer contract.
use attesor_core::cpu::Cpu;
use std::sync::atomic::{AtomicU64, Ordering};

pub const SYS_NATIVE_CALL: u64 = 0x4154;
const SHIM: &str = "libattesornative.dylib";
static CALLS: AtomicU64 = AtomicU64::new(0);

#[link(name = "z")]
extern "C" {
    fn crc32(crc: libc::c_ulong, buf: *const u8, len: libc::c_uint) -> libc::c_ulong;
    fn adler32(adler: libc::c_ulong, buf: *const u8, len: libc::c_uint) -> libc::c_ulong;
    fn compress2(dest: *mut u8, destlen: *mut libc::c_ulong, source: *const u8, sourcelen: libc::c_ulong, level: libc::c_int) -> libc::c_int;
    fn uncompress(dest: *mut u8, destlen: *mut libc::c_ulong, source: *const u8, sourcelen: libc::c_ulong) -> libc::c_int;
}

fn ptr(p: u64) -> u64 { p & 0x00ff_ffff_ffff_ffff }

pub fn hostcall(cpu: &mut Cpu) {
    let a = [cpu.x[0], cpu.x[1], cpu.x[2], cpu.x[3], cpu.x[4], cpu.x[5]];
    if a[0] == 0 {
        cpu.x[0] = CALLS.load(Ordering::Relaxed);
        cpu.cf = 0;
        return;
    }
    let r = unsafe {
        match a[0] {
            1 => crc32(a[1], ptr(a[2]) as *const u8, a[3] as u32),
            2 => adler32(a[1], ptr(a[2]) as *const u8, a[3] as u32),
            3 => compress2(ptr(a[1]) as *mut u8, ptr(a[2]) as *mut u64, ptr(a[3]) as *const u8, a[4], a[5] as i32) as i64 as u64,
            4 => uncompress(ptr(a[1]) as *mut u8, ptr(a[2]) as *mut u64, ptr(a[3]) as *const u8, a[4]) as i64 as u64,
            _ => { cpu.x[0] = libc::ENOSYS as u64; cpu.cf = 1; return; }
        }
    };
    CALLS.fetch_add(1, Ordering::Relaxed);
    cpu.x[0] = r;
    cpu.cf = 0;
}

pub fn shim_path() -> Option<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("ATTESOR_NATIVE_DIR") {
        let path = std::path::PathBuf::from(d).join(SHIM);
        return path.is_file().then_some(path);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() { dirs.push(d.join("native")); dirs.push(d.to_path_buf()); }
    }
    if let Some(h) = std::env::var_os("HOME") { dirs.push(std::path::Path::new(&h).join(".attesor/native")); }
    dirs.into_iter().map(|d: std::path::PathBuf| d.join(SHIM)).find(|p| p.is_file())
}

pub fn is_shim(path: &str) -> bool {
    std::path::Path::new(path).file_name().is_some_and(|n| n == SHIM)
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
    if std::env::var("ATTESOR_NATIVE_LIBS").as_deref() != Ok("1") { return Ok(()); }
    let shim = shim_path().ok_or("ATTESOR_NATIVE_LIBS=1: libattesornative.dylib missing; run native/build.sh and set ATTESOR_NATIVE_DIR")?;
    let shim = shim.to_string_lossy();
    if let Some(e) = envp.iter_mut().find(|e| e.starts_with("DYLD_INSERT_LIBRARIES=")) {
        e.push(':'); e.push_str(&shim);
    } else { envp.push(format!("DYLD_INSERT_LIBRARIES={shim}")); }
    Ok(())
}
