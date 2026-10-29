//! execve / posix_spawn. Native (x86-64 or universal) programs run directly on
//! the host (except owned x86 GUI stubs); ARM programs are re-launched through Attesor, carrying over
//! the sysroot and options so children (e.g. Chromium/Electron helper processes)
//! run in the same environment.

use crate::hostsys::{self, SysResult};
use std::ffi::CString;
use std::io::Read;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;

const SYS_EXECVE: u64 = 59;

/// Options to forward to re-launched children (`--sysroot X`, `--dyld D`, ...).
static FORWARD: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn configure(flags: Vec<String>) {
    *FORWARD.lock().unwrap() = flags;
}

const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;

/// An owned x86 stub is a GUI entry, not a genuine native implementation.
/// Programmatic exec/spawn selects its ARM slice directly, retaining Node mode,
/// arguments and descriptors instead of passing through the GUI runner.
fn needs_translation(path: &str) -> bool {
    let Ok(file) = std::fs::File::open(path) else { return false };
    let Ok(meta) = file.metadata() else { return false };
    let size = meta.len();
    let mut header = [0u8; 32];
    if size < 32 || file.read_exact_at(&mut header, 0).is_err() { return false; }
    let magic = u32::from_be_bytes(header[..4].try_into().unwrap());
    let little = matches!(magic, 0xbeba_feca | 0xbfba_feca);
    let wide = matches!(magic, 0xcafe_babf | 0xbfba_feca);
    if matches!(magic, 0xcafe_babe | 0xcafe_babf | 0xbeba_feca | 0xbfba_feca) {
        let count = word32(&header, 4, little) as u64;
        let step = if wide { 32 } else { 20 };
        let table = 8 + step * count;
        if count == 0 || count > 64 || table > size { return false; }
        let (mut arm, mut x86, mut owned) = (false, false, false);
        for i in 0..count {
            let mut entry = [0u8; 32];
            if file.read_exact_at(&mut entry[..step as usize], 8 + i * step).is_err() { return false; }
            let cpu = word32(&entry, 0, little);
            let offset = if wide { word64(&entry, 8, little) } else { word32(&entry, 8, little) as u64 };
            let len = if wide { word64(&entry, 16, little) } else { word32(&entry, 12, little) as u64 };
            if offset < table || offset > size || len < 32 || len > size - offset { return false; }
            if cpu == CPU_TYPE_ARM64 { arm = true; }
            if cpu == CPU_TYPE_X86_64 {
                if x86 { return false; }
                x86 = true;
                owned = owned_x86_stub(&file, offset, len);
            }
        }
        arm && (!x86 || owned)
    } else {
        word32(&header, 0, true) == 0xfeed_facf && word32(&header, 4, true) == CPU_TYPE_ARM64
    }
}

fn word32(b: &[u8], at: usize, little: bool) -> u32 {
    let value = b[at..at + 4].try_into().unwrap();
    if little { u32::from_le_bytes(value) } else { u32::from_be_bytes(value) }
}
fn word64(b: &[u8], at: usize, little: bool) -> u64 {
    let value = b[at..at + 8].try_into().unwrap();
    if little { u64::from_le_bytes(value) } else { u64::from_be_bytes(value) }
}

fn stub_marker(file: &std::fs::File, mut offset: u64, mut len: u64) -> bool {
    const PREFIX: &[u8] = b"ATTESOR_STUB_V";
    let (mut matched, mut digits) = (0, false);
    let mut bytes = [0u8; 4096];
    while len != 0 {
        let count = len.min(bytes.len() as u64) as usize;
        if file.read_exact_at(&mut bytes[..count], offset).is_err() { return false; }
        for &byte in &bytes[..count] {
            if matched < PREFIX.len() && byte == PREFIX[matched] { matched += 1; }
            else if matched == PREFIX.len() && byte.is_ascii_digit() { digits = true; }
            else if matched == PREFIX.len() && digits && byte == 0 { return true; }
            else { matched = usize::from(byte == PREFIX[0]); digits = false; }
        }
        offset += count as u64;
        len -= count as u64;
    }
    false
}

fn owned_x86_stub(file: &std::fs::File, base: u64, size: u64) -> bool {
    let mut header = [0u8; 32];
    if size < 32 || file.read_exact_at(&mut header, base).is_err()
        || word32(&header, 0, true) != 0xfeed_facf || word32(&header, 4, true) != CPU_TYPE_X86_64 { return false; }
    let count = word32(&header, 16, true);
    let bytes = word32(&header, 20, true) as u64;
    if count > 4096 || bytes > size - 32 { return false; }
    let (mut at, end, mut found) = (32u64, 32 + bytes, false);
    for _ in 0..count {
        let mut command = [0u8; 72];
        if at > end || end - at < 8 || file.read_exact_at(&mut command[..8], base + at).is_err() { return false; }
        let kind = word32(&command, 0, true);
        let len = word32(&command, 4, true) as u64;
        if len < 8 || len > end - at { return false; }
        if kind == 0x19 {
            if len < 72 || file.read_exact_at(&mut command, base + at).is_err() { return false; }
            let sections = word32(&command, 64, true) as u64;
            if sections > (len - 72) / 80 { return false; }
            for i in 0..sections {
                let mut section = [0u8; 80];
                if file.read_exact_at(&mut section, base + at + 72 + 80 * i).is_err() { return false; }
                if &section[16..23] == b"__TEXT\0" &&
                    (&section[..8] == b"__const\0" || &section[..10] == b"__cstring\0") {
                    let len = word64(&section, 40, true);
                    let offset = word32(&section, 48, true) as u64;
                    if offset < end || offset > size || len > size - offset { return false; }
                    found |= stub_marker(file, base + offset, len);
                }
            }
        }
        at += len;
    }
    found && at == end
}

/// Darwin shebangs carry one optional argument, not shell-split arguments.
fn script_interpreter(path: &str) -> Option<(String, Option<String>)> {
    let mut bytes = [0u8; 4160];
    let count = std::fs::File::open(path).ok()?.read(&mut bytes).ok()?;
    let line = bytes[..count].strip_prefix(b"#!")?.split(|&c| c == b'\n').next()?;
    let line = std::str::from_utf8(line).ok()?.trim_start_matches([' ', '\t']);
    let split = line.find([' ', '\t']);
    let (interpreter, argument) = match split {
        Some(at) => (&line[..at], Some(line[at..].trim_matches([' ', '\t', '\r']))),
        None => (line, None),
    };
    if !needs_translation(interpreter) { return None; }
    Some((interpreter.to_string(), argument.filter(|a| !a.is_empty()).map(str::to_string)))
}

unsafe fn cstr(p: u64) -> String {
    if p == 0 {
        return String::new();
    }
    std::ffi::CStr::from_ptr(p as *const libc::c_char).to_string_lossy().into_owned()
}

unsafe fn read_argv(p: u64) -> Vec<String> {
    let mut v = Vec::new();
    if p == 0 {
        return v;
    }
    let mut i = 0;
    loop {
        let e = *((p + i * 8) as *const u64);
        if e == 0 || i > 4096 {
            break;
        }
        v.push(cstr(e));
        i += 1;
    }
    v
}

/// Environment for host processes (native children and the attesor re-launcher): the arm64 GPU/UI shims
/// come out of DYLD_INSERT_LIBRARIES (attesor injects them into guests itself), and the x86-64 exec shim
/// goes in, with ATTESOR_BIN, so that native middlemen (/bin/sh, pnpm, …) can start arm64 programs too.
/// Returns the new environment (keep alive until the call), and points `args` at it.
fn host_env(n: u64, args: &mut [u64; 8]) -> Option<(Vec<CString>, Vec<u64>)> {
    let eidx = if n == SYS_EXECVE { 2 } else { 4 };
    if args[eidx] == 0 {
        return None;
    }
    let guest_shims: Vec<String> = [crate::gpu::shim_path(), crate::gpu::ui_shim_path()].into_iter().flatten().collect();
    let exec_shim = crate::gpu::exec_shim_path();
    let mut env = unsafe { read_argv(args[eidx]) };
    let mut inserts: Vec<String> = Vec::new();
    env.retain(|e| match e.strip_prefix("DYLD_INSERT_LIBRARIES=") {
        Some(v) => {
            inserts.extend(v.split(':').filter(|p| !p.is_empty() && !crate::native::is_shim(p) && !guest_shims.iter().any(|s| s == p)).map(str::to_string));
            false
        }
        None => true,
    });
    if let Some(x) = exec_shim {
        if !inserts.contains(&x) {
            inserts.push(x);
        }
    }
    if !inserts.is_empty() {
        env.push(format!("DYLD_INSERT_LIBRARIES={}", inserts.join(":")));
    }
    if !env.iter().any(|e| e.starts_with("ATTESOR_BIN=")) {
        if let Ok(me) = std::env::current_exe() {
            env.push(format!("ATTESOR_BIN={}", me.to_string_lossy()));
        }
    }
    let cleaned: Vec<CString> = env.into_iter().filter_map(|e| CString::new(e).ok()).collect();
    let mut ptrs: Vec<u64> = cleaned.iter().map(|c| c.as_ptr() as u64).collect();
    ptrs.push(0);
    args[eidx] = ptrs.as_ptr() as u64;
    Some((cleaned, ptrs))
}

pub fn exec_like(n: u64, args: &[u64; 8]) -> SysResult {
    let (pidx, aidx) = if n == SYS_EXECVE { (0, 1) } else { (1, 3) };
    let path = unsafe { cstr(args[pidx]) };
    if path.is_empty() {
        return hostsys::unix(n, args);
    }
    let mut args = *args;
    let _env = host_env(n, &mut args);
    let args = &args;
    let script = if needs_translation(&path) { None } else {
        match script_interpreter(&path) {
            Some(target) => Some(target),
            None => return hostsys::unix(n, args),
        }
    };
    let Ok(me) = std::env::current_exe() else { return hostsys::unix(n, args) };
    let argv = unsafe { read_argv(args[aidx]) };
    let mut new_argv: Vec<String> = vec![me.to_string_lossy().into_owned()];
    new_argv.extend(FORWARD.lock().unwrap().iter().cloned());
    // The image path and argv[0] are independent. Darwin replaces argv[0]
    // with the interpreter path for scripts, but preserves it for binaries.
    new_argv.push("--guest-argv0".into());
    new_argv.push(script.as_ref().map(|(interpreter, _)| interpreter.clone())
        .unwrap_or_else(|| argv.first().cloned().unwrap_or_default()));
    if let Some((interpreter, argument)) = script {
        new_argv.push(interpreter);
        if let Some(argument) = argument { new_argv.push(argument); }
    }
    new_argv.push(path);
    new_argv.extend(argv.into_iter().skip(1));
    let cstrs: Vec<CString> = new_argv.iter().filter_map(|s| CString::new(s.as_str()).ok()).collect();
    let mut ptrs: Vec<u64> = cstrs.iter().map(|c| c.as_ptr() as u64).collect();
    ptrs.push(0);
    let mut a = *args;
    a[pidx] = cstrs[0].as_ptr() as u64;
    a[aidx] = ptrs.as_ptr() as u64;
    let r = hostsys::unix(n, &a);
    drop(cstrs);
    r
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn macho(cpu: u32, marker: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 184 + marker.len()];
        for (at, value) in [(0, 0xfeed_facf), (4, cpu), (16, 1), (20, 152),
                            (32, 0x19), (36, 152), (96, 1), (152, 184)] {
            b[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        b[104..112].copy_from_slice(b"__const\0");
        b[120..127].copy_from_slice(b"__TEXT\0");
        b[144..152].copy_from_slice(&(marker.len() as u64).to_le_bytes());
        b[184..].copy_from_slice(marker);
        b
    }
    fn fat(entries: &[(u32, &[u8])], wide: bool, little: bool) -> Vec<u8> {
        let step = if wide { 32 } else { 20 };
        let mut b = vec![0u8; 8 + entries.len() * step];
        let magic: u32 = if wide { 0xcafe_babf } else { 0xcafe_babe };
        b[..4].copy_from_slice(&if little { magic.to_le_bytes() } else { magic.to_be_bytes() });
        let n = entries.len() as u32;
        b[4..8].copy_from_slice(&if little { n.to_le_bytes() } else { n.to_be_bytes() });
        for (i, &(cpu, payload)) in entries.iter().enumerate() {
            let at = 8 + i * step;
            b[at..at + 4].copy_from_slice(&if little { cpu.to_le_bytes() } else { cpu.to_be_bytes() });
            if wide {
                let off = b.len() as u64; let len = payload.len() as u64;
                b[at + 8..at + 16].copy_from_slice(&if little { off.to_le_bytes() } else { off.to_be_bytes() });
                b[at + 16..at + 24].copy_from_slice(&if little { len.to_le_bytes() } else { len.to_be_bytes() });
            } else {
                let off = b.len() as u32; let len = payload.len() as u32;
                b[at + 8..at + 12].copy_from_slice(&if little { off.to_le_bytes() } else { off.to_be_bytes() });
                b[at + 12..at + 16].copy_from_slice(&if little { len.to_le_bytes() } else { len.to_be_bytes() });
            }
            b.extend_from_slice(payload);
        }
        b
    }
    #[test]
    fn owned_stub_selection_and_bounds() {
        let dir = std::env::temp_dir().join(format!("attesor-arch-unit-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("candidate");
        let arm = macho(CPU_TYPE_ARM64, b"ATTESOR_STUB_V2\0");
        let native = macho(CPU_TYPE_X86_64, b"genuine native\0");
        let stub = macho(CPU_TYPE_X86_64, b"ATTESOR_STUB_V2\0");
        let check = |bytes: &[u8], expected: bool| {
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(needs_translation(path.to_str().unwrap()), expected);
        };
        check(&arm, true); check(&native, false);
        for wide in [false, true] {
            for little in [false, true] {
                let own = fat(&[(CPU_TYPE_X86_64, &stub), (CPU_TYPE_ARM64, &arm)], wide, little);
                check(&own, true);
                check(&fat(&[(CPU_TYPE_X86_64, &native), (CPU_TYPE_ARM64, &arm)], wide, little), false);
                check(&own[..own.len() - 1], false);
                let mut invalid = own.clone();
                invalid[16..20].fill(0xff); check(&invalid, false);
            }
        }
        check(&fat(&[(CPU_TYPE_X86_64, &stub), (CPU_TYPE_X86_64, &native), (CPU_TYPE_ARM64, &arm)], false, false), false);
        let incomplete = macho(CPU_TYPE_X86_64, b"ATTESOR_STUB_V2");
        check(&fat(&[(CPU_TYPE_X86_64, &incomplete), (CPU_TYPE_ARM64, &arm)], false, false), false);
        let mut invalid = stub.clone(); invalid[36..40].copy_from_slice(&7u32.to_le_bytes());
        check(&fat(&[(CPU_TYPE_X86_64, &invalid), (CPU_TYPE_ARM64, &arm)], false, false), false);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
