//! JIT self-test: translate single instructions and compare the result with
//! the reference interpreter (`attesor --selftest-jit [class] [count]`).

use super::*;
use attesor_core::disasm::disasm;
use super::emit::DEAD_FLAG_SKIPS;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn val(&mut self) -> u64 {
        match self.next() % 8 {
            0 => 0,
            1 => u64::MAX,
            2 => self.next() % 64,
            3 => 1u64 << (self.next() % 64),
            4 => self.next() as u32 as u64,
            5 => 0x8000_0000_0000_0000 | (self.next() % 5),
            _ => self.next(),
        }
    }
}

fn snapshot(c: &Cpu) -> Vec<u64> {
    let mut v: Vec<u64> = c.x.to_vec();
    v.push(c.nzcv() as u64);
    for i in 0..32 {
        v.push(c.v[i][0]);
        v.push(c.v[i][1]);
    }
    v.push(c.tpidr_el0);
    v
}

/// Pairs of (flag-setting instruction, flag consumer) to exercise compare-and-branch / select fusion.
fn run_selftest_fuse(count: usize, deep: bool) {
    // (mask, value) of the flag-setting encodings: add/sub imm, shifted reg, extended reg (all with S),
    // logical imm, logical shifted reg (ANDS/BICS).
    const SETTERS: [(u32, u32); 5] = [
        (0x3F80_0000, 0x3100_0000),
        (0x3F20_0000, 0x2B00_0000),
        (0x3F20_0000, 0x2B20_0000),
        (0x7F80_0000, 0x7200_0000),
        (0x7F00_0000, 0x6A00_0000),
    ];
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let (mut ok, mut bad, mut skipped, mut native, mut fused) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for it in 0..count {
        let (mask, val) = SETTERS[(rng.next() % SETTERS.len() as u64) as usize];
        let setter = (rng.next() as u32 & !mask) | val;
        let branch = deep || it % 2 == 0;
        let consumer: u32 = if branch {
            // b.cond +8 (skips one instruction)
            0x5400_0000 | (2 << 5) | (rng.next() as u32 % 14)
        } else {
            let base = 0x1A80_0000u32; // csel family
            let f = rng.next() as u32;
            (f & !0x3FE0_0800 & !0x0000_0000) | base | (f & 0xC000_0000) | (f & 0x0000_0400)
        };
        if !branch && (consumer & 0x3FE0_0800) != 0x1A80_0000 {
            skipped += 1;
            continue;
        }
        let mut init = Cpu::new();
        for i in 0..31 {
            init.x[i] = rng.val();
        }
        init.x[31] = rng.val();
        init.set_nzcv((rng.next() % 16) as u32);
        init.pc = page;
        let mut r = Cpu::new();
        *r = clone_cpu(&init);
        // deep: [setter, b.cond +8, A, B, svc] with A and B overwriting all flags
        let (wa, wb) = {
            let (m1, v1) = SETTERS[(rng.next() % SETTERS.len() as u64) as usize];
            let (m2, v2) = SETTERS[(rng.next() % SETTERS.len() as u64) as usize];
            ((rng.next() as u32 & !m1) | v1, (rng.next() as u32 & !m2) | v2)
        };
        unsafe {
            *(page as *mut u32) = setter;
            *((page + 4) as *mut u32) = consumer;
            if deep {
                *((page + 8) as *mut u32) = wa;
                *((page + 12) as *mut u32) = wb;
                *((page + 16) as *mut u32) = 0xD400_1001;
            } else {
                *((page + 8) as *mut u32) = 0xD400_1001;
                *((page + 12) as *mut u32) = 0xD400_1001;
            }
        }
        if deep {
            // reference: run to the SVC
            let mut steps = 0;
            let mut bad_ref = false;
            r.pc = page;
            loop {
                let insn = unsafe { *(r.pc as *const u32) };
                match interp::exec(&mut r, insn) {
                    Ok(()) => {}
                    Err(interp::Exit::Svc(_)) => break,
                    Err(_) => { bad_ref = true; break; }
                }
                steps += 1;
                if steps > 8 { bad_ref = true; break; }
            }
            if bad_ref {
                skipped += 1;
                continue;
            }
        } else {
            if interp::exec(&mut r, setter).is_err() {
                skipped += 1;
                continue;
            }
            r.pc = page + 4;
            if interp::exec(&mut r, consumer).is_err() {
                skipped += 1;
                continue;
            }
        }
        invalidate_range(page, 32);
        let mut j = Cpu::new();
        *j = clone_cpu(&init);
        setup_cpu(&mut j);
        let fb0 = STATS_FALLBACK.load(Ordering::Relaxed);
        let host = lookup_or_translate(page);
        if STATS_FALLBACK.load(Ordering::Relaxed) == fb0 {
            native += 1;
        }
        unsafe { attesor_jit_enter(&mut *j, host) };
        let mut hops = 0;
        while deep && j.exit_reason != EXIT_SVC && j.exit_reason != EXIT_PENDING && hops < 6 {
            let h2 = lookup_or_translate(j.pc);
            unsafe { attesor_jit_enter(&mut *j, h2) };
            hops += 1;
        }
        let pending = PENDING.with(|p| p.take());
        if j.exit_reason == EXIT_PENDING || pending.is_some() {
            skipped += 1;
            continue;
        }
        let mut a = snapshot(&r);
        let mut b = snapshot(&j);
        if branch && !deep {
            a.push(r.pc);
            b.push(j.pc);
        }
        if a != b || ((!branch || deep) && j.exit_reason != EXIT_SVC) {
            bad += 1;
            if bad <= 20 {
                println!("MISMATCH {:08x} {} ; {:08x} {}", setter, disasm(setter), consumer, disasm(consumer));
                for i in 0..a.len() {
                    if a[i] != b[i] {
                        println!("    slot {i}: interp={:#x} jit={:#x}", a[i], b[i]);
                    }
                }
            }
        } else {
            ok += 1;
            fused += 1;
        }
    }
    println!("jit selftest [fuse{}]: ok={ok} mismatch={bad} skipped={skipped} native={native} pairs={fused} dead_flag_skips={}", if deep { "2" } else { "" }, DEAD_FLAG_SKIPS.load(Ordering::Relaxed));
}

/// Flag setter feeding a chain of 1-3 selects, then either a B.cond over a writer or straight-line code
/// up to a writer: the shape where the setter's guest flags are never stored (flags_dead_after_selects).
fn run_selftest_selchain(count: usize) {
    const SETTERS: [(u32, u32); 5] = [
        (0x3F80_0000, 0x3100_0000),
        (0x3F20_0000, 0x2B00_0000),
        (0x3F20_0000, 0x2B20_0000),
        (0x7F80_0000, 0x7200_0000),
        (0x7F00_0000, 0x6A00_0000),
    ];
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    let mut rng = Rng(0x5e1c_4a11_0bad_cafe);
    let (mut ok, mut bad, mut skipped, mut native) = (0u64, 0u64, 0u64, 0u64);
    let skips0 = DEAD_FLAG_SKIPS.load(Ordering::Relaxed);
    let setter_of = |rng: &mut Rng| {
        let (m, v) = SETTERS[(rng.next() % SETTERS.len() as u64) as usize];
        (rng.next() as u32 & !m) | v
    };
    for _ in 0..count {
        let mut prog = vec![setter_of(&mut rng)];
        // all selects test the same condition class often (eq/ne/hi...), sometimes different ones
        let cond0 = rng.next() as u32 % 14;
        for _ in 0..1 + rng.next() % 3 {
            let f = rng.next() as u32;
            let cond = if f & 1 != 0 { cond0 } else { (f >> 1) % 14 };
            prog.push((f & !0x3FE0_F800) | 0x1A80_0000 | (cond << 12) | (f & 0x0000_0400));
        }
        if rng.next() % 2 == 0 {
            prog.push(0x5400_0000 | (2 << 5) | (rng.next() as u32 % 14)); // b.cond +8
            prog.push(setter_of(&mut rng));
            prog.push(setter_of(&mut rng));
        } else {
            prog.push((rng.next() as u32 & !0xFF80_0000) | 0x9100_0000); // add (immediate), no flags
            prog.push(setter_of(&mut rng));
        }
        prog.push(0xD400_1001);
        let mut init = Cpu::new();
        for i in 0..31 {
            init.x[i] = rng.val();
        }
        init.x[31] = rng.val();
        init.set_nzcv((rng.next() % 16) as u32);
        init.pc = page;
        for (i, w) in prog.iter().enumerate() {
            unsafe { *((page + 4 * i as u64) as *mut u32) = *w };
        }
        let mut r = Cpu::new();
        *r = clone_cpu(&init);
        let mut steps = 0;
        let mut bad_ref = false;
        loop {
            let insn = unsafe { *(r.pc as *const u32) };
            match interp::exec(&mut r, insn) {
                Ok(()) => {}
                Err(interp::Exit::Svc(_)) => break,
                Err(_) => { bad_ref = true; break; }
            }
            steps += 1;
            if steps > 12 { bad_ref = true; break; }
        }
        if bad_ref {
            skipped += 1;
            continue;
        }
        invalidate_range(page, 64);
        let mut j = Cpu::new();
        *j = clone_cpu(&init);
        setup_cpu(&mut j);
        let fb0 = STATS_FALLBACK.load(Ordering::Relaxed);
        let host = lookup_or_translate(page);
        if STATS_FALLBACK.load(Ordering::Relaxed) == fb0 {
            native += 1;
        }
        unsafe { attesor_jit_enter(&mut *j, host) };
        let mut hops = 0;
        while j.exit_reason != EXIT_SVC && j.exit_reason != EXIT_PENDING && hops < 6 {
            let h2 = lookup_or_translate(j.pc);
            unsafe { attesor_jit_enter(&mut *j, h2) };
            hops += 1;
        }
        let pending = PENDING.with(|p| p.take());
        if j.exit_reason == EXIT_PENDING || pending.is_some() {
            skipped += 1;
            continue;
        }
        let (a, b) = (snapshot(&r), snapshot(&j));
        if a != b || j.exit_reason != EXIT_SVC {
            bad += 1;
            if bad <= 20 {
                println!("MISMATCH");
                for w in &prog {
                    println!("    {:08x} {}", w, disasm(*w));
                }
                for i in 0..a.len() {
                    if a[i] != b[i] {
                        println!("    slot {i}: interp={:#x} jit={:#x}", a[i], b[i]);
                    }
                }
            }
        } else {
            ok += 1;
        }
    }
    println!("jit selftest [selchain]: ok={ok} mismatch={bad} skipped={skipped} native={native} dead_flag_skips={}", DEAD_FLAG_SKIPS.load(Ordering::Relaxed) - skips0);
}

/// Random straight-line blocks over a small register set (so values are reused constantly): exercises
/// the JIT's register cache and flag fusion across instructions. Memory operations use x20-x23 as
/// bases, which no other instruction may write except through their own writeback.
/// A scalar/vector FP or SIMD&FP load/store instruction over v0-v5 (memory through x20-x23), or an
/// integer one: blocks of these exercise the V-register cache (`VCache`).
fn fp_insn(rng: &mut Rng) -> u32 {
    let r = |rng: &mut Rng| rng.next() as u32 % 6;
    let (d, n, m, a) = (r(rng), r(rng), r(rng), r(rng));
    let ty = if rng.next() % 3 == 0 { 0 } else { 1 << 22 }; // single / double
    let base = 20 + rng.next() as u32 % 4;
    match rng.next() % 14 {
        0 | 1 | 2 => 0x1E20_0800 | ty | (m << 16) | ((rng.next() as u32 % 4) << 12) | (n << 5) | d, // FMUL/FDIV/FADD/FSUB
        3 => 0x1E21_C000 | ty | (n << 5) | d, // FSQRT
        4 | 5 => 0x1F00_0000 | ty | ((rng.next() as u32 & 1) << 21) | (m << 16) | ((rng.next() as u32 & 1) << 15) | (a << 10) | (n << 5) | d, // FMADD family
        6 => 0x1E20_2000 | ty | (m << 16) | (n << 5) | ((rng.next() as u32 & 1) << 3), // FCMP (reg / #0)
        7 => 0x1E20_0C00 | ty | (m << 16) | ((rng.next() as u32 % 14) << 12) | (n << 5) | d, // FCSEL
        8 => 0x1E20_4000 | ty | ((rng.next() as u32 % 3) << 15) | (n << 5) | d, // FMOV/FABS/FNEG
        9 => 0x4E20_D400 | ((ty >> 22) << 22) | (m << 16) | (n << 5) | d, // FADD (vector, 4s / 2d)
        10 => 0xFD40_0000 | ((rng.next() as u32 % 64) << 10) | (base << 5) | d, // LDR d
        11 => 0xFD00_0000 | ((rng.next() as u32 % 64) << 10) | (base << 5) | d, // STR d
        12 => 0x9E67_0000 | (n << 5) | d, // FMOV d, x
        _ => 0x8B00_0000 | (m << 16) | (n << 5) | d, // ADD x (integer)
    }
}

/// Bit patterns for FP registers: ordinary numbers of both widths, zeros, infinities, quiet NaNs,
/// denormals. `fpnan` separately covers signalling priority and exact canonical payloads.
fn fp_val(rng: &mut Rng) -> u64 {
    let f = |rng: &mut Rng| -> u32 {
        match rng.next() % 8 {
            0 => 0x7FC0_0000 | (rng.next() as u32 & 0xFFFF), // qNaN
            1 => 0xFFC0_0000 | (rng.next() as u32 & 0xFFFF), // negative qNaN
            2 => 0x7F80_0000 | (rng.next() as u32 & 0x8000_0000), // inf
            3 => rng.next() as u32 & 0x807F_FFFF, // denormal / zero
            _ => ((rng.next() as f32 / u64::MAX as f32 - 0.5) * 100.0).to_bits(),
        }
    };
    match rng.next() % 10 {
        0 => 0x7FF8_0000_0000_0000 | (rng.next() & 0xFFFF),
        1 => 0xFFF8_0000_0000_0000 | (rng.next() & 0xFFFF),
        2 => 0x7FF0_0000_0000_0000 | (rng.next() & 0x8000_0000_0000_0000),
        3 => rng.next() & 0x800F_FFFF_FFFF_FFFF,
        4 | 5 => (f(rng) as u64) | (f(rng) as u64) << 32,
        _ => ((rng.next() as f64 / u64::MAX as f64 - 0.5) * 1000.0).to_bits(),
    }
}

fn run_selftest_blocks(count: usize, fp: bool) {
    const CLASSES: [(u32, u32); 6] = [
        (0x1C00_0000, 0x1000_0000), // dp immediate
        (0x0E00_0000, 0x0A00_0000), // dp register
        (0x0E00_0000, 0x0A00_0000),
        (0x3B00_0000, 0x3900_0000), // load/store unsigned offset
        (0x3B20_0C00, 0x3800_0400), // post-index
        (0x3B20_0C00, 0x3800_0C00), // pre-index
    ];
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    let mut buf = vec![0u8; 1 << 20];
    let bufp = buf.as_mut_ptr() as u64;
    let mut rng = Rng(0x0dd_ba11_f00d_cafe);
    let (mut ok, mut bad, mut skipped, mut insns, mut fallbacks) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for it in 0..count {
        let len = match std::env::var("ATTESOR_SELFTEST_LEN").ok().and_then(|v| v.parse().ok()) {
            Some(n) => n,
            None => 2 + (rng.next() % 10) as usize,
        };
        let mut prog: Vec<u32> = Vec::new();
        let mut tries = 0;
        while prog.len() < len && tries < 200 {
            tries += 1;
            if fp {
                prog.push(fp_insn(&mut rng));
                continue;
            }
            let (mask, val) = CLASSES[(rng.next() % CLASSES.len() as u64) as usize];
            let mut insn = (rng.next() as u32 & !mask) | val;
            let mem = mask != 0x1C00_0000 && mask != 0x0E00_0000;
            // small register set: rd, rn, rm in 0..6 (rn in 20..23 for memory operations)
            insn = (insn & !31) | (rng.next() as u32 % 6);
            if mem {
                insn = (insn & !(31 << 5)) | ((20 + rng.next() as u32 % 4) << 5);
                let rt2 = (rng.next() as u32) % 6;
                if insn & 0x3A00_0000 == 0x2800_0000 {
                    insn = (insn & !(31 << 10)) | (rt2 << 10);
                }
            } else {
                insn = (insn & !(31 << 5)) | ((rng.next() as u32 % 6) << 5);
                insn = (insn & !(31 << 16)) | ((rng.next() as u32 % 6) << 16);
            }
            if emit::is_terminator(insn) {
                continue;
            }
            prog.push(insn);
        }
        let mut init = Cpu::new();
        for i in 0..31 {
            init.x[i] = rng.val();
        }
        for i in 20..24 {
            init.x[i] = bufp + 0x40000 + (rng.next() % 0x60000 & !15);
        }
        init.x[31] = bufp + 0x80000;
        init.set_nzcv((rng.next() % 16) as u32);
        for i in 0..32 {
            init.v[i] = if fp { [fp_val(&mut rng), fp_val(&mut rng)] } else { [rng.val(), rng.val()] };
        }
        if fp {
            // some doubles in memory for LDR d
            for k in 0..0x2000 {
                let v = fp_val(&mut rng);
                buf[0x40000 + 8 * k..0x40000 + 8 * k + 8].copy_from_slice(&v.to_le_bytes());
            }
        }
        init.pc = page;
        let mem0: Vec<u8> = buf.clone();
        let mut r = Cpu::new();
        *r = clone_cpu(&init);
        let mut valid = true;
        for (i, w) in prog.iter().enumerate() {
            r.pc = page + 4 * i as u64;
            if interp::exec(&mut r, *w).is_err() {
                valid = false;
                break;
            }
        }
        if !valid {
            buf.copy_from_slice(&mem0);
            skipped += 1;
            continue;
        }
        let mem_ref = buf.clone();
        buf.copy_from_slice(&mem0);
        unsafe {
            for (i, w) in prog.iter().enumerate() {
                *((page + 4 * i as u64) as *mut u32) = *w;
            }
            *((page + 4 * prog.len() as u64) as *mut u32) = 0xD400_1001;
        }
        invalidate_range(page, (4 * prog.len() + 4) as u64);
        let mut j = Cpu::new();
        *j = clone_cpu(&init);
        setup_cpu(&mut j);
        let fb0 = STATS_FALLBACK.load(Ordering::Relaxed);
        let host = lookup_or_translate(page);
        fallbacks += STATS_FALLBACK.load(Ordering::Relaxed) - fb0;
        unsafe { attesor_jit_enter(&mut *j, host) };
        let pending = PENDING.with(|p| p.take());
        if j.exit_reason == EXIT_PENDING || pending.is_some() {
            skipped += 1;
            buf.copy_from_slice(&mem0);
            continue;
        }
        let a = snapshot(&r);
        let b = snapshot(&j);
        let mem_diff = buf != mem_ref;
        if a != b || mem_diff || j.exit_reason != EXIT_SVC {
            bad += 1;
            if bad <= 15 {
                println!("MISMATCH block #{it}:");
                for w in &prog {
                    println!("    {:08x} {}", w, disasm(*w));
                }
                for i in 0..a.len() {
                    if a[i] != b[i] {
                        let name = if i < 32 { format!("x{i}") } else if i == 32 { "nzcv".into() } else { format!("slot{i}") };
                        println!("    {name}: interp={:#x} jit={:#x}", a[i], b[i]);
                    }
                }
                if mem_diff {
                    println!("    memory differs");
                }
                if j.exit_reason != EXIT_SVC {
                    println!("    exit reason {}", j.exit_reason);
                }
            }
        } else {
            ok += 1;
            insns += prog.len() as u64;
        }
        buf.copy_from_slice(&mem0);
    }
    println!("jit selftest [{}]: ok={ok} mismatch={bad} skipped={skipped} insns={insns} fallback_insns={fallbacks}", if fp { "fpblocks" } else { "blocks" });
}

/// Deterministic NaN boundary matrix, including signalling/quiet priority,
/// signed canonical NaNs, invalid operations, destructive destinations and
/// mixed exceptional/ordinary vector lanes. No random sampling is needed to
/// hit the very sparse canonical patterns that caused the old failures.
fn run_selftest_fpnan(count: usize) {
    const VALUES32: [u32; 12] = [
        0, 0x8000_0000, 0x3f80_0000, 0xbf80_0000,
        0x7f80_0000, 0xff80_0000, 0x7fc0_0000, 0xffc0_0000,
        0x7fc0_1234, 0xffc0_5678, 0x7f80_1234, 0xff80_5678,
    ];
    const VALUES64: [u64; 12] = [
        0, 0x8000_0000_0000_0000, 0x3ff0_0000_0000_0000, 0xbff0_0000_0000_0000,
        0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_0000, 0xfff8_0000_0000_0000,
        0x7ff8_0000_0000_1234, 0xfff8_0000_0000_5678,
        0x7ff0_0000_0000_1234, 0xfff0_0000_0000_5678,
    ];
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE,
                   libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    let (mut ok, mut bad, mut skipped, mut native) = (0u64, 0u64, 0u64, 0u64);
    for it in 0..count {
        let double = it & 1 != 0;
        let kind = (it / 2) % 18;
        let inputs = it / 36;
        let indices = [inputs % 12, (inputs / 12) % 12, (inputs / 144) % 12];
        let d = 1 + (inputs % 3) as u32; // aliases n, m or addend
        let ty = if double { 1 << 22 } else { 0 };
        let q = double || inputs & 1 != 0;
        let qbit = if q { 1 << 30 } else { 0 };
        let insn = match kind {
            0..=3 => 0x1e20_0800 | ty | (2 << 16) | ((kind as u32) << 12) | (1 << 5) | d,
            4 => 0x1e21_c000 | ty | (1 << 5) | d,
            5..=8 => {
                let variant = (kind - 5) as u32;
                0x1f00_0000 | ty | ((variant >> 1) << 21) | (2 << 16)
                    | ((variant & 1) << 15) | (3 << 10) | (1 << 5) | d
            }
            9..=12 => {
                let base = [0x0e20_d400, 0x0ea0_d400, 0x2e20_dc00, 0x2e20_fc00][kind - 9];
                base | qbit | ty | (2 << 16) | (1 << 5) | d
            }
            13 | 14 => 0x0e20_cc00 | qbit | ty | (((kind - 13) as u32) << 23)
                | (2 << 16) | (1 << 5) | d,
            _ => {
                let base = [0x0f80_9000, 0x0f80_1000, 0x0f80_5000][kind - 15];
                let lane = (inputs / 3) as u32 & if double { 1 } else { 3 };
                base | qbit | ty | (2 << 16) | ((lane >> if double { 0 } else { 1 }) << 11)
                    | (if double { 0 } else { (lane & 1) << 21 }) | (1 << 5) | d
            }
        };
        let mut init = Cpu::new();
        for reg in 0..32 {
            let base = if (1..=3).contains(&reg) { indices[reg - 1] } else { reg % 12 };
            let mut bytes = [0u8; 16];
            if double {
                for lane in 0..2 {
                    bytes[lane * 8..lane * 8 + 8].copy_from_slice(&VALUES64[(base + 3 * lane) % 12].to_le_bytes());
                }
            } else {
                for lane in 0..4 {
                    bytes[lane * 4..lane * 4 + 4].copy_from_slice(&VALUES32[(base + 3 * lane) % 12].to_le_bytes());
                }
            }
            init.v[reg] = [u64::from_le_bytes(bytes[..8].try_into().unwrap()),
                           u64::from_le_bytes(bytes[8..].try_into().unwrap())];
        }
        if std::env::var_os("ATTESOR_SELFTEST_FPCR").is_some() {
            init.fpcr = (((it / 2) as u64 & 3) << 22) | (((it / 8) as u64 & 1) << 24);
        }
        interp::sys_sync_host_fp_mode(init.fpcr);
        init.pc = page;
        let mut r = Cpu::new();
        *r = clone_cpu(&init);
        if interp::exec(&mut r, insn).is_err() { skipped += 1; continue; }
        unsafe {
            *(page as *mut u32) = insn;
            *((page + 4) as *mut u32) = 0xd400_1001;
        }
        invalidate_range(page, 8);
        let mut j = Cpu::new();
        *j = clone_cpu(&init);
        setup_cpu(&mut j);
        let before = STATS_FALLBACK.load(Ordering::Relaxed);
        let host = lookup_or_translate(page);
        if STATS_FALLBACK.load(Ordering::Relaxed) == before { native += 1; }
        unsafe { attesor_jit_enter(&mut *j, host) };
        if snapshot(&r) != snapshot(&j) || j.exit_reason != EXIT_SVC {
            bad += 1;
            if bad <= 20 {
                println!("MISMATCH fpnan #{it}: {insn:08x} {} inputs={indices:?}", disasm(insn));
                for reg in 0..32 {
                    if r.v[reg] != j.v[reg] {
                        println!("    v{reg}: interp={:x?} jit={:x?}", r.v[reg], j.v[reg]);
                    }
                }
            }
        } else { ok += 1; }
    }
    unsafe { libc::munmap(page as *mut libc::c_void, 0x1000); }
    println!("jit selftest [fpnan]: ok={ok} mismatch={bad} skipped={skipped} native={native}");
}

pub fn run_selftest(class: &str, count: usize) {
    if class == "fpnan" {
        return run_selftest_fpnan(count);
    }
    if class == "fuse" {
        return run_selftest_fuse(count, false);
    }
    if class == "fuse2" {
        return run_selftest_fuse(count, true);
    }
    if class == "blocks" || class == "fpblocks" {
        return run_selftest_blocks(count, class == "fpblocks");
    }
    if class == "selchain" {
        return run_selftest_selchain(count);
    }
    let (mask, val): (u32, u32) = match class {
        "dpimm" => (0x1C00_0000, 0x1000_0000),
        "dpreg" => (0x0E00_0000, 0x0A00_0000),
        "ldst" => (0x0A00_0000, 0x0800_0000),
        "branch" => (0x1C00_0000, 0x1400_0000),
        other => {
            let p: Vec<&str> = other.split(':').collect();
            (u32::from_str_radix(p[0], 16).unwrap(), u32::from_str_radix(p[1], 16).unwrap())
        }
    };
    let is_mem = class == "ldst" || std::env::var_os("ATTESOR_SELFTEST_MEM").is_some();
    // The instruction under test plus an svc terminator live at pc. PC-relative
    // literal loads (`ldr ..., $±imm19<<2`) can reach ±1MB from pc, so centre pc
    // in a 4MB window: every literal target stays mapped (zeroed) and the result
    // no longer depends on whatever happens to be mapped around a 4K page
    // (unmapped -> SIGSEGV on some hosts, garbage -> spurious mismatches).
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x400000, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    let pc = page + 0x200000;
    let mut buf = vec![0u8; 1 << 20];
    let bufp = buf.as_mut_ptr() as u64;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut ok, mut bad, mut skipped) = (0, 0, 0);
    let mut native = 0u64;
    for _ in 0..count {
        let mut insn = (rng.next() as u32 & !mask) | val;
        let branch = class == "branch";
        if emit::is_terminator(insn) != branch || (branch && insn & 0xFF00_0000 == 0xD400_0000) {
            skipped += 1;
            continue;
        }
        if is_mem && (insn >> 5) & 31 == 31 {
            insn &= !(31 << 5);
        }
        // Reference first (it tells us whether the encoding is valid).
        let mut init = Cpu::new();
        for i in 0..31 {
            init.x[i] = if is_mem { bufp + 0x40000 + (rng.next() % 0x60000 & !15) } else { rng.val() };
        }
        init.x[31] = bufp + 0x80000;
        if is_mem && insn & 0x3B20_0C00 == 0x3820_0800 {
            // register offset: keep the index small so the address stays in the buffer
            let rm = ((insn >> 16) & 31) as usize;
            let rn = ((insn >> 5) & 31) as usize;
            if rm != rn && rm < 31 {
                init.x[rm] = rng.next() % 512;
            } else {
                skipped += 1;
                continue;
            }
        }
        // SIMD structure loads/stores with register post-increment
        if is_mem && insn & 0xBF00_0000 == 0x0C00_0000 && std::env::var_os("ATTESOR_SELFTEST_SIMDMEM").is_none() {
            skipped += 1;
            continue;
        }
        init.set_nzcv((rng.next() % 16) as u32);
        for i in 0..32 {
            init.v[i] = [rng.val(), rng.val()];
        }
        // Opt-in FPCR rounding coverage for instructions that honor it. Keep
        // the general arithmetic suite at its documented default FPCR.
        if std::env::var_os("ATTESOR_SELFTEST_FPCR").is_some() {
            // rounding mode and flush-to-zero; the host MXCSR follows FPCR as a guest MSR would set it
            init.fpcr = ((rng.next() & 3) << 22) | ((rng.next() & 1) << 24);
        }
        interp::sys_sync_host_fp_mode(init.fpcr);
        init.pc = pc;
        let mem0: Vec<u8> = buf.clone();
        let mut r = Cpu::new();
        *r = clone_cpu(&init);
        match interp::exec(&mut r, insn) {
            Ok(()) => {}
            Err(_) => {
                skipped += 1;
                continue;
            }
        }
        let mem_ref = buf.clone();
        buf.copy_from_slice(&mem0);
        // JIT: single instruction followed by SVC (block terminator).
        unsafe {
            *(pc as *mut u32) = insn;
            *((pc + 4) as *mut u32) = 0xD400_1001; // svc #0x80
        }
        invalidate_range(pc, 8);
        if std::env::var_os("SELFTEST_VERBOSE").is_some() {
            eprintln!("test {:08x} {}", insn, disasm(insn));
        }
        let mut j = Cpu::new();
        *j = clone_cpu(&init);
        setup_cpu(&mut j);
        let fb0 = STATS_FALLBACK.load(Ordering::Relaxed);
        let host = lookup_or_translate(pc);
        if STATS_FALLBACK.load(Ordering::Relaxed) == fb0 {
            native += 1;
        }
        unsafe { attesor_jit_enter(&mut *j, host) };
        if j.exit_reason == super::emit::EXIT_INTERP {
            // non-default FPCR: the block defers to the interpreter, as the dispatcher would
            let _ = interp::exec(&mut j, insn);
            // ...and the svc #0x80 terminator after it exits as the JIT's would
            j.pc += 4;
            j.exit_reason = EXIT_SVC;
            j.exit_data = 0x80;
            native = native.saturating_sub(1);
        }
        let pending = PENDING.with(|p| p.take());
        if j.exit_reason == EXIT_PENDING || pending.is_some() {
            skipped += 1;
            buf.copy_from_slice(&mem0);
            continue;
        }
        let mut a = snapshot(&r);
        let mut b = snapshot(&j);
        if branch {
            a.push(r.pc);
            b.push(j.pc);
        }
        let mem_diff = buf != mem_ref;
        if a != b || mem_diff || (!branch && j.exit_reason != EXIT_SVC) {
            bad += 1;
            if bad <= 40 {
                println!("MISMATCH {:08x} {}", insn, disasm(insn));
                for i in 0..a.len() {
                    if a[i] != b[i] {
                        let name = if i < 32 { format!("x{i}") } else if i == 32 { "nzcv".into() } else if i < 97 { format!("v{}.{}", (i - 33) / 2, (i - 33) % 2) } else if i == 97 { "tpidr".into() } else { "pc".into() };
                        println!("    {name}: interp={:#x} jit={:#x}", a[i], b[i]);
                    }
                }
                if mem_diff {
                    println!("    memory differs");
                }
                if j.exit_reason != EXIT_SVC {
                    println!("    exit reason {}", j.exit_reason);
                }
            }
        } else {
            ok += 1;
        }
        buf.copy_from_slice(&mem0);
    }
    println!("jit selftest [{class}]: ok={ok} mismatch={bad} skipped={skipped} native={native}");
}

fn clone_cpu(c: &Cpu) -> Cpu {
    let mut n = Cpu::default();
    n.x = c.x;
    n.pc = c.pc;
    n.set_nzcv(c.nzcv());
    n.v = c.v;
    n.fpcr = c.fpcr;
    n.tpidr_el0 = c.tpidr_el0;
    n.tpidrro_el0 = c.tpidrro_el0;
    n
}


/// `attesor --jit-dump <hex insn>...`: translate the instructions as one block and print the
/// generated x86 code as hex (disassemble with objdump on a .byte file).
pub fn dump_block(words: &[u32]) {
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    for (i, w) in words.iter().enumerate() {
        unsafe { *((page + 4 * i as u64) as *mut u32) = *w };
    }
    unsafe { *((page + 4 * words.len() as u64) as *mut u32) = 0xD400_1001 };
    let host = translate(page);
    let bytes = unsafe { std::slice::from_raw_parts(host as *const u8, 512) };
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    println!("{hex}");
}
