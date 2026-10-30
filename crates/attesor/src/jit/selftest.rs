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
fn run_selftest_fuse(count: usize, deep: bool, seed: Option<u64>) {
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
    let mut rng = Rng(seed.unwrap_or(0x1234_5678_9abc_def1));
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
        init.set_pc(page);
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
            r.set_pc(page);
            loop {
                let insn = unsafe { *(r.pc() as *const u32) };
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
            r.set_pc(page + 4);
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
            let h2 = lookup_or_translate(j.pc());
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
            a.push(r.pc());
            b.push(j.pc());
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
    assert_eq!(bad, 0, "fusion differential mismatches");
}

/// Flag setter feeding a chain of 1-3 selects, then either a B.cond over a writer or straight-line code
/// up to a writer: the shape where the setter's guest flags are never stored (flags_dead_after_selects).
fn run_selftest_selchain(count: usize, seed: Option<u64>) {
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
    let mut rng = Rng(seed.unwrap_or(0x5e1c_4a11_0bad_cafe));
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
        init.set_pc(page);
        for (i, w) in prog.iter().enumerate() {
            unsafe { *((page + 4 * i as u64) as *mut u32) = *w };
        }
        let mut r = Cpu::new();
        *r = clone_cpu(&init);
        let mut steps = 0;
        let mut bad_ref = false;
        loop {
            let insn = unsafe { *(r.pc() as *const u32) };
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
            let h2 = lookup_or_translate(j.pc());
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
    assert_eq!(bad, 0, "select-chain differential mismatches");
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

fn run_selftest_blocks(count: usize, fp: bool, seed: Option<u64>) {
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
    let mut rng = Rng(seed.unwrap_or(0x0dd_ba11_f00d_cafe));
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
        init.set_pc(page);
        let mem0: Vec<u8> = buf.clone();
        let mut r = Cpu::new();
        *r = clone_cpu(&init);
        let mut valid = true;
        for (i, w) in prog.iter().enumerate() {
            r.set_pc(page + 4 * i as u64);
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
    assert_eq!(bad, 0, "block differential mismatches");
}

/// Keep the loop counters (x24, x28), branch scratch
/// (x27), and memory bases (x20-x23) out of the data pool. Writeback is bounded
/// by short bodies * 50 iterations, or maximum-length bodies * 3 iterations,
/// inside the private buffer. x29 supplies a bounded extended register offset.
fn loop_insn(rng: &mut Rng, pool: &[u32], wide: bool, i: usize, index_option: u32) -> u32 {
    let pick = |rng: &mut Rng| pool[rng.next() as usize % pool.len()];
    let mut d = pick(rng);
    // Wide bodies read every member of the 12-16 register pool, rather than
    // merely sampling a large pool and accidentally fitting in the host budget.
    let mut n = if wide { pool[i % pool.len()] } else { pick(rng) };
    let mut m = if wide { pool[(i + 2) % pool.len()] } else { pick(rng) };
    let sf = (rng.next() as u32 & 1) << 31;
    let width = if sf != 0 { 64 } else { 32 };
    let kind = if wide && rng.next() % 4 != 0 { rng.next() % 5 } else { rng.next() % 17 };
    if matches!(kind, 0..=7 | 14..=16) {
        match rng.next() % 8 {
            0 => d = n,
            1 => d = m,
            2 => n = 31,
            3 => m = 31,
            4 => d = 31,
            _ => {}
        }
    }
    match kind {
        0 => { // ADD(S)/SUB(S), shifted register
            let op = (rng.next() as u32 & 3) << 29;
            sf | 0x0b00_0000 | op | ((rng.next() as u32 % 3) << 22)
                | (m << 16) | ((rng.next() as u32 % width) << 10) | (n << 5) | d
        }
        1 => { // AND/ORR/EOR/ANDS and inverted variants
            sf | 0x0a00_0000 | ((rng.next() as u32 & 3) << 29)
                | ((rng.next() as u32 & 3) << 22) | ((rng.next() as u32 & 1) << 21)
                | (m << 16) | ((rng.next() as u32 % width) << 10) | (n << 5) | d
        }
        2 => { // CSEL/CSINC/CSINV/CSNEG
            sf | 0x1a80_0000 | ((rng.next() as u32 & 1) << 30)
                | ((rng.next() as u32 & 1) << 10) | (m << 16)
                | ((rng.next() as u32 % 14) << 12) | (n << 5) | d
        }
        3 => { // MADD/MSUB
            sf | 0x1b00_0000 | (m << 16) | ((rng.next() as u32 & 1) << 15)
                | (pick(rng) << 10) | (n << 5) | d
        }
        4 => { // LSLV/LSRV/ASRV/RORV
            sf | 0x1ac0_2000 | (m << 16) | ((rng.next() as u32 & 3) << 10) | (n << 5) | d
        }
        5 => { // ADD(S)/SUB(S), immediate
            sf | 0x1100_0000 | ((rng.next() as u32 & 3) << 29)
                | ((rng.next() as u32 & 0xfff) << 10) | (n << 5) | d
        }
        6 => { // logical immediate, rotated eight-bit mask
            sf | 0x1200_1c00 | ((rng.next() as u32 & 3) << 29)
                | (if sf != 0 { 1 << 22 } else { 0 })
                | ((rng.next() as u32 % width) << 16) | (n << 5) | d
        }
        7 => { // MOVZ/MOVN/MOVK (including destructive cached destinations)
            let op = [0, 2, 3][rng.next() as usize % 3];
            sf | 0x1280_0000 | (op << 29)
                | ((rng.next() as u32 % (width / 16)) << 21)
                | ((rng.next() as u32 & 0xffff) << 5) | d
        }
        14 => { // ADD(S)/SUB(S), every extended-register form, including W compares
            let flags = rng.next() as u32 & 1;
            sf | 0x0b20_0000 | ((rng.next() as u32 & 1) << 30) | (flags << 29)
                | (m << 16) | ((rng.next() as u32 & 7) << 13)
                | ((rng.next() as u32 % 5) << 10) | (n << 5)
                | (if flags != 0 && rng.next() & 1 == 0 { 31 } else { d })
        }
        15 => { // bitfields, including destructive BFM and identity W writes
            let (r,s) = if rng.next() % 4 == 0 { (0,width-1) }
                else { (rng.next() as u32 % width,rng.next() as u32 % width) };
            sf | 0x1300_0000 | ((rng.next() as u32 % 3) << 29)
                | (if sf != 0 {1 << 22} else {0}) | (r << 16) | (s << 10) | (n << 5) | d
        }
        16 => sf | 0x1380_0000 | (if sf != 0 {1 << 22} else {0})
            | (m << 16) | ((rng.next() as u32 % width) << 10) | (n << 5) | d,
        10 => { // LDP/STP, signed offset and pre/post-index, W/X and SIMD Q
            let vector = rng.next() % 4 == 0;
            let size = if vector { 2 } else { rng.next() as u32 & 1 };
            let t2 = pool[(pool.iter().position(|&r| r == d).unwrap() + 1) % pool.len()];
            let mode = [1, 2, 3][rng.next() as usize % 3];
            let offset = [0u32, 1, 0x7f][rng.next() as usize % 3];
            (if vector { 0xad00_0000 } else if size == 0 { 0x2900_0000 } else { 0xa900_0000 })
                & !(3 << 23) | (mode << 23) | ((rng.next() as u32 & 1) << 22)
                | (offset << 15) | (t2 << 10) | ((20 + rng.next() as u32 % 4) << 5) | d
        }
        11 => { // LDR/STR register offset, x29 remains a small private-buffer index
            let size = rng.next() as u32 % 4;
            match rng.next() % 4 {
                0 => {
                    // SIMD Q has a 16-byte scaled index (not an x86 SIB scale).
                    return 0x3ca0_0800 | ((rng.next() as u32 & 1) << 22)
                        | (29 << 16) | (index_option << 13)
                        | ((rng.next() as u32 & 1) << 12)
                        | ((20 + rng.next() as u32 % 4) << 5) | d;
                }
                1 => {
                    // SIMD&FP B/H/S/D (scales 1..8)
                    return (size << 30) | 0x3c20_0800 | ((rng.next() as u32 & 1) << 22)
                        | (29 << 16) | (index_option << 13)
                        | ((rng.next() as u32 & 1) << 12)
                        | ((20 + rng.next() as u32 % 4) << 5) | d;
                }
                _ => {}
            }
            let opc = if size == 3 { rng.next() as u32 & 1 }
                else if size == 2 { rng.next() as u32 % 3 } else { rng.next() as u32 & 3 };
            (size << 30) | 0x3820_0800 | (opc << 22)
                | (index_option << 13)
                | (29 << 16) | ((rng.next() as u32 & 1) << 12)
                | ((20 + rng.next() as u32 % 4) << 5) | d
        }
        12 => fp_insn(rng),
        13 => { // SIMD Boolean or SHA256H/H2 (the latter excludes the region)
            let template = [0x4e20_1c00, 0x6e20_1c00, 0x5e00_4000, 0x5e00_5000][rng.next() as usize % 4];
            template | (m << 16) | (n << 5) | d
        }
        _ => {
            let size = rng.next() as u32 % 4;
            let base = 20 + rng.next() as u32 % 4;
            let opc = if size == 3 { rng.next() as u32 & 1 }
                else if size == 2 { rng.next() as u32 % 3 } else { rng.next() as u32 & 3 };
            let load = opc << 22;
            match rng.next() % 3 {
                0 => (size << 30) | 0x3900_0000 | load
                    | ((rng.next() as u32 % 32) << 10) | (base << 5) | d,
                mode => { // signed pre/post-index; base never aliases the data register
                    let offset = if rng.next() & 1 == 0 { 1i32 << size } else { -(1i32 << size) };
                    (size << 30) | 0x3800_0000 | load | (((offset as u32) & 0x1ff) << 12)
                        | (if mode == 1 { 0x400 } else { 0xc00 }) | (base << 5) | d
                }
            }
        }
    }
}

/// Bounded single/multi-block loops: diamonds, side exits in either arm,
/// helpers in one arm, flag-carrying joins, multiple latches and nested reentry.
/// The same random memory/FP/alias/pressure operands run on the new path.
fn run_selftest_loops(count: usize, seed: u64) {
    assert_ne!(seed, 0, "xorshift seed must be nonzero");
    let counting = COUNT_LOOP_REGIONS.swap(true, Ordering::Relaxed);
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0)
    };
    assert_ne!(page, libc::MAP_FAILED, "selftest loop code allocation");
    let page = page as u64;
    let mut buf = vec![0u8; 32768];
    let bufp = buf.as_mut_ptr() as u64;
    let mut rng = Rng(seed);
    let (mut ok, mut bad, mut skipped, mut native) = (0u64, 0u64, 0u64, 0u64);
    let (mut max_blocks, mut nested_entries, mut aliases, mut q_scale16, mut negative_sxtw) = (0u64,0u64,0u64,0u64,0u64);
    let regions0 = STATS_LOOP_REGIONS.load(Ordering::Relaxed);
    let multi0 = STATS_MULTI_LOOP_REGIONS.load(Ordering::Relaxed);
    let mut multi_cases = 0;
    let mut guest_calls = 0;
    for it in 0..count {
        let wide = (it / 8) % 4 == 0;
        let pool_len = if wide { 12 + rng.next() as usize % 5 } else { 3 + rng.next() as usize % 4 };
        let mut pool: Vec<u32> = (0..20).chain(25..27).chain([30]).collect();
        for i in (1..pool.len()).rev() { pool.swap(i, rng.next() as usize % (i + 1)); }
        pool.truncate(pool_len);
        let exit_kind = it % 8;
        let branch_kind = it % 3;
        let max_body = it % 32 == 0;
        let nested = it % 16 == 1;
        let shape = (it / 8) % 8;
        let len = if max_body { emit::MAX_BLOCK_INSNS - if branch_kind == 2 {3} else {2} }
            else if wide { pool_len - 2 } else { 2 + rng.next() as usize % 13 };
        let iterations = 1 + rng.next() % if max_body || nested {3} else {50};
        let outer = if nested { 2 + rng.next() % 15 } else {1};
        let index_option = [2,3,6,7][it % 4];
        let mut prog = Vec::new();
        let mut side_branches = Vec::new();
        let mut guest_call = None;
        if !max_body && shape != 0 {
            multi_cases += 1;
            // Both arms contain random operand forms. Even/odd trip counts
            // exercise both arms repeatedly, with a single fixed assignment.
            if shape == 7 {
                prog.push(0xd100_0000 | ((1 + rng.next() % iterations) as u32) << 10 | (28 << 5) | 27);
                side_branches.push(prog.len()); prog.push(0);
            }
            let split = prog.len(); prog.push(0); // TBZ counter -> second arm
            for i in 0..3 { prog.push(loop_insn(&mut rng, &pool, wide, i, index_option)); }
            if shape == 2 {
                if it & 1 != 0 {
                    guest_call = Some(prog.len()); prog.push(0); // BL callee, patched below
                    guest_calls += 1;
                } else {
                    prog.push(0x1ac0_4000 | (pool[1] << 16) | (pool[0] << 5) | pool[0]);
                }
            }
            if shape == 3 || shape == 6 {
                prog.push(0xd100_0000 | ((1 + rng.next() % iterations) as u32) << 10 | (28 << 5) | 27);
                side_branches.push(prog.len()); prog.push(0);
            }
            let join = prog.len(); prog.push(0); // B join
            let arm = prog.len();
            for i in 0..3 { prog.push(loop_insn(&mut rng, &pool, wide, i+3, index_option)); }
            if shape == 3 || shape == 7 {
                prog.push(0xd100_0000 | ((1 + rng.next() % iterations) as u32) << 10 | (28 << 5) | 27);
                side_branches.push(prog.len()); prog.push(0);
            }
            prog[split] = 0x3600_001c | (((arm - split) as u32) << 5);
            prog[join] = 0x1400_0000 | ((prog.len() - join) as u32);
            if shape == 4 {
                // NZCV survives a non-flag-setting branch and an internal join.
                prog.push(0xf100_071f); // cmp x24,#1
                prog.push(0x3600_005c); // tbz w28,#0,+8
                prog.push(0xd503_201f); // nop
                prog.push(0x9a9f_07fa); // cset x26,ne
            }
        }
        for i in 0..len {
            prog.push(loop_insn(&mut rng, &pool, wide, i, index_option));
            if !max_body && i == 0 && it % 16 < 4 {
                // Alias the load destination with its base or extended index,
                // then restore the pointer/index before the next random access.
                let alias = if it / 16 % 2 == 0 { 20 } else { 29 };
                prog.push(0x9100_0000 | (alias << 5) | 27); // mov x27,xAlias
                prog.push(0xb860_0800 | (29 << 16) | (index_option << 13) | (20 << 5) | alias);
                prog.push(0x9100_0000 | (27 << 5) | alias); // mov xAlias,x27
                aliases += 1;
            }
            if i == len / 2 {
                match exit_kind {
                    4 | 5 => {
                        // A delayed taken exit, or a side exit never taken. x28 is
                        // still positive here, so subtracting zero never takes it.
                        let at = if exit_kind == 4 { 1 + rng.next() % iterations } else { 0 };
                        prog.push(0xd100_0000 | ((at as u32) << 10) | (28 << 5) | 27);
                        side_branches.push(prog.len());
                        prog.push(0); // CBZ x27, side_svc (patched below)
                    }
                    6 => {
                        // CRC32B wD,wD,wM: valid in interp, not native in dp2.
                        // Reuse the written register after the helper to test cache invalidation.
                        prog.push(0x1ac0_4000 | (pool[1] << 16) | (pool[0] << 5) | pool[0]);
                        prog.push(0x9100_0400 | (pool[0] << 5) | pool[0]);
                    }
                    7 => prog.push(0xd400_1041), // SVC #0x82 inside the body
                    _ => {}
                }
            }
        }
        let sf = if rng.next() & 1 == 0 { 0 } else { 1 << 31 };
        // SUBS + B.NE, SUB + CBNZ, or SUB + NEG + TBNZ of the sign
        // bit. NEG(counter) has its sign bit set exactly while counter > 0.
        prog.push(sf | 0x5100_0400 | (if branch_kind == 0 { 1 << 29 } else { 0 }) | (28 << 5) | 28);
        if branch_kind == 2 { prog.push(sf | 0x4b00_0000 | (28 << 16) | (31 << 5) | 27); }
        let latch = |at: usize| {
            let back = -(at as i32) as u32;
            match branch_kind {
                0 => 0x5400_0001 | ((back & 0x7ffff) << 5),
                1 => sf | 0x3500_0000 | ((back & 0x7ffff) << 5) | 28,
                _ => sf | 0x3700_0000 | (31 << 19) | ((back & 0x3fff) << 5) | 27,
            }
        };
        if !max_body && shape == 5 {
            // Alternate between two genuinely executed backedges. All inserted
            // branches preserve NZCV produced by SUBS on the flag-based form.
            prog.push(0x3600_007c); // tbz w28,#0,+12
            prog.push(latch(prog.len()));
            prog.push(0x1400_0002); // b exit
            prog.push(latch(prog.len()));
        } else { prog.push(latch(prog.len())); }
        if max_body { assert_eq!(prog.len(),emit::MAX_BLOCK_INSNS); max_blocks += 1; }
        if nested { nested_entries += outer; }
        if index_option == 6 { negative_sxtw += 1; }
        q_scale16 += prog.iter().filter(|&&w| w & 0xbfa0_1c00 == 0x3ca0_1800
            && matches!((w >> 13) & 7,2|3)).count() as u64;
        // Read the exit flags before any successor overwrites them, in W and X forms.
        prog.push(0x9a9f_07fa); // cset x26,ne
        prog.push(0x1a9f_17f9); // cset w25,eq
        if nested {
            // Re-enter the same inner region many times with short trip counts.
            prog.push(0xd100_0718); // sub x24,x24,#1 (preserve exit NZCV)
            prog.push(0xb400_0078); // cbz x24,+12 (SVC)
            prog.push(0xd280_001c | ((iterations as u32) << 5)); // movz x28,#iterations
            let back = -(prog.len() as i32) as u32;
            prog.push(0x1400_0000 | (back & 0x3ff_ffff));
        }
        prog.push(0xd400_1001); // normal exit: SVC #0x80
        for at in side_branches {
            let offset = (prog.len() - at) as u32;
            prog[at] = 0xb400_0000 | (offset << 5) | 27;
        }
        prog.push(0xd400_1021); // side exit: SVC #0x81
        if let Some(at) = guest_call {
            prog[at] = 0x9400_0000 | (prog.len()-at) as u32;
            prog.push(0x1ac0_4000 | (pool[1] << 16) | (27 << 5) | 27); // CRC32B w27,w27,wM
            prog.push(0xd65f_03c0); // RET: x30 was set by BL
        }
        unsafe {
            for (i, &w) in prog.iter().enumerate() { *((page + i as u64 * 4) as *mut u32) = w; }
        }
        invalidate_range(page, prog.len() as u64 * 4);
        for chunk in buf.chunks_exact_mut(8) { chunk.copy_from_slice(&rng.next().to_le_bytes()); }
        let mem0 = buf.clone();
        let mut init = Cpu::new();
        for r in 0..32 {
            init.x[r] = rng.val() | 0xa5a5_0000_0000_0000;
            init.v[r] = [fp_val(&mut rng), fp_val(&mut rng)];
        }
        // Store pointer-shaped payloads as well as using tagged access bases.
        if it % 4 == 3 { init.x[pool[0] as usize] = bufp | 0xcd00_0000_0000_0000; }
        init.x[29] = match index_option {
            2 => 0xfeed_beef_0000_0000 | (rng.next() % 16),
            6 => 0xfeed_beef_0000_0000 | ((-(1 + (rng.next() % 16) as i32)) as u32 as u64),
            _ => rng.next() % 16,
        };
        for r in 20..24 {
            init.x[r] = (bufp + 16384 + (r as u64 - 20) * 256)
                | (if it & 1 != 0 { 0xab00_0000_0000_0000 } else { 0 });
        }
        init.x[28] = iterations;
        init.x[24] = outer;
        init.tpidr_el0 = rng.val();
        init.set_nzcv((rng.next() & 15) as u32);
        init.set_pc(page);
        let mut reference = clone_cpu(&init);
        let limit = (prog.len() + 1) * (iterations as usize + 1) * (outer as usize + 1);
        let mut svc = None;
        let mut helper_probe_executed = false;
        for _ in 0..limit {
            let idx = reference.pc().wrapping_sub(page) / 4;
            let Some(&w) = prog.get(idx as usize) else { break };
            helper_probe_executed |= w & 0xffe0_fc00 == 0x1ac0_4000;
            match interp::exec(&mut reference, w) {
                Ok(()) => {}
                Err(Exit::Svc(imm)) => { svc = Some(imm); break; }
                Err(_) => break,
            }
        }
        let expected_memory = buf.clone();
        buf.copy_from_slice(&mem0);
        let Some(svc) = svc else { skipped += 1; continue };
        let mut translated = clone_cpu(&init);
        setup_cpu(&mut translated);
        let fb0 = STATS_FALLBACK.load(Ordering::Relaxed);
        let mut pending = None;
        // Dispatch until SVC, not just one host entry: side exits and fallback
        // continuations may leave the unit. Bound public entries as well.
        for _ in 0..limit {
            if translated.pc() < page || translated.pc() >= page + prog.len() as u64 * 4 { break; }
            let host = lookup_or_translate(translated.pc());
            unsafe { attesor_jit_enter(&mut translated, host); }
            pending = PENDING.with(|p| p.take());
            if translated.exit_reason != EXIT_CONTINUE || pending.is_some() { break; }
        }
        let fallbacks = STATS_FALLBACK.load(Ordering::Relaxed) - fb0;
        if fallbacks == 0 { native += 1; }
        // Keep the fallback probe honest if native instruction coverage changes.
        assert!(!helper_probe_executed || fallbacks != 0, "loop fallback probe became native");
        let (a, b) = (snapshot(&reference), snapshot(&translated));
        let mem_diff = buf != expected_memory;
        if a != b || mem_diff || reference.pc() != translated.pc()
            || translated.exit_reason != EXIT_SVC || translated.exit_data != svc as u64 || pending.is_some() {
            bad += 1;
            if bad <= 15 {
                println!("MISMATCH loop #{it}: seed={seed:#x} iterations={iterations} pool={pool_len} exit={exit_kind} backedge={branch_kind}");
                println!("    initial x={:x?} nzcv={:x}", init.x, init.nzcv());
                for (i, &w) in prog.iter().enumerate() { println!("    +{:03x} {w:08x} {}", i * 4, disasm(w)); }
                for i in 0..a.len() {
                    if a[i] != b[i] { println!("    slot {i}: interp={:#x} jit={:#x}", a[i], b[i]); }
                }
                if let Some(i) = buf.iter().zip(&expected_memory).position(|(a, b)| a != b) {
                    println!("    memory[{i:#x}]: interp={:#x} jit={:#x}", expected_memory[i], buf[i]);
                }
                println!("    pc: interp={:#x} jit={:#x}; exit={} data={:#x} expected_svc={svc:#x} pending={pending:?}",
                    reference.pc(), translated.pc(), translated.exit_reason, translated.exit_data);
            }
        } else { ok += 1; }
    }
    invalidate_range(page, 0x1000);
    unsafe { libc::munmap(page as *mut libc::c_void, 0x1000); }
    COUNT_LOOP_REGIONS.store(counting, Ordering::Relaxed);
    let loop_regions = STATS_LOOP_REGIONS.load(Ordering::Relaxed) - regions0;
    let multi_regions = STATS_MULTI_LOOP_REGIONS.load(Ordering::Relaxed) - multi0;
    println!("jit selftest [loops]: ok={ok} mismatch={bad} skipped={skipped} native={native} loop_regions={loop_regions} seed={seed:#x} max_blocks={max_blocks} nested_entries={nested_entries} aliases={aliases} q_scale16={q_scale16} negative_sxtw={negative_sxtw} multi_cases={multi_cases} multi_regions={multi_regions} guest_calls={guest_calls}");
    assert_eq!(bad, 0, "loop differential mismatches");
}

/// Exhaustive operand-form cycle: random encodings almost never combine region
/// entry, SIMD Q x16 addressing, poisoned UXTW upper bits and aliased outputs.
/// Check all architectural state and the entire memory image, not just a checksum.
fn run_selftest_addrforms(count: usize, seed: u64) {
    assert_ne!(seed, 0);
    let page = unsafe { libc::mmap(std::ptr::null_mut(), 4096,
        libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
    assert_ne!(page, libc::MAP_FAILED);
    let pc = page as u64;
    let mut memory = vec![0u8; 8192];
    let mut rng = Rng(seed);
    // (SIMD, size, opc): integer signed/unsigned loads and stores, B/H/S/D/Q.
    let mut forms = Vec::new();
    for size in 0..4u32 {
        for opc in 0..4u32 {
            if (opc == 2 && size == 3) || (opc == 3 && size >= 2) { continue; }
            forms.push((0, size, opc));
        }
        forms.push((1, size, 0)); forms.push((1, size, 1));
    }
    forms.extend([(1, 0, 2), (1, 0, 3)]);
    let combinations = forms.len() * 4 * 2 * 2 * 2 * 5;
    let (mut ok, mut native, mut q16) = (0usize, 0usize, 0usize);
    for it in 0..count {
        let mut n = it % combinations;
        let (v, size, opc) = forms[n % forms.len()]; n /= forms.len();
        let option = [2, 3, 6, 7][n % 4]; n /= 4;
        let scaled = (n % 2) as u32; n /= 2;
        let rn = if n % 2 == 0 { 0 } else { 31 }; n /= 2;
        let rm = if n % 2 == 0 { 1 } else { 31 }; n /= 2;
        let rt = [0, 1, 2, 30, 31][n % 5];
        let access = 0x3820_0800 | (size << 30) | (v << 26) | (opc << 22)
            | (rm << 16) | (option << 13) | (scaled << 12) | (rn << 5) | rt;
        let words = [access, 0x9100_0000 | (21 << 5) | rn,
            0x9100_0000 | (22 << 5) | 1, 0xf100_079c, 0x54ff_ff81, 0xd400_1001];
        unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), page as *mut u32, words.len()); }
        invalidate_range(pc, 4096);
        for chunk in memory.chunks_exact_mut(8) { chunk.copy_from_slice(&rng.next().to_le_bytes()); }
        let before = memory.clone();
        let mut init = Cpu::new();
        for r in 0..32 { init.x[r] = rng.val(); init.v[r] = [rng.next(), rng.next()]; }
        let base = (memory.as_ptr() as u64 + 4096) | if it & 1 == 0 {0} else {0xab00_0000_0000_0000};
        let index = match option {
            2 => 0xdead_beef_0000_0000 | (1 + rng.next() % 31),
            6 => 0xdead_beef_0000_0000 | ((-(1 + (rng.next() % 31) as i32)) as u32 as u64),
            7 => (-(1 + (rng.next() % 31) as i64)) as u64,
            _ => 1 + rng.next() % 31,
        };
        init.x[rn as usize] = base; init.x[1] = index;
        init.x[21] = base; init.x[22] = index; init.x[28] = 3;
        init.tpidr_el0 = rng.next(); init.tpidrro_el0 = rng.next();
        init.fpsr = rng.next() & 0x0800_009f;
        init.set_nzcv((rng.next() & 15) as u32); init.set_pc(pc);
        let mut reference = clone_cpu(&init);
        let mut expected_svc = None;
        for _ in 0..32 {
            let w = unsafe { *(reference.pc() as *const u32) };
            match interp::exec(&mut reference, w) {
                Ok(()) => {}, Err(Exit::Svc(imm)) => { expected_svc = Some(imm); break; },
                other => panic!("addrforms reference rejected {access:08x}: {other:?}"),
            }
        }
        assert_eq!(expected_svc, Some(0x80));
        let expected_memory = memory.clone(); memory.copy_from_slice(&before);
        let mut actual = clone_cpu(&init); setup_cpu(&mut actual);
        let fb0 = STATS_FALLBACK.load(Ordering::Relaxed);
        for _ in 0..32 {
            let host = lookup_or_translate(actual.pc());
            unsafe { attesor_jit_enter(&mut actual, host); }
            if actual.exit_reason != EXIT_CONTINUE { break; }
        }
        assert!(PENDING.with(|p| p.take()).is_none(), "addrforms pending {access:08x}");
        assert_eq!(snapshot(&reference), snapshot(&actual), "addrforms state case={it} word={access:08x}");
        assert_eq!(reference.pc(), actual.pc(), "addrforms PC {access:08x}");
        assert_eq!((reference.fpcr, reference.fpsr, reference.tpidrro_el0),
            (actual.fpcr, actual.fpsr, actual.tpidrro_el0), "addrforms system state {access:08x}");
        assert_eq!((reference.excl_addr, reference.excl_val, reference.excl_val2, reference.excl_size),
            (actual.excl_addr, actual.excl_val, actual.excl_val2, actual.excl_size));
        assert_eq!(actual.exit_reason, EXIT_SVC); assert_eq!(actual.exit_data, 0x80);
        assert!(memory == expected_memory, "addrforms memory case={it} word={access:08x}");
        if STATS_FALLBACK.load(Ordering::Relaxed) == fb0 { native += 1; }
        if v == 1 && opc >= 2 && scaled == 1 { q16 += 1; }
        ok += 1;
    }
    invalidate_range(pc, 4096);
    unsafe { libc::munmap(page, 4096); }
    println!("jit selftest [addrforms]: ok={ok} mismatch=0 skipped=0 native={native} combinations={combinations} q_scale16={q16} seed={seed:#x}");
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
        init.set_pc(page);
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
    assert_eq!(bad, 0, "FP NaN differential mismatches");
}

/// Memory-block test with tagged pointers, literal scratch writes and a
/// deliberately hostile (but ABI-conforming) fallback helper. r11 is caller-saved.
extern "C" fn tbi_clobbering_helper(cpu: *mut Cpu, insn: u32, pc: u64) -> u64 {
    let status = helper_interp(cpu, insn, pc);
    unsafe { std::arch::asm!("mov r11, 0xabababababababab", out("r11") _, options(nomem, nostack, preserves_flags)); }
    status
}

fn run_selftest_tbiblocks(count: usize, seed: Option<u64>) {
    let page = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x1000, libc::PROT_READ | libc::PROT_WRITE,
                   libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64
    };
    let mut memory = vec![0u8; 8192];
    let addr = memory.as_mut_ptr() as u64;
    let mut rng = Rng(seed.unwrap_or(0x7142_19a5_0022_8bb1));
    let (mut ok, mut bad, mut native, mut fallbacks) = (0u64, 0u64, 0u64, 0u64);
    for it in 0..count {
        for chunk in memory.chunks_exact_mut(8) { chunk.copy_from_slice(&rng.next().to_le_bytes()); }
        let prog = [
            0xf940_0280, // ldr x0, [x20]
            0xf940_06a1, // ldr x1, [x21, #8] (cached mask)
            0xd2e0_0006 | (((0x1234 + it as u32) & 0xffff) << 5), // movz x6, #imm, lsl #48: r11 scratch
            0xf840_86c2, // ldr x2, [x22], #8 (must reload mask after scratch)
            0xf940_06c3, // ldr x3, [x22, #8]
            0x1ee0_c020, // fabs h0, h1: valid interpreted half instruction
            0xf940_0a84, // ldr x4, [x20, #16] (must reload after helper)
            0x9ac1_0808, // udiv x8, x0, x1: internal conditional paths
            0xf900_0be3, // str x3, [sp, #16]
            0xa902_16e4, // stp x4, x5, [x23, #32]
            0xa942_16e4, // ldp x4, x5, [x23, #32]
            0x4c40_7282, // ld1 {v2.16b}, [x20]
            0x4c00_72a2, // st1 {v2.16b}, [x21]
            0x3940_3289, // ldrb w9, [x20, #12]
            0x7940_1aaa, // ldrh w10, [x21, #12]
            0xb940_0ecb, // ldr w11, [x22, #12]
        ];
        let mut init = Cpu::new();
        init.set_pc(page);
        init.set_nzcv((rng.next() & 15) as u32);
        for r in 0..31 { init.x[r] = rng.val(); }
        for r in 0..32 { init.v[r] = [rng.next(), rng.next()]; }
        for r in 20..24 {
            let tag = ((it + r) & 255) as u64;
            init.x[r] = (addr + ((r - 20) * 1024 + (rng.next() as usize & 127) * 4) as u64) | (tag << 56);
        }
        init.x[31] = (addr + 6144) | (((it & 255) as u64) << 56);
        let before = memory.clone();
        let mut reference = clone_cpu(&init);
        for (i, insn) in prog.iter().enumerate() {
            reference.set_pc(page + i as u64 * 4);
            interp::exec(&mut reference, *insn).expect("valid TBI probe instruction");
        }
        let expected_memory = memory.clone();
        memory.copy_from_slice(&before);
        unsafe {
            for (i, w) in prog.iter().enumerate() { *((page + i as u64 * 4) as *mut u32) = *w; }
            *((page + prog.len() as u64 * 4) as *mut u32) = 0xd400_1001;
        }
        invalidate_range(page, (prog.len() * 4 + 4) as u64);
        let mut translated = clone_cpu(&init);
        setup_cpu(&mut translated);
        translated.helpers[HELPER_INTERP] = tbi_clobbering_helper as usize as u64;
        let before_fallback = STATS_FALLBACK.load(Ordering::Relaxed);
        let host = lookup_or_translate(page);
        let fb = STATS_FALLBACK.load(Ordering::Relaxed) - before_fallback;
        fallbacks += fb;
        native += prog.len() as u64 - fb;
        unsafe { attesor_jit_enter(&mut translated, host) };
        let pending = PENDING.with(|p| p.take());
        if snapshot(&reference) != snapshot(&translated) || memory != expected_memory
            || translated.exit_reason != EXIT_SVC || pending.is_some() {
            bad += 1;
            if bad <= 5 { println!("MISMATCH TBI block #{it} tag={} memory={}", it & 255, memory != expected_memory); }
        } else { ok += 1; }
    }
    unsafe { libc::munmap(page as *mut libc::c_void, 0x1000); }
    println!("jit selftest [tbiblocks]: ok={ok} mismatch={bad} skipped=0 native={native} fallback_insns={fallbacks}");
    assert_eq!(bad, 0, "TBI block differential mismatches");
}

/// Private complete units avoid relying on hot thresholds/chaining to exercise
/// actual nested host frames. The same guest program runs in the interpreter.
/// All branch targets (also changed-LR targets) are public unit entries.
fn call_program(rng: &mut Rng, mode: usize) -> Vec<u32> {
    fn branch(at: usize, to: usize, link: bool) -> u32 {
        (if link { 0x9400_0000 } else { 0x1400_0000 }) | ((to as i32 - at as i32) as u32 & 0x03ff_ffff)
    }
    fn adr(at: usize, to: usize, r: u32) -> u32 {
        let imm = ((to as i32 - at as i32) * 4) as u32;
        0x1000_0000 | ((imm & 3) << 29) | (((imm >> 2) & 0x7ffff) << 5) | r
    }
    const SAVE: u32 = 0xa9bf_7bfd; // stp x29,x30,[sp,#-16]!
    const RESTORE: u32 = 0xa8c1_7bfd;
    const RET: u32 = 0xd65f_03c0;
    // x25 = outer SP; x26 = outer continuation; x22 = nonmatching return target.
    let mut p = vec![0x9100_03f9, 0, 0, 0, 0, 0x9100_0400, 0xd400_1001,
                     0x9100_1c00, 0xd400_1021];
    p[1] = adr(1, 5, 26); p[2] = adr(2, 7, 22);
    if mode == 7 {
        // Recursion well beyond RAS_LIMIT. At the bound BL takes its plain
        // path; repeated guest return PCs may match older equivalent frames.
        let f = p.len();
        p.extend([SAVE, 0xd100_079c, 0, 0, 0, 0x9100_0400, RESTORE, RET]);
        p[f + 2] = 0xb400_001c | (4 << 5); // cbz x28, restore
        p[f + 3] = adr(f + 3, f, 17);
        p[f + 4] = if rng.next() & 1 == 0 { branch(f + 4, f, true) } else { 0xd63f_0220 };
        p[3] = adr(3, f, 17); p[4] = 0xd63f_0220;
        return p;
    }
    let n = 3 + rng.next() as usize % 6;
    let mut starts = Vec::new();
    let mut links = Vec::new();
    let mut outer_cont = 0;
    for f in 0..n {
        starts.push(p.len());
        let tail = f != 0 && f + 1 < n && rng.next() % 3 == 0;
        if !tail { p.push(SAVE); }
        // Scalar V values must not be cached across guest calls.
        p.push(0x1e61_2800); // fadd d0,d0,d1
        p.push(0x9100_0000 | ((1 + rng.next() as u32 % 31) << 10));
        if f == 0 {
            p.push(0x9100_03f8); // x24 = checkpoint SP inside outer function
            let at = p.len(); p.push(0); links.push((at, usize::MAX, false)); // ADR x23, after-call
        }
        if f + 1 == n {
            if mode == 6 { p.push(0x1ac0_4020); } // CRC32B helper barrier
            if mode == 4 || mode == 5 {
                // Longjmp to an older frame, or to a PC matching no frame.
                p.push(if mode == 4 { 0x9100_031f } else { 0x9100_033f }); // sp=x24/x25
                p.push(if mode == 4 { 0xaa17_03fe } else { 0xaa16_03fe }); // lr=x23/x22
                p.push(RET);
            } else { p.extend([RESTORE, RET]); }
        } else {
            let child = if mode == 4 || mode == 5 { f + 1 } else { f + 1 + rng.next() as usize % (n - f - 1) };
            let indirect = rng.next() & 1 != 0;
            if indirect {
                let at = p.len(); p.push(0); links.push((at, child, true));
                p.push(if tail { 0xd61f_0220 } else { 0xd63f_0220 });
            } else {
                let at = p.len(); p.push(0); links.push((at, child, false));
            }
            if f == 0 { outer_cont = p.len(); }
            if !tail { p.extend([0x9100_0c00, RESTORE, RET]); }
        }
    }
    for (at, child, indirect) in links {
        p[at] = if child == usize::MAX { adr(at, outer_cont, 23) }
            else if indirect { adr(at, starts[child], 17) }
            else {
                // Tail encodings have no restore/return after their transfer.
                let tail = at + 1 == p.len() || starts.contains(&(at + 1));
                branch(at, starts[child], !tail)
            };
    }
    p[3] = adr(3, starts[0], 17);
    p[4] = if mode & 1 == 0 { branch(4, starts[0], true) } else { 0xd63f_0220 };
    p
}

struct CallsCode { base: u64, len: usize }
impl CallsCode {
    fn new() -> Self {
        let len = 1 << 20;
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
        assert_ne!(p, libc::MAP_FAILED);
        Self { base: p as u64, len }
    }
    fn compile(&self, pc: u64, words: &[u32], callret: bool) -> (HashMap<u64, u64>, u64) {
        let mut entries = std::collections::BTreeSet::from([0usize]);
        for (i, &w) in words.iter().enumerate() {
            if emit::is_terminator(w) && i + 1 < words.len() { entries.insert(i + 1); }
            let target = if w & 0x7c00_0000 == 0x1400_0000 {
                Some(i as i64 + (((w & 0x03ff_ffff) as i32) << 6 >> 6) as i64)
            } else if w & 0x7e00_0000 == 0x3400_0000 {
                Some(i as i64 + ((((w >> 5) & 0x7ffff) as i32) << 13 >> 13) as i64)
            } else if w & 0x9f00_0000 == 0x1000_0000 {
                let imm = (((w >> 5) & 0x7ffff) << 2) | ((w >> 29) & 3);
                Some(i as i64 + ((imm as i32) << 11 >> 11) as i64 / 4)
            } else { None };
            if let Some(t) = target { assert!(t >= 0 && (t as usize) < words.len()); entries.insert(t as usize); }
        }
        let mut u = Unit::new(StubKind::Plain, false);
        u.callret = callret;
        for &i in &entries { let l = u.a.create_label(); u.labels.insert(pc + i as u64 * 4, l); }
        for &i in &entries {
            emit::emit_block(&mut u, pc + i as u64 * 4, &|p| p.checked_sub(pc).and_then(|x| words.get((x / 4) as usize)).copied());
        }
        u.finish();
        let r = u.assemble_options(self.base, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
        assert!(r.inner.code_buffer.len() < self.len);
        unsafe { std::ptr::copy_nonoverlapping(r.inner.code_buffer.as_ptr(), self.base as *mut u8, r.inner.code_buffer.len()); }
        let hosts = u.labels.iter().map(|(&p, l)| (p, r.label_ip(l).unwrap())).collect();
        (hosts, u.insns_fallback)
    }
}
impl Drop for CallsCode {
    fn drop(&mut self) { unsafe { libc::munmap(self.base as *mut _, self.len); } }
}

fn run_selftest_calls(count: usize, seed: Option<u64>) {
    const PC: u64 = 0x2000_0000;
    let code = CallsCode::new();
    let mut rng = Rng(seed.unwrap_or(0xca11_7e57_1234_5678));
    let mut stack = vec![0u8; 32768];
    let mut ok = 0; let mut bad = 0; let mut native = 0;
    for it in 0..count {
        let mode = it % 8;
        let words = call_program(&mut rng, mode);
        let (hosts, fallback) = code.compile(PC, &words, emit::callret_enabled());
        stack.fill(0);
        let mut init = Cpu::new();
        for r in 0..31 { init.x[r] = rng.val(); init.v[r] = [fp_val(&mut rng), rng.val()]; }
        init.v[0][0] = 0.5f64.to_bits(); init.v[1][0] = 0.25f64.to_bits();
        init.x[31] = (stack.as_mut_ptr() as u64 + stack.len() as u64 - 4096) & !15;
        init.x[28] = 270 + rng.next() % 65;
        init.set_nzcv((rng.next() & 15) as u32); init.set_pc(PC);
        let mut r = clone_cpu(&init);
        let mut svc = None;
        for _ in 0..20000 {
            let Some(&w) = r.pc().checked_sub(PC).and_then(|p| words.get((p / 4) as usize)) else { break };
            match interp::exec(&mut r, w) { Ok(()) => {}, Err(Exit::Svc(n)) => { svc = Some(n); break; }, Err(e) => panic!("calls reference {w:08x}: {e:?}") }
        }
        assert!(svc.is_some(), "nonterminating calls reference: {mode}");
        let expected_memory = stack.clone(); stack.fill(0);
        let mut j = clone_cpu(&init); setup_cpu(&mut j);
        // These full-tier fixture hosts are intentionally not published in BLOCKS.
        // Seed the fixture IBTC directly; tiered's production insertion requires
        // current registry ownership and must reject unregistered/cold hosts.
        ibtc_clear(); for (&p, &h) in &hosts {
            IBTC.with(|t| t.borrow_mut().as_mut().unwrap().insert(p, h));
        }
        let mut pending = None;
        for _ in 0..20000 {
            let Some(&host) = hosts.get(&j.pc()) else { break };
            unsafe { attesor_jit_enter(&mut j, host); }
            pending = PENDING.with(|p| p.take());
            if j.exit_reason != EXIT_CONTINUE || pending.is_some() { break; }
        }
        native += (fallback == 0) as usize;
        if snapshot(&j) != snapshot(&r) || stack != expected_memory || j.pc() != r.pc()
            || j.exit_reason != EXIT_SVC || j.exit_data != svc.unwrap() as u64 || pending.is_some() {
            bad += 1;
            if bad <= 5 {
                println!("MISMATCH calls #{it} mode={mode} pc={:#x}/{:#x} exit={} data={}", j.pc(), r.pc(), j.exit_reason, j.exit_data);
                for (i, (&a, b)) in snapshot(&r).iter().zip(snapshot(&j)).enumerate() { if a != b { println!(" slot {i}: interp={a:#x} jit={b:#x}"); } }
                for (i, &w) in words.iter().enumerate() { println!(" +{:03x} {w:08x} {}", i * 4, disasm(w)); }
            }
        } else { ok += 1; }
    }
    ibtc_clear(); // no dangling entries into the private mapping
    println!("jit selftest [calls]: ok={ok} mismatch={bad} skipped=0 native={native} callret={} deep={}", emit::callret_enabled() as u8, count / 8);
    assert_eq!(bad, 0, "call differential mismatches");
}

/// Whole-unit relocation: short/near branches, patchable slots, indirect calls,
/// RET, integer/FP constants, chaining, invalidation, and retranslation. Check
/// stub label metadata before execution so a missing continuation rebase reports
/// mismatches rather than jumping into the middle of an instruction.
fn run_selftest_jitcode(count: usize, seed: u64) {
    let page = unsafe { libc::mmap(std::ptr::null_mut(), 0x4000,
        libc::PROT_READ | libc::PROT_WRITE, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64 };
    assert_ne!(page, libc::MAP_FAILED as u64);
    let mut rng = Rng(seed.max(1));
    let (mut ok, mut bad) = (0, 0);
    let mut residues = [0usize; 8];
    for it in 0..count {
        invalidate_range(page, 0x4000);
        ibtc_clear();
        // Every unit in this standalone fixture belonged to the just-retired
        // generation. Production keeps old slot metadata for safety, but these
        // test-only stale slots will never execute again. Retaining them makes
        // the metadata audit quadratic (and auditing promotions multiplies it
        // by the tier threshold), hiding failures behind an L1 timeout.
        if let Some(ix) = INDEX.lock().unwrap().as_mut() { ix.slot_cont.clear(); }
        let mut init = Cpu::new();
        init.set_pc(page);
        for n in 0..32 { init.x[n] = rng.val(); }
        init.set_nzcv((rng.next() & 15) as u32);
        let mut p = Vec::new();
        // Change both encoding size and branch displacement. > 127 host bytes
        // requires near conditional transfers, including constant pool operands.
        for n in 0..it % 40 {
            let rd = (n % 5) as u32;
            p.push(0x9100_0000 | ((rng.next() as u32 & 4095) << 10) | (rd << 5) | rd);
        }
        p.push(0xd280_0000 | ((it as u32 & 1) << 5)); // movz x0, #0/1
        p.push(0xb100_0421); // adds x1, x1, #1
        p.push(0x9a84_0062 | (((it % 14) as u32) << 12)); // csel x2,x3,x4,cc
        p.push(0x1e6e_1000); // fmov d0,#1.0
        p.push(0x6ee0_f801); // fneg v1.2d,v0.2d: RIP-relative sign constant
        p.push(0x4ee1_c402); // fminnm v2.2d,v0.2d,v1.2d: cold paths/pool
        p.push(0x3500_0040); // cbnz w0,+8
        p.push(0x9100_24a5); // add x5,x5,#9
        let call = p.len(); p.push(0);
        p.push(0xca04_0063); // eor x3,x3,x4
        p.push(0xd400_1001); // svc #0x80
        let callee = p.len();
        p.push(0x9100_0442); // add x2,x2,#1
        p.push(0xd65f_03c0); // ret x30
        let rn = 6 + (it % 24) as u32;
        p[call] = if it & 1 == 0 { 0x9400_0000 | (callee - call) as u32 }
            else { init.x[rn as usize] = page + callee as u64 * 4; 0xd63f_0000 | (rn << 5) };
        unsafe { std::ptr::copy_nonoverlapping(p.as_ptr(), page as *mut u32, p.len()); }
        let mut reference = clone_cpu(&init);
        let mut stopped = false;
        for _ in 0..128 {
            let insn = unsafe { *(reference.pc() as *const u32) };
            match interp::exec(&mut reference, insn) {
                Ok(()) => {}, Err(Exit::Svc(_)) => { stopped = true; break },
                Err(e) => panic!("invalid jitcode reference {e:?}"),
            }
        }
        assert!(stopped);
        let mut translated = clone_cpu(&init);
        setup_cpu(&mut translated);
        let allocation_start = code_cache().cursor();
        lookup_or_translate(page);
        // Translate both callee and continuation up front. Repeated passes below
        // then exercise IBTC hits and already-chained direct exits.
        for pc in [page + callee as u64 * 4, page + (call + 1) as u64 * 4] {
            let h = lookup_or_translate(pc); ibtc_insert(pc, h);
        }
        let metadata_valid = || {
            INDEX.lock().unwrap().as_ref().unwrap().slot_cont.iter().all(|(&slot, &cont)| {
                // Restrict the audit to this iteration, including promotions.
                if cont < allocation_start { return true; }
                let bytes = unsafe { std::slice::from_raw_parts(cont as *const u8, 15) };
                // mmap guest PCs exceed imm32: MOV RAX, imm64 = 48 B8 + 8 bytes.
                let target = u64::from_le_bytes(bytes[2..10].try_into().unwrap());
                slot & 7 == 0 && bytes[..2] == [0x48, 0xb8]
                    && (page..page + 0x4000).contains(&target)
            })
        };
        let mut metadata_ok = metadata_valid();
        if metadata_ok {
            // Exercise real cold completions and promotion, not just three
            // direct entries into cold code (which has no patchable slots).
            let passes = if tiered_enabled() { tiered::threshold() as usize + 3 } else { 3 };
            for _ in 0..passes {
                translated = clone_cpu(&init); setup_cpu(&mut translated);
                let mut host = if tiered_enabled() { tiered_dispatch(page) } else { lookup_or_translate(page) };
                if !metadata_valid() { metadata_ok = false; break; }
                let mut exited = false;
                for _ in 0..32 {
                    let entry_pc = translated.pc();
                    unsafe { attesor_jit_enter(&mut translated, host); }
                    if tiered_enabled() && translated.exit_reason != emit::EXIT_INTERP {
                        note_cold_completion(entry_pc, host);
                    }
                    if translated.exit_reason == EXIT_SVC { exited = true; break; }
                    if translated.exit_reason == EXIT_PENDING { break; }
                    let pc = translated.pc();
                    let target = if tiered_enabled() { tiered_dispatch(pc) } else { lookup_or_translate(pc) };
                    if !metadata_valid() { metadata_ok = false; break; }
                    ibtc_insert(pc, target);
                    if translated.exit_data != 0 { chain_slot(translated.exit_data, pc, target); }
                    host = target;
                }
                if !exited || snapshot(&reference) != snapshot(&translated) { metadata_ok = false; break; }
            }
        }
        // The final dispatch may be a callee/continuation with no slots. Audit
        // the original public entry, after any cold-to-full promotion.
        residues[(lookup(page).expect("jitcode entry remains registered") & 7) as usize] += 1;
        if metadata_ok { ok += 1; }
        else { bad += 1; if bad <= 5 { println!("MISMATCH jitcode #{it}: seed={seed:#x} call_reg=x{rn}"); } }
    }
    invalidate_range(page, 0x4000); ibtc_clear();
    unsafe { libc::munmap(page as *mut _, 0x4000); }
    println!("jit selftest [jitcode]: ok={ok} mismatch={bad} seed={seed:#x} host-residues={residues:?}");
    assert_eq!(bad, 0, "whole-unit relocation differential mismatch");
    if slot_reuse() && count >= 256 { assert!(residues.iter().all(|n| *n > 0), "exercise all eight prefix alignments"); }
}

/// Every sample executes disk-loaded Plain code, not the runtime JIT. Public
/// entries at every instruction force cross-block cases and canonical reentry.
fn run_selftest_offline(count: usize, seed: u64) {
    let root = std::path::PathBuf::from(format!("/private/tmp/attesor-offline-{}-{seed:x}", std::process::id()));
    std::fs::create_dir(&root).expect("fresh private selftest cache");
    std::env::set_var("ATTESOR_CACHE", &root);
    let page = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) as u64 };
    assert_ne!(page, libc::MAP_FAILED as u64);
    if precise_invalidation_enabled() {
        // An image may end in the middle of a readable page. AOT must not
        // inline/read its neighbor and then register an image-only dependency.
        let words = [0xd280_00e0u32, 0x9100_0400, 0xd280_0540, 0xd400_1001];
        unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), page as *mut u32, words.len()); }
        assert_eq!(crate::aot::selftest_page(page, 2).unwrap(), 2);
        let original = lookup(page).expect("closed image must publish");
        let execute = || {
            let mut c = Cpu::new(); c.set_pc(page); setup_cpu(&mut c);
            for _ in 0..16 {
                let host = lookup_or_translate(c.pc());
                unsafe { attesor_jit_enter(&mut *c, host); }
                if c.exit_reason == EXIT_SVC { return c.x[0]; }
                assert_eq!(c.exit_reason, EXIT_CONTINUE);
            }
            panic!("closed-image boundary fixture did not exit");
        };
        assert_eq!(execute(), 42);
        unsafe { *((page + 8) as *mut u32) = 0xd280_0c60; } // MOV X0,#99
        invalidate_range(page + 8, 4);
        assert_eq!(lookup(page), Some(original), "neighbor invalidation must preserve proven AOT");
        assert_eq!(execute(), 99, "unbounded AOT reader would execute stale 42");
        invalidate_range(page, 8);
        assert_eq!(lookup(page), None);
        assert!(!register_aot_block(page, original, Some((page, page + 8))), "late stale image must stay revoked");
        invalidate_range(page + 32, 4);
        assert!(!register_aot_block(page + 32, original, Some((page + 32, page + 36))),
            "invalidation before the first load must also be remembered");
        println!("PASS offline closed-image boundary: stale oracle 42 != fresh 99; late load refused");
    }
    let removed0 = super::offline::REMOVED_STORES.load(Ordering::Relaxed);
    let mut rng = Rng(seed);
    let (mut ok, mut bad, mut loaded) = (0, 0, 0);
    let mut memory = vec![0u8; 256];
    let pointer = memory.as_mut_ptr() as u64;
    for it in 0..count {
        let d = (rng.next() % 16) as u32;
        let m = (d + 1) % 16;
        let sf = if rng.next() & 1 == 0 { 0 } else { 1 << 31 };
        let mov = |r: u32, value: u32| sf | 0x5280_0000u32 | ((value & 0xffff) << 5) | r;
        let a = rng.next() as u32;
        let b = rng.next() as u32;
        let mut prog = vec![mov(d, a)];
        match it % 9 {
            0 => { prog.extend([mov(d, b), 0x9100_0400 | (d << 5) | m]); }
            1 => { // Both diamond successors overwrite; CBZ does not read NZCV.
                prog.extend([0xb400_0060 | 19, mov(d, b), 0x1400_0002, mov(d, b ^ 0x55)]);
            }
            2 => { // Only one successor overwrites: the other observes d.
                prog.extend([0xb400_0060 | 19, mov(d, b), 0x1400_0002, 0x9100_0400 | (d << 5) | m]);
            }
            3 => { // Finite multi-block cycle; no overwrite can be inferred from a cycle alone.
                prog.extend([0x9100_0400 | (d << 5) | d, 0xf100_079c, 0x54ff_ffc1]);
            }
            4 => { prog.extend([0xf900_0280 | d, mov(d, b), 0xf940_0280 | m]); }
            5 => { // Interpreter helper observes Cpu state; CRC32B.
                prog.extend([0x1ac0_4000 | (m << 16) | (d << 5) | d, mov(m, b)]);
            }
            6 => { prog.extend([0xd400_1021, mov(d, b)]); }
            7 => { // SP updates, SP reads, and ZR discard must not alias.
                prog.extend([0x9100_43ff, 0x9100_03e0 | m, 0x9100_83ff, 0xaa1f_03ff, mov(d, b)]);
            }
            _ => { // W write zero extends; X read and overwrite chain.
                prog.extend([0x5280_0000 | ((b & 0xffff) << 5) | d,
                    0x8b00_0000 | (d << 16) | (d << 5) | m, mov(d, a ^ b)]);
            }
        }
        // Addressing forms remain observable barriers: signed pre/post, Q pair,
        // scaled SXTW (including negative index), and register/data aliases.
        match it % 5 {
            0 => prog.push(0xf800_8680 | d), // STR Xd,[x20],#8
            1 => prog.push(0xf81f_8e80 | d), // STR Xd,[x20,#-8]!
            2 => prog.extend([0x3cb5_da90, 0x3cb6_5a91]), // Q scaled SXTW/UXTW
            3 => prog.extend([0xad00_0680, 0xad40_0e82]), // STP/LDP Q
            _ => prog.push(0xf900_0280 | d),
        }
        prog.push(0xd400_1001);
        unsafe { std::ptr::copy_nonoverlapping(prog.as_ptr(), page as *mut u32, prog.len()); }
        invalidate_range(page, 4096);
        // Production AOT entries are immutable image code, not indexed JIT
        // units. This suite intentionally rewrites a page: discard its old
        // external registrations and thread-local IBTC before the next load.
        if let Some(blocks) = BLOCKS.write().unwrap().as_mut() {
            blocks.entries.retain(|&pc, _| pc < page || pc >= page + 4096);
            blocks.external.retain(|&pc, _| pc < page || pc >= page + 4096);
            assert!(blocks.external.is_empty(), "offline fixture owns all external entries");
            blocks.external_invalidations.clear();
        }
        ibtc_clear();
        // Each sample is a NEW image generation built from the current private
        // bytes below, not an attempt to republish an invalidated production
        // blob. Isolate these synthetic lifetimes just like the directed AOT
        // lifecycle test. Production's one-way latch is deliberately unchanged.
        EXTERNAL_INVALIDATED.store(false, Ordering::Relaxed);
        for byte in &mut memory { *byte = rng.next() as u8; }
        let initial_memory = memory.clone();
        let mut init = Cpu::new();
        for n in 0..32 { init.x[n] = rng.val(); init.v[n] = [rng.next(), rng.next()]; }
        init.x[19] = it as u64 & 1;
        init.x[20] = pointer + 128;
        init.x[21] = 0xfeed_beef_ffff_fffe; // negative SXTW, scale 16
        init.x[22] = 0xfeed_beef_0000_0002; // UXTW ignores high bits, scale 16
        init.x[28] = 1 + rng.next() % 8;
        init.set_nzcv((rng.next() & 15) as u32);
        // Alternate public entries prove that removed labelled stores retain
        // valid entry offsets. Do not enter the middle of a loop unboundedly.
        let entry = if it % 4 == 0 { 1 } else { 0 };
        init.set_pc(page + entry * 4);
        let mut reference = clone_cpu(&init);
        let mut svc = None;
        for _ in 0..1000 {
            let index = reference.pc().wrapping_sub(page) / 4;
            let Some(&w) = prog.get(index as usize) else { break; };
            match interp::exec(&mut reference, w) {
                Ok(()) => {}, Err(Exit::Svc(n)) => { svc = Some(n); break; },
                Err(e) => panic!("offline reference exit {e:?}: {w:08x}"),
            }
        }
        let expected_memory = memory.clone();
        memory.copy_from_slice(&initial_memory);
        let n = crate::aot::selftest_page(page, prog.len()).expect("build/load private AOT");
        assert!(n >= prog.len(), "all public entries must survive");
        loaded += n;
        let mut translated = clone_cpu(&init);
        setup_cpu(&mut translated);
        let jit0 = STATS_BLOCKS.load(Ordering::Relaxed);
        for _ in 0..1000 {
            let host = lookup_or_translate(translated.pc());
            unsafe { attesor_jit_enter(&mut translated, host); }
            if translated.exit_reason != EXIT_CONTINUE { break; }
        }
        assert_eq!(STATS_BLOCKS.load(Ordering::Relaxed), jit0, "must execute disk code, not a JIT replacement");
        let pending = PENDING.with(|p| p.take());
        if snapshot(&reference) != snapshot(&translated) || expected_memory != memory
            || reference.pc() != translated.pc() || translated.exit_reason != EXIT_SVC
            || Some(translated.exit_data as u16) != svc || pending.is_some() {
            bad += 1;
            if bad < 5 { println!("MISMATCH offline #{it} seed={seed:#x} words={prog:x?}\nreference={:x?}\ntranslated={:x?}", snapshot(&reference), snapshot(&translated)); }
        } else { ok += 1; }
    }
    invalidate_range(page, 4096);
    unsafe { libc::munmap(page as *mut _, 4096); }
    std::fs::remove_dir_all(root).unwrap();
    let removed = super::offline::REMOVED_STORES.load(Ordering::Relaxed) - removed0;
    // loopcensus publishes architectural PC at every public entry. This
    // suite gives every instruction a public entry, so those intentional
    // non-GPR observer barriers can prevent every DSE opportunity. Keep the
    // semantic checks above; require positive work in non-census runs instead.
    assert!(!super::offline::enabled() || super::loops::census_enabled()
        || count < 9 || removed > 0, "optimizer was not exercised");
    println!("jit selftest [offline]: removed_stores={removed} ok={ok} mismatch={bad} disk_loaded_blocks={loaded} seed={seed:#x} optimized={}", super::offline::enabled());
    assert_eq!(bad, 0, "offline differential mismatches");
}

fn zeroed(bytes: &[u8]) -> bool {
    // Exact OR reduction, not a hash: a nonzero byte cannot cancel another.
    bytes.iter().fold(0u8, |bits, &byte| bits | byte) == 0
}

pub fn run_selftest(class: &str, count: usize, seed: Option<u64>) {
    assert!(seed != Some(0), "xorshift seed must be nonzero");
    if class == "addrforms" { return run_selftest_addrforms(count, seed.unwrap_or(0xa44f_0123)); }
    if class == "offline" { return run_selftest_offline(count, seed.unwrap_or(0xd0ff_11ae_1234_5678)); }
    if class == "jitcode" { return run_selftest_jitcode(count, seed.unwrap_or(0xc0de_1234_5678_9abc)); }
    if class == "calls" { return run_selftest_calls(count, seed); }
    if class == "loops" { return run_selftest_loops(count, seed.unwrap_or(0x100f_5e1f_7e57_cafe)); }
    if class == "tbiblocks" { return run_selftest_tbiblocks(count, seed); }
    if class == "fpnan" {
        return run_selftest_fpnan(count);
    }
    if class == "fuse" {
        return run_selftest_fuse(count, false, seed);
    }
    if class == "fuse2" {
        return run_selftest_fuse(count, true, seed);
    }
    if class == "blocks" || class == "fpblocks" {
        return run_selftest_blocks(count, class == "fpblocks", seed);
    }
    if class == "selchain" {
        return run_selftest_selchain(count, seed);
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
    // Only suites whose entire encoding domain excludes data memory may use
    // the immutable zero-buffer oracle. These are the DP/SIMD op0 groups in
    // core::interp::exec; arbitrary masks that admit LD/ST retain byte snapshots.
    // The named branch suite also filters out exceptions before execution.
    let memory_free = class == "branch" || (0u32..16)
        .filter(|op0| ((op0 << 25) ^ val) & mask & 0x1e00_0000 == 0)
        .all(|op0| matches!(op0, 5 | 7 | 8 | 9 | 13 | 15));
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
    let mut rng = Rng(seed.unwrap_or(0x9e37_79b9_7f4a_7c15));
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
        init.set_pc(pc);
        let mem0 = if memory_free { Vec::new() } else { buf.clone() };
        let mut r = Cpu::new();
        *r = clone_cpu(&init);
        match interp::exec(&mut r, insn) {
            Ok(()) => {}
            Err(_) => {
                skipped += 1;
                continue;
            }
        }
        let mem_ref = if memory_free {
            assert!(zeroed(&buf), "non-memory reference changed the memory fixture");
            Vec::new()
        } else { buf.clone() };
        if !memory_free { buf.copy_from_slice(&mem0); }
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
            j.set_pc(j.pc() + 4);
            j.exit_reason = EXIT_SVC;
            j.exit_data = 0x80;
            native = native.saturating_sub(1);
        }
        let pending = PENDING.with(|p| p.take());
        if j.exit_reason == EXIT_PENDING || pending.is_some() {
            skipped += 1;
            if memory_free { buf.fill(0); } else { buf.copy_from_slice(&mem0); }
            continue;
        }
        let mut a = snapshot(&r);
        let mut b = snapshot(&j);
        if branch {
            a.push(r.pc());
            b.push(j.pc());
        }
        let mem_diff = if memory_free { !zeroed(&buf) } else { buf != mem_ref };
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
        if memory_free {
            if mem_diff { buf.fill(0); }
        } else { buf.copy_from_slice(&mem0); }
    }
    println!("jit selftest [{class}]: ok={ok} mismatch={bad} skipped={skipped} native={native}");
    assert_eq!(bad, 0, "instruction differential mismatches");
}

fn clone_cpu(c: &Cpu) -> Cpu {
    let mut n = Cpu::default();
    n.x = c.x;
    n.set_pc(c.pc());
    n.set_nzcv(c.nzcv());
    n.v = c.v;
    n.fpcr = c.fpcr;
    n.fpsr = c.fpsr;
    n.excl_addr = c.excl_addr;
    n.excl_val = c.excl_val;
    n.excl_val2 = c.excl_val2;
    n.excl_size = c.excl_size;
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
    // ATTESOR_DUMP_BYTES: how much to print (default 512; whole loops of FP code are larger)
    let n = std::env::var("ATTESOR_DUMP_BYTES").ok().and_then(|v| v.parse().ok()).unwrap_or(512usize).min(1 << 20);
    let bytes = unsafe { std::slice::from_raw_parts(host as *const u8, n) };
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    println!("{hex}");
}

#[cfg(test)]
mod callret_tests {
    use super::*;
    const PC: u64 = 0x2400_0000;

    #[test]
    fn non_memory_oracle_detects_every_byte_including_tail() {
        let mut bytes = vec![0u8; 4099];
        assert!(zeroed(&bytes));
        for n in 0..bytes.len() {
            bytes[n] = 0x80;
            assert!(!zeroed(&bytes));
            bytes[n] = 0;
        }
        assert!(zeroed(&bytes));
    }

    extern "C" fn change_epoch(c: *mut Cpu, _: u32, _: u64) -> u64 {
        let c = unsafe { &mut *c };
        let ptr = c.epoch_ptr as usize;
        if c.x[1] == 0 {
            unsafe { &*(ptr as *const AtomicU64) }.fetch_add(1, Ordering::SeqCst);
        } else {
            std::thread::spawn(move || {
                unsafe { &*(ptr as *const AtomicU64) }.fetch_add(1, Ordering::SeqCst);
            }).join().unwrap();
        }
        0
    }

    #[test]
    fn epoch_change_in_open_call_rejects_continuation_including_other_thread() {
        let code = CallsCode::new();
        // bl callee; add x0,x0,#1; svc; fallback; ret
        let words = [0x94000003, 0x91000400, 0xd4001001, 0, 0xd65f03c0];
        let (hosts, fallback) = code.compile(PC, &words, true);
        assert_eq!(fallback, 1);
        for thread in [0, 1] {
            let epoch = AtomicU64::new(17);
            let mut c = Cpu::new(); setup_cpu(&mut c);
            c.epoch_ptr = &epoch as *const _ as u64;
            c.helpers[HELPER_INTERP] = change_epoch as *const () as u64;
            c.x[0] = 42; c.x[1] = thread;
            c.set_pc(PC);
            unsafe { attesor_jit_enter(&mut *c, hosts[&PC]); }
            assert_eq!(epoch.load(Ordering::SeqCst), 18);
            assert_eq!(c.entry_epoch, 17);
            assert_eq!(c.pc(), PC + 4);
            assert_eq!(c.exit_reason, EXIT_CONTINUE); assert_eq!(c.exit_data, 0);
            assert_eq!(c.x[0], 42, "must not execute invalidated guest continuation");
            unsafe { attesor_jit_enter(&mut *c, hosts[&(PC + 4)]); }
            assert_eq!(c.entry_epoch, 18); assert_eq!(c.exit_reason, EXIT_SVC);
            assert_eq!(c.x[0], 43);
        }
    }

    #[test]
    fn svc_discards_open_frames_before_reentry() {
        let code = CallsCode::new();
        let words = [0x94000003, 0x91000400, 0xd4001001, 0xd4001021, 0xd65f03c0];
        let (hosts, _) = code.compile(PC, &words, true);
        let mut c = Cpu::new(); setup_cpu(&mut c); c.x[0] = 42;
        // Full-tier fixture hosts are not in the production registry.
        ibtc_clear(); for (&p, &h) in &hosts {
            IBTC.with(|t| t.borrow_mut().as_mut().unwrap().insert(p, h));
        }
        unsafe { attesor_jit_enter(&mut *c, hosts[&PC]); }
        assert_eq!(c.exit_reason, EXIT_SVC); assert_eq!(c.exit_data, 0x81);
        assert_eq!(c.pc(), PC + 16); assert_eq!(c.x[0], 42);
        unsafe { attesor_jit_enter(&mut *c, hosts[&(PC + 16)]); }
        assert_eq!(c.exit_reason, EXIT_SVC); assert_eq!(c.exit_data, 0x80);
        assert_eq!(c.x[0], 43);
        ibtc_clear();
    }
}
