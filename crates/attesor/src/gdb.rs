//! GDB remote serial protocol stub: `attesor --gdb PORT program…` waits for a debugger (lldb
//! `gdb-remote PORT`, gdb `target remote :PORT`) before the first guest instruction.
//! `--gdb-fd FD` instead uses an inherited connected stream socket (e.g. a Unix socketpair).
//!
//! The debugged thread runs in the interpreter, so a software breakpoint is just a pc check before each
//! step and single-stepping is one interpreter step; guest memory is never patched. Only the thread that
//! starts the program is debugged (threads it creates run normally). Supported: registers (g/G/p/P, with
//! an AArch64 target description), memory (m/M), Z0/z0 breakpoints (Z1 accepted as the same), c/s and
//! vCont, Ctrl-C, kill, detach, and an exit report.

use attesor_core::cpu::Cpu;
use attesor_core::interp;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, AtomicU16, Ordering};

static PORT: AtomicU16 = AtomicU16::new(0);
static CONNECTED_FD: AtomicI32 = AtomicI32::new(-1);
/// Socket of the connected debugger (-1: none), for the exit report.
static SOCK: AtomicI32 = AtomicI32::new(-1);
static NO_ACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Main executable and its load slide, for the hint on how to load its symbols.
static MAIN: std::sync::OnceLock<(String, u64)> = std::sync::OnceLock::new();

pub fn set_main(path: &str, slide: u64) {
    let _ = MAIN.set((path.to_string(), slide));
}

pub fn set_port(p: u16) {
    PORT.store(p, Ordering::Relaxed);
}

pub fn set_fd(fd: i32) {
    CONNECTED_FD.store(fd, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    PORT.load(Ordering::Relaxed) != 0 || CONNECTED_FD.load(Ordering::Relaxed) >= 0
}

/// The guest called exit(status): tell the debugger (best effort, the process ends right after).
pub fn on_exit(status: i32) {
    let fd = SOCK.load(Ordering::Relaxed);
    if fd >= 0 {
        let pkt = frame(&format!("W{:02x}", status as u8));
        unsafe { libc::write(fd, pkt.as_ptr() as *const libc::c_void, pkt.len()) };
    }
}

fn frame(data: &str) -> Vec<u8> {
    let sum = data.bytes().fold(0u8, |a, b| a.wrapping_add(b));
    format!("${data}#{sum:02x}").into_bytes()
}

trait Transport: Read + Write + AsRawFd {}
impl<T: Read + Write + AsRawFd> Transport for T {}

struct Conn {
    s: Box<dyn Transport>,
    buf: Vec<u8>,
    last_reply: Vec<u8>,
    last_stop: &'static str,
}

enum Incoming {
    Packet(String),
    Interrupt,
    Closed,
}

impl Conn {
    fn stop(&mut self, reason: &'static str) {
        self.last_stop = reason;
        self.send(reason);
    }

    fn send(&mut self, data: &str) {
        if log() {
            eprintln!("[gdb] <- {}", &data[..data.len().min(200)]);
        }
        self.last_reply = frame(data);
        let _ = self.s.write_all(&self.last_reply);
    }

    fn fill(&mut self) -> bool {
        let mut b = [0u8; 4096];
        match self.s.read(&mut b) {
            Ok(0) | Err(_) => false,
            Ok(n) => {
                self.buf.extend_from_slice(&b[..n]);
                true
            }
        }
    }

    /// Next packet (acknowledged unless in no-ack mode), or a Ctrl-C byte.
    fn recv(&mut self) -> Incoming {
        loop {
            while let Some(&c) = self.buf.first() {
                match c {
                    0x03 => {
                        self.buf.remove(0);
                        return Incoming::Interrupt;
                    }
                    b'$' => break,
                    b'-' => {
                        self.buf.remove(0);
                        if !NO_ACK.load(Ordering::Relaxed) {
                            let _ = self.s.write_all(&self.last_reply);
                        }
                    }
                    _ => {
                        self.buf.remove(0); // acks and noise
                    }
                }
            }
            if let Some(hash) = self.buf.iter().position(|&c| c == b'#') {
                if self.buf.len() >= hash + 3 {
                    let body: Vec<u8> = self.buf[1..hash].to_vec();
                    let checksum = std::str::from_utf8(&self.buf[hash + 1..hash + 3])
                        .ok().and_then(|s| u8::from_str_radix(s, 16).ok());
                    let valid = checksum == Some(body.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
                    self.buf.drain(..hash + 3);
                    if !valid {
                        if !NO_ACK.load(Ordering::Relaxed) { let _ = self.s.write_all(b"-"); }
                        continue;
                    }
                    if !NO_ACK.load(Ordering::Relaxed) {
                        let _ = self.s.write_all(b"+");
                    }
                    let p = unescape(&body);
                    if !p.is_ascii() {
                        self.send("E01");
                        continue;
                    }
                    if log() {
                        eprintln!("[gdb] -> {}", &p[..p.len().min(200)]);
                    }
                    return Incoming::Packet(p);
                }
            }
            if !self.fill() {
                return Incoming::Closed;
            }
        }
    }

    /// While the guest runs: has the debugger sent Ctrl-C?
    fn interrupted(&mut self) -> bool {
        let mut p = libc::pollfd { fd: fd_of(&*self.s), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut p, 1, 0) } > 0 {
            if !self.fill() {
                return true; // debugger gone: stop and let the main loop notice
            }
        }
        if let Some(i) = self.buf.iter().position(|&c| c == 0x03) {
            self.buf.remove(i);
            return true;
        }
        false
    }
}

/// ATTESOR_GDB_LOG=1: print the packets on stderr.
fn log() -> bool {
    static L: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *L.get_or_init(|| std::env::var_os("ATTESOR_GDB_LOG").is_some())
}

fn fd_of(s: &dyn Transport) -> i32 {
    s.as_raw_fd()
}

fn unescape(b: &[u8]) -> String {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'}' && i + 1 < b.len() {
            out.push(b[i + 1] ^ 0x20);
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_le(v: u64, bytes: usize) -> String {
    v.to_le_bytes()[..bytes].iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_le(h: &str) -> u64 {
    let mut v = 0u64;
    for i in (0..h.len() / 2).take(8) {
        let b = u64::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap_or(0);
        v |= b << (8 * i);
    }
    v
}

fn valid_hex(h: &str) -> bool {
    h.len() % 2 == 0 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex_bytes(h: &str) -> Option<Vec<u8>> {
    if !valid_hex(h) { return None; }
    Some((0..h.len() / 2).map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap()).collect())
}

/// Register numbering of the target description: x0-x30 (0-30), sp 31, pc 32, cpsr 33,
/// v0-v31 34-65, fpsr 66, fpcr 67.
fn reg_hex(cpu: &Cpu, n: usize) -> Option<String> {
    Some(match n {
        0..=31 => hex_le(cpu.x[n], 8),
        32 => hex_le(cpu.pc, 8),
        33 => hex_le((cpu.nzcv() as u64) << 28, 4),
        34..=65 => format!("{}{}", hex_le(cpu.v[n - 34][0], 8), hex_le(cpu.v[n - 34][1], 8)),
        66 => hex_le(cpu.fpsr, 4),
        67 => hex_le(cpu.fpcr, 4),
        _ => return None,
    })
}

fn reg_size(n: usize) -> usize {
    match n {
        0..=32 => 8,
        34..=65 => 16,
        _ => 4,
    }
}

fn set_reg(cpu: &mut Cpu, n: usize, h: &str) -> bool {
    if n >= 68 || h.len() != reg_size(n) * 2 || !valid_hex(h) { return false; }
    match n {
        0..=31 => cpu.x[n] = parse_le(h),
        32 => cpu.pc = parse_le(h),
        33 => cpu.set_nzcv((parse_le(h) >> 28) as u32 & 15),
        34..=65 => {
            if h.len() < 32 {
                return false;
            }
            cpu.v[n - 34] = [parse_le(&h[..16]), parse_le(&h[16..32])];
        }
        66 => cpu.fpsr = parse_le(h),
        67 => {
            cpu.fpcr = parse_le(h);
            interp::sys_sync_host_fp_mode(cpu.fpcr);
        }
        _ => return false,
    }
    true
}

const TARGET_XML: &str = r#"<?xml version="1.0"?>
<!DOCTYPE target SYSTEM "gdb-target.dtd">
<target version="1.0">
<architecture>aarch64</architecture>
<feature name="org.gnu.gdb.aarch64.core">
REGS_CORE</feature>
<feature name="org.gnu.gdb.aarch64.fpu">
<vector id="v2d" type="ieee_double" count="2"/>
<vector id="v2u" type="uint64" count="2"/>
<vector id="v4f" type="ieee_single" count="4"/>
<vector id="v4u" type="uint32" count="4"/>
<vector id="v8u" type="uint16" count="8"/>
<vector id="v16u" type="uint8" count="16"/>
<union id="vnd"><field name="f" type="v2d"/><field name="u" type="v2u"/></union>
<union id="vns"><field name="f" type="v4f"/><field name="u" type="v4u"/></union>
<union id="vnh"><field name="u" type="v8u"/></union>
<union id="vnb"><field name="u" type="v16u"/></union>
<union id="vnq"><field name="u" type="uint128"/></union>
<union id="aarch64v"><field name="d" type="vnd"/><field name="s" type="vns"/><field name="h" type="vnh"/><field name="b" type="vnb"/><field name="q" type="vnq"/></union>
REGS_FPU</feature>
</target>
"#;

fn target_xml() -> String {
    // dwarf_regnum / generic / encoding / format are lldb's extensions (gdb ignores them): lldb needs
    // the DWARF numbers to find variables kept in registers, and the generic roles for calls and unwinding.
    let mut core = String::new();
    for i in 0..31 {
        let generic = match i {
            0..=7 => format!(" generic=\"arg{}\"", i + 1),
            29 => " generic=\"fp\"".to_string(),
            30 => " generic=\"ra\"".to_string(),
            _ => String::new(),
        };
        let alt = match i { 29 => " altname=\"fp\"", 30 => " altname=\"lr\"", _ => "" };
        core += &format!("<reg name=\"x{i}\" bitsize=\"64\" type=\"int\" regnum=\"{i}\" dwarf_regnum=\"{i}\" ehframe_regnum=\"{i}\" encoding=\"uint\" format=\"hex\" group=\"general\"{generic}{alt}/>\n");
    }
    core += "<reg name=\"sp\" bitsize=\"64\" type=\"data_ptr\" regnum=\"31\" dwarf_regnum=\"31\" ehframe_regnum=\"31\" encoding=\"uint\" format=\"hex\" group=\"general\" generic=\"sp\"/>\n";
    core += "<reg name=\"pc\" bitsize=\"64\" type=\"code_ptr\" regnum=\"32\" dwarf_regnum=\"32\" ehframe_regnum=\"32\" encoding=\"uint\" format=\"hex\" group=\"general\" generic=\"pc\"/>\n";
    core += "<reg name=\"cpsr\" bitsize=\"32\" type=\"int\" regnum=\"33\" encoding=\"uint\" format=\"hex\" group=\"general\" generic=\"flags\"/>\n";
    let mut fpu = String::new();
    for i in 0..32 {
        fpu += &format!("<reg name=\"v{i}\" bitsize=\"128\" type=\"aarch64v\" regnum=\"{}\" dwarf_regnum=\"{}\" encoding=\"vector\" format=\"vector-uint8\" group=\"float\"/>\n", 34 + i, 64 + i);
    }
    fpu += "<reg name=\"fpsr\" bitsize=\"32\" type=\"int\" regnum=\"66\"/>\n";
    fpu += "<reg name=\"fpcr\" bitsize=\"32\" type=\"int\" regnum=\"67\"/>\n";
    TARGET_XML.replace("REGS_CORE", &core).replace("REGS_FPU", &fpu)
}

/// qXfer read reply for `annex` at offset/length.
fn xfer(doc: &str, args: &str) -> String {
    let (off, len) = args.split_once(',').unwrap_or(("0", "0"));
    let off = usize::from_str_radix(off, 16).unwrap_or(0).min(doc.len());
    let len = usize::from_str_radix(len, 16).unwrap_or(0);
    let end = off.saturating_add(len).min(doc.len());
    let chunk = &doc[off..end];
    let mut out = String::from(if end < doc.len() { "m" } else { "l" });
    for c in chunk.chars() {
        // escape the protocol's special characters
        if matches!(c, '#' | '$' | '}' | '*') {
            out.push('}');
            out.push((c as u8 ^ 0x20) as char);
        } else {
            out.push(c);
        }
    }
    out
}

const STOP: &str = "T05thread:1;";

fn symbol_hint(port: Option<u16>) {
    if let Some((path, slide)) = MAIN.get() {
        let name = path.rsplit('/').next().unwrap_or(path);
        if let Some(port) = port {
            eprintln!("attesor: symbols: lldb -o 'target create {path}' -o 'gdb-remote {port}' -o 'target modules load --file {name} --slide {slide:#x}'");
        } else {
            eprintln!("attesor: symbols: target create {path}; target modules load --file {name} --slide {slide:#x}");
        }
    }
}

/// Run `cpu` under the debugger (instead of the JIT) until the guest thread ends.
pub fn run(cpu: &mut Cpu) {
    let port = PORT.load(Ordering::Relaxed);
    let fd = CONNECTED_FD.swap(-1, Ordering::Relaxed);
    let s: Box<dyn Transport> = if fd >= 0 {
        let mut ty: libc::c_int = 0;
        let mut len = std::mem::size_of_val(&ty) as libc::socklen_t;
        let valid = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_TYPE,
            &mut ty as *mut _ as *mut libc::c_void, &mut len) == 0 };
        if !valid || ty != libc::SOCK_STREAM {
            eprintln!("attesor: --gdb-fd: {fd} is not a stream socket");
            std::process::exit(1);
        }
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        eprintln!("attesor: debugger connected on inherited fd {fd}; stopped at {:#x}", cpu.pc);
        symbol_hint(None);
        Box::new(unsafe { UnixStream::from_raw_fd(fd) })
    } else {
        let listener = match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("attesor: --gdb: cannot listen on 127.0.0.1:{port}: {e}");
                std::process::exit(1);
            }
        };
        eprintln!("attesor: waiting for a debugger on 127.0.0.1:{port} (lldb: gdb-remote {port}); stopped at {:#x}", cpu.pc);
        symbol_hint(Some(port));
        let (s, _) = match listener.accept() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("attesor: --gdb: accept: {e}");
                std::process::exit(1);
            }
        };
        let _ = s.set_nodelay(true);
        Box::new(s)
    };
    SOCK.store(fd_of(&*s), Ordering::Relaxed);
    let mut c = Conn { s, buf: Vec::new(), last_reply: Vec::new(), last_stop: STOP };
    let mut bps: HashSet<u64> = HashSet::new();
    let mut allocations: HashMap<u64, (usize, i32)> = HashMap::new();
    let xml = target_xml();
    loop {
        let pkt = match c.recv() {
            Incoming::Packet(p) => p,
            Incoming::Interrupt => {
                c.stop("T02thread:1;");
                continue;
            }
            Incoming::Closed => {
                // debugger went away: keep running the program normally
                SOCK.store(-1, Ordering::Relaxed);
                crate::engine::interp_loop(cpu);
                return;
            }
        };
        // lldb appends the thread to register packets (QThreadSuffixSupported): one thread, drop it
        let pkt = match pkt.find(";thread:") {
            Some(i) if matches!(pkt.as_bytes().first(), Some(b'g' | b'G' | b'p' | b'P')) => pkt[..i].to_string(),
            _ => pkt,
        };
        let (cmd, rest) = pkt.split_at(1.min(pkt.len()));
        match cmd {
            "?" => c.send(c.last_stop),
            "g" => {
                let all: String = (0..68).filter_map(|n| reg_hex(cpu, n)).collect();
                c.send(&all);
            }
            "G" => {
                let size: usize = (0..68).map(|n| reg_size(n) * 2).sum();
                if rest.len() != size || !valid_hex(rest) {
                    c.send("E01");
                    continue;
                }
                let mut pos = 0;
                for n in 0..68 {
                    let w = reg_size(n) * 2;
                    if pos + w > rest.len() {
                        break;
                    }
                    set_reg(cpu, n, &rest[pos..pos + w]);
                    pos += w;
                }
                c.send("OK");
            }
            "p" => match usize::from_str_radix(rest, 16).ok().and_then(|n| reg_hex(cpu, n)) {
                Some(h) => c.send(&h),
                None => c.send("E01"),
            },
            "P" => {
                let ok = rest.split_once('=').and_then(|(n, v)| usize::from_str_radix(n, 16).ok().map(|n| set_reg(cpu, n, v))).unwrap_or(false);
                c.send(if ok { "OK" } else { "E01" });
            }
            "m" => {
                let parsed = rest.split_once(',').and_then(|(a, l)| Some((u64::from_str_radix(a, 16).ok()?, usize::from_str_radix(l, 16).ok()?)));
                match parsed {
                    Some((a, l)) => {
                        if a.checked_add(l.min(0x10000) as u64).is_none() {
                            c.send("E14");
                            continue;
                        }
                        let mut b = vec![0u8; l.min(0x10000)];
                        // partial reads: as much as is mapped, page by page
                        let mut got = 0;
                        while got < b.len() {
                            let chunk = (0x1000 - ((a + got as u64) & 0xfff) as usize).min(b.len() - got);
                            if !crate::guestmem::read(a + got as u64, &mut b[got..got + chunk]) {
                                break;
                            }
                            got += chunk;
                        }
                        if got == 0 && !b.is_empty() {
                            c.send("E14");
                        } else {
                            let h: String = b[..got].iter().map(|x| format!("{x:02x}")).collect();
                            c.send(&h);
                        }
                    }
                    None => c.send("E01"),
                }
            }
            "M" => {
                let ok = rest.split_once(':').and_then(|(al, data)| {
                    let (a, len) = al.split_once(',')?;
                    let a = u64::from_str_radix(a, 16).ok()?;
                    let len = usize::from_str_radix(len, 16).ok()?;
                    let bytes = hex_bytes(data)?;
                    if len != bytes.len() || len > 0x10000 || a.checked_add(len as u64).is_none() { return None; }
                    // LLDB uploads expression code into allocations requested as RX.
                    // Permit a debugger write only to its own full allocation, then
                    // restore its declared permissions before execution resumes.
                    let owned = allocations.iter().find_map(|(&base, &(size, prot))| {
                        (a >= base && a.checked_add(len as u64)? <= base.checked_add(size as u64)?)
                            .then_some((base, size, prot))
                    });
                    let temporary = owned.filter(|&(_, _, prot)| prot & libc::PROT_WRITE == 0);
                    if let Some((base, size, prot)) = temporary {
                        if unsafe { libc::mprotect(base as *mut libc::c_void, size, prot | libc::PROT_WRITE) } != 0 {
                            return Some(false);
                        }
                    }
                    let mut ok = crate::guestmem::write(a, &bytes);
                    if let Some((base, size, prot)) = temporary {
                        ok &= unsafe { libc::mprotect(base as *mut libc::c_void, size, prot) } == 0;
                    }
                    if ok {
                        crate::engine::invalidate_range(a, bytes.len() as u64);
                    }
                    Some(ok)
                });
                c.send(if ok == Some(true) { "OK" } else { "E14" });
            }
            "Z" | "z" => {
                let mut f = rest.split(',');
                let (kind, addr) = (f.next().unwrap_or(""), f.next().and_then(|a| u64::from_str_radix(a, 16).ok()));
                if matches!(kind, "0" | "1") && (f.next() != Some("4") || addr.map(|a| a & 3 != 0).unwrap_or(true)) {
                    c.send("E01");
                    continue;
                }
                match (kind, addr) {
                    ("0" | "1", Some(a)) => {
                        if cmd == "Z" { bps.insert(a); } else { bps.remove(&a); }
                        c.send("OK");
                    }
                    _ => c.send(""), // watchpoints: unsupported
                }
            }
            "c" | "s" => {
                if !rest.is_empty() {
                    match u64::from_str_radix(rest, 16) {
                        Ok(a) => cpu.pc = a,
                        Err(_) => { c.send("E01"); continue; }
                    }
                }
                if !resume(cpu, &mut c, &bps, cmd == "s") {
                    return;
                }
            }
            "v" if pkt == "vCont?" => c.send("vCont;c;s"),
            "v" if pkt.starts_with("vCont;") => {
                // the first action decides (one thread)
                let act = pkt["vCont;".len()..].split(';').next().unwrap_or("");
                let action = act.split(':').next().unwrap_or("");
                if !matches!(action, "c" | "s") {
                    c.send("E01");
                    continue;
                }
                let step = action == "s";
                if !resume(cpu, &mut c, &bps, step) {
                    return;
                }
            }
            "v" if pkt.starts_with("vMustReplyEmpty") => c.send(""),
            // lldb: allocate / free memory for expressions (guest addresses are host addresses)
            "_" if pkt.starts_with("_M") => {
                let request = pkt[2..].split_once(',').and_then(|(size, perms)| {
                    let size = usize::from_str_radix(size, 16).ok()?;
                    if size == 0 || size > 16 * 1024 * 1024 || perms.is_empty()
                        || !perms.bytes().all(|b| matches!(b, b'r' | b'w' | b'x')) { return None; }
                    let prot = (if perms.contains('r') { libc::PROT_READ } else { 0 })
                        | (if perms.contains('w') { libc::PROT_WRITE } else { 0 })
                        | (if perms.contains('x') { libc::PROT_EXEC } else { 0 });
                    Some((size, prot))
                });
                if let Some((size, prot)) = request {
                    let p = unsafe { libc::mmap(std::ptr::null_mut(), size, prot,
                        libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
                    if p == libc::MAP_FAILED { c.send("E01"); } else {
                        allocations.insert(p as u64, (size, prot));
                        c.send(&format!("{:x}", p as u64));
                    }
                } else { c.send("E01"); }
            }
            "_" if pkt.starts_with("_m") => {
                let addr = u64::from_str_radix(&pkt[2..], 16).ok();
                let ok = addr.and_then(|addr| allocations.get(&addr).copied().map(|size| (addr, size)))
                    .map(|(addr, (size, _))| {
                        if unsafe { libc::munmap(addr as *mut libc::c_void, size) } == 0 {
                            allocations.remove(&addr);
                            crate::engine::invalidate_range(addr, size as u64);
                            true
                        } else { false }
                    }).unwrap_or(false);
                c.send(if ok { "OK" } else { "E01" });
            }
            "k" => {
                SOCK.store(-1, Ordering::Relaxed);
                std::process::exit(137);
            }
            "D" => {
                c.send("OK");
                SOCK.store(-1, Ordering::Relaxed);
                crate::engine::interp_loop(cpu);
                return;
            }
            "H" | "T" => c.send("OK"),
            "q" | "Q" => {
                let reply = query(&pkt, cpu, &xml, c.last_stop);
                c.send(&reply);
                if pkt == "QStartNoAckMode" {
                    NO_ACK.store(true, Ordering::Relaxed);
                }
            }
            _ => c.send(""),
        }
    }
}

fn query(pkt: &str, cpu: &Cpu, xml: &str, stop: &str) -> String {
    let pid = std::process::id();
    let arch = "cputype:16777228;cpusubtype:0;ostype:macosx;vendor:apple;endian:little;ptrsize:8;";
    if pkt.starts_with("qSupported") {
        return "PacketSize=20000;qXfer:features:read+;QStartNoAckMode+;vContSupported+".into();
    }
    if let Some(a) = pkt.strip_prefix("qXfer:features:read:target.xml:") {
        return xfer(xml, a);
    }
    match pkt {
        "QStartNoAckMode" | "QThreadSuffixSupported" | "QListThreadsInStopReply" => "OK".into(),
        "qAttached" => "1".into(),
        "qC" => "QC1".into(),
        "qfThreadInfo" => "m1".into(),
        "qsThreadInfo" => "l".into(),
        "qHostInfo" => format!("{arch}hostname:attesor;"),
        "qProcessInfo" => format!("pid:{pid:x};parent-pid:{:x};{arch}", unsafe { libc::getppid() }),
        "qOffsets" => "Text=0;Data=0;Bss=0".into(),
        "qSymbol::" => "OK".into(),
        _ if pkt.starts_with("qThreadStopInfo") => stop.into(),
        _ if pkt.starts_with("qRegisterInfo") => {
            // lldb's register discovery: answer from the target description instead
            let _ = cpu;
            String::new()
        }
        _ => String::new(),
    }
}

/// Run until a breakpoint, a single step, Ctrl-C or the end of the thread. Sends the stop reply;
/// false when the thread ended.
fn resume(cpu: &mut Cpu, c: &mut Conn, bps: &HashSet<u64>, step: bool) -> bool {
    let mut n = 0u64;
    loop {
        // the instruction at the resume pc runs even if it has a breakpoint (the debugger stepped over it)
        if n > 0 && (step || bps.contains(&cpu.pc)) {
            c.stop(STOP);
            return true;
        }
        n += 1;
        match interp::step(cpu) {
            Ok(()) => {}
            // traps stop in the debugger (pc stays on the instruction) instead of ending the program
            Err(interp::Exit::Brk(_)) => {
                c.stop(STOP);
                return true;
            }
            Err(e @ (interp::Exit::Udf(_) | interp::Exit::Unimplemented(_))) => {
                if log() { eprintln!("[gdb] illegal guest instruction at {:#x}: {e:?}", cpu.pc); }
                c.stop("T04thread:1;");
                return true;
            }
            Err(e) => {
                if !crate::engine::handle_exit(cpu, e) {
                    c.send("W00");
                    SOCK.store(-1, Ordering::Relaxed);
                    return false;
                }
            }
        }
        if n & 0xfff == 0 && c.interrupted() {
            c.stop("T02thread:1;");
            return true;
        }
    }
}
