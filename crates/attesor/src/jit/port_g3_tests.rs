//! Directed execution tests. Run serially in fresh processes: switches are frozen.
use super::*;

fn token(name: &str) -> bool {
    let has = |k| std::env::var(k).unwrap_or_default().split(',').any(|s| s == name);
    has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF")
}

struct Arena { raw: *mut libc::c_void, pc: u64 }
impl Arena {
    fn new() -> Self {
        let raw = unsafe { libc::mmap(std::ptr::null_mut(), 0x40000,
            libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0) };
        assert_ne!(raw, libc::MAP_FAILED);
        Self { raw, pc: (raw as u64 + 0x3fff) & !0x3fff }
    }
    fn write(&self, pc: u64, words: &[u32]) {
        unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), pc as *mut u32, words.len()); }
    }
}
impl Drop for Arena {
    fn drop(&mut self) {
        invalidate_range(self.raw as u64, 0x40000);
        ibtc_clear();
        if let Some(r) = BLOCKS.write().unwrap().as_mut() {
            let lo = self.raw as u64; let hi = lo + 0x40000;
            r.entries.retain(|&pc, _| pc < lo || pc >= hi);
            r.external.retain(|&pc, _| pc < lo || pc >= hi);
        }
        unsafe { libc::munmap(self.raw, 0x40000); }
    }
}
fn full(pc: u64) -> u64 {
    let host = translate(pc);
    if tiered_enabled() {
        for _ in 0..tiered::threshold() { note_cold_completion(pc, host); }
        tiered_dispatch(pc)
    } else { host }
}
fn enter(cpu: &mut Cpu, pc: u64, host: u64) {
    cpu.set_pc(pc); cpu.exit_reason = 0; cpu.exit_data = 0;
    unsafe { attesor_jit_enter(cpu, host); }
}

#[test]
fn g3_bliccache_execution_tags_epoch_alias_and_cold() {
    assert_eq!(bliccache::enabled(), token("bliccache"));
    let arena = Arena::new(); let site = arena.pc; let target = site + 0x8000;
    arena.write(target, &[0xd28000e0, 0xd4001001]); // MOV X0,#7; SVC
    let dest = full(target);
    let mut cpu = Cpu::new(); setup_cpu(&mut cpu);
    let active = token("bliccache") && !emit::callret_enabled() && !verify_mode();
    for (reg, instruction) in [(1, 0xd63f0020), (30, 0xd63f03c0), (1, 0xd73f0822)] {
        arena.write(site, &[instruction]); // BLR, x30 alias, existing BLRAA path
        invalidate_range(site, 4); let caller = full(site);
        ibtc_clear(); cpu.x[reg as usize] = target; cpu.x[0] = 99;
        enter(&mut cpu, site, caller);
        assert_eq!(cpu.x[30], site + 4);
        assert_eq!(cpu.pc(), target);
        assert_eq!(cpu.x[0], 99);
        assert_eq!(cpu.exit_reason, if active { emit::EXIT_BLICCACHE } else { EXIT_CONTINUE });
        if !active { continue; }
        assert_eq!(cpu.exit_data, site);
        bliccache::publish(site, target, dest);
        cpu.x[reg as usize] = target;
        enter(&mut cpu, site, caller);
        assert_eq!((cpu.x[0], cpu.exit_reason), (7, EXIT_SVC), "positive site-cache hit must execute callee");
        // Full site and target identity, not just hash bits. A wrong site must miss.
        let collision = (site + 4..site + 0x20000).step_by(4)
            .find(|&p| bliccache::offset(p) == bliccache::offset(site)).unwrap();
        bliccache::publish(collision, target, dest);
        cpu.x[reg as usize] = target; cpu.x[0] = 99;
        enter(&mut cpu, site, caller);
        assert_eq!((cpu.x[0], cpu.exit_reason), (99, emit::EXIT_BLICCACHE));
        bliccache::publish(site, target, dest);
        cpu.x[reg as usize] = target ^ (1 << 48);
        enter(&mut cpu, site, caller);
        assert_eq!(cpu.exit_reason, emit::EXIT_BLICCACHE);
        // Re-enter samples the NEW entry epoch; the OLD table epoch must reject.
        EPOCH.fetch_add(1, Ordering::SeqCst);
        cpu.x[reg as usize] = target; cpu.x[0] = 99;
        enter(&mut cpu, site, caller);
        assert_eq!((cpu.x[0], cpu.exit_reason), (99, emit::EXIT_BLICCACHE));
        bliccache::publish(site, target, dest);
        cpu.x[reg as usize] = target;
        enter(&mut cpu, site, caller); assert_eq!(cpu.x[0], 7);
        // Stale-code sensitivity control: without invalidation the actual cached
        // native callee still returns 7, not the new guest instruction's 42.
        arena.write(target, &[0xd2800540]);
        cpu.x[reg as usize] = target; enter(&mut cpu, site, caller);
        assert_eq!(cpu.x[0], 7); assert_ne!(cpu.x[0], 42);
        arena.write(target, &[0xd28000e0]);
    }
    if active {
        arena.write(site, &[0xd63f0020]); invalidate_range(site, 4);
        let caller = full(site);
        bliccache::publish(site, target, dest);
        arena.write(target, &[0xd2800540]);
        invalidate_range(target, 4);
        cpu.x[1] = target; cpu.x[0] = 99;
        enter(&mut cpu, site, caller);
        assert_eq!((cpu.x[0], cpu.exit_reason), (99, emit::EXIT_BLICCACHE));
        let fresh = full(target);
        bliccache::publish(site, target, fresh);
        enter(&mut cpu, site, caller);
        assert_eq!((cpu.x[0], cpu.exit_reason), (42, EXIT_SVC));
        // AOT/untracked and cold targets cannot become positive site entries.
        let external = target + 0x8000;
        register_block(external, dest);
        bliccache::publish(site, external, dest);
        cpu.x[1] = external; cpu.x[0] = 99;
        enter(&mut cpu, site, caller);
        assert_eq!((cpu.x[0], cpu.exit_reason), (99, if lookup(external) == Some(dest) { EXIT_CONTINUE } else { emit::EXIT_BLICCACHE }));
        if tiered_enabled() {
            let cold_pc = target + 0xc000;
            arena.write(cold_pc, &[0xd2800540, 0xd4001001]);
            let cold = tiered_dispatch(cold_pc);
            bliccache::publish(site, cold_pc, cold);
            cpu.x[1] = cold_pc; cpu.x[0] = 99;
            enter(&mut cpu, site, caller);
            assert_eq!((cpu.x[0], cpu.exit_reason), (99, EXIT_CONTINUE), "cold entry must stay in dispatcher");
        }
    }
    eprintln!("g3 bliccache: active={active}; native hits, full tags, collisions, epoch, x30 and stale-code control");
}

#[test]
fn g3_ibtc2_ways_fill_epoch_replacement_and_threads() {
    assert_eq!(emit::ibtc2_enabled(), token("ibtc2"));
    let arena = Arena::new(); let pc = arena.pc;
    let targets = [pc + 0x8000, pc + 0x18000, pc + 0x28000];
    let mut hosts = Vec::new();
    for (i, &at) in targets.iter().enumerate() {
        arena.write(at, &[0xd2800000 | (((i + 7) as u32) << 5), 0xd4001001]);
        hosts.push(full(at));
    }
    let mut cpu = Cpu::new(); setup_cpu(&mut cpu); ibtc_clear();
    // Low-level policy: two colliding tags coexist, refresh does not evict,
    // third distinct tag replaces way 0 (round robin, not LRU).
    let mut cache = IndirectCache::new();
    assert_eq!(cache.entries.len(), 32768);
    assert_eq!(cache.victims.is_some(), token("ibtc2"));
    cache.insert(targets[0], hosts[0]); cache.insert(targets[1], hosts[1]);
    if token("ibtc2") {
        let set = ((targets[0] >> 2) as usize) & (IBTC2_SETS - 1); let k = set * 2;
        assert_eq!(&cache.entries[k..k+2], &[[targets[0], hosts[0]], [targets[1], hosts[1]]]);
        cache.insert(targets[0], hosts[2]);
        assert_eq!(cache.victims.as_ref().unwrap()[set], 0);
        cache.insert(targets[2], hosts[2]);
        assert_eq!(&cache.entries[k..k+2], &[[targets[2], hosts[2]], [targets[1], hosts[1]]]);
        cache.clear(); cache.insert(1, hosts[0]); cache.insert(0, 0);
        assert!(cache.entries.iter().all(|&e| e == IBTC_EMPTY));
        cache.insert(0, hosts[0]); assert_eq!(cache.entries[0], [0, hosts[0]]);
    }
    for branch in [0xd61f0020, 0xd63f0020, 0xd65f0020] { // BR/BLR/RET X1
        arena.write(pc, &[branch]); invalidate_range(pc, 4); let caller = full(pc);
        ibtc_clear();
        for i in 0..2 { assert!(ibtc2_fill(&mut cpu, targets[i], hosts[i])); }
        for i in 0..2 {
            cpu.x[1] = targets[i]; cpu.x[0] = 99;
            if branch == 0xd63f0020 && token("bliccache") && !emit::callret_enabled() {
                bliccache::publish(pc, targets[i], hosts[i]);
            }
            enter(&mut cpu, pc, caller);
            assert_eq!((cpu.x[0], cpu.exit_reason), ((i + 7) as u64, EXIT_SVC), "both native ways must execute");
        }
        if !token("ibtc2") { continue; }
        // Third fill replaces the first way; BR must dispatch instead of
        // executing a tag with the same set but different complete PC.
        assert!(ibtc2_fill(&mut cpu, targets[2], hosts[2]));
        if branch != 0xd61f0020 { continue; }
        cpu.x[1] = targets[0]; cpu.x[0] = 99; enter(&mut cpu, pc, caller);
        assert_eq!((cpu.x[0], cpu.exit_reason), (99, EXIT_CONTINUE));
        cpu.x[1] = targets[1] ^ (1 << 48); enter(&mut cpu, pc, caller);
        assert_eq!(cpu.exit_reason, EXIT_CONTINUE);
        cpu.x[1] = 1; enter(&mut cpu, pc, caller); assert_eq!(cpu.exit_reason, EXIT_CONTINUE);
        // Deliberately stale table vs jit_enter resampling, on a second thread.
        std::thread::spawn(|| { EPOCH.fetch_add(1, Ordering::SeqCst); }).join().unwrap();
        cpu.x[1] = targets[1]; cpu.x[0] = 99; enter(&mut cpu, pc, caller);
        assert_eq!((cpu.x[0], cpu.exit_reason), (99, EXIT_CONTINUE));
        assert_ne!(cpu.ibtc_epoch, cpu.entry_epoch);
        sync_ibtc_epoch(&mut cpu);
        IBTC.with(|t| {
            let t = t.borrow(); let t = t.as_ref().unwrap();
            assert!(t.entries.iter().all(|&e| e == IBTC_EMPTY));
            assert!(t.victims.as_ref().unwrap().iter().all(|&v| v == 0));
        });
        assert!(ibtc2_fill(&mut cpu, targets[1], hosts[1]));
        enter(&mut cpu, pc, caller); assert_eq!(cpu.x[0], 8);
        // Actual stale-code oracle then production invalidation/fill refusal.
        arena.write(targets[1], &[0xd2800540]);
        enter(&mut cpu, pc, caller); assert_eq!(cpu.x[0], 8); assert_ne!(cpu.x[0], 42);
        invalidate_range(targets[1], 4);
        assert!(!ibtc2_fill(&mut cpu, targets[1], hosts[1]));
        hosts[1] = full(targets[1]); assert!(ibtc2_fill(&mut cpu, targets[1], hosts[1]));
        enter(&mut cpu, pc, caller); assert_eq!(cpu.x[0], 42);
        arena.write(targets[1], &[0xd2800100]); invalidate_range(targets[1], 4);
        hosts[1] = full(targets[1]);
    }
    if token("ibtc2") {
        let external = pc + 0x30000;
        register_block(external, hosts[0]);
        ibtc_clear(); let _ = ibtc2_fill(&mut cpu, external, hosts[0]);
        IBTC.with(|t| assert!(t.borrow().as_ref().unwrap().entries.iter().all(|&e| e == IBTC_EMPTY)));
        if tiered_enabled() {
            arena.write(external + 0x4000, &[0xd4001001]);
            let cold = tiered_dispatch(external + 0x4000);
            assert!(ibtc2_fill(&mut cpu, external + 0x4000, cold));
            IBTC.with(|t| assert!(t.borrow().as_ref().unwrap().entries.iter().all(|&e| e == IBTC_EMPTY)));
        }
    }
    let parent = cpu.ibtc;
    std::thread::spawn(move || {
        let mut c = Cpu::new(); c.ibtc = parent; setup_cpu(&mut c);
        assert_ne!(c.ibtc, parent, "setup must rebind to native-thread TLS");
        IBTC.with(|t| assert!(t.borrow().as_ref().unwrap().entries.iter().all(|&e| e == IBTC_EMPTY)));
    }).join().unwrap();
    eprintln!("g3 ibtc2: full tags, both native ways, replacement, epoch, SMC/fill, TLS; stale oracle 8 != 42");
}

fn branch(at: u64, target: u64) -> u32 {
    let delta = (target as i64 - at as i64) / 4;
    assert!((-0x2000000..0x2000000).contains(&delta));
    0x14000000 | (delta as u32 & 0x03ffffff)
}

#[test]
fn g3_rel32chain_range_shape() {
    let site = 0x1_0000_0000u64;
    for displacement in [i32::MIN as i128, -1, 0, i32::MAX as i128] {
        let target = (site as i128 + 5 + displacement) as u64;
        let mut b = [0xff, 0x25, 1, 2, 3, 4];
        assert!(rel32chain::link_unpublished(&mut b, site, site, target));
        assert_eq!((b[0], b[5]), (0xe9, 0x90));
        assert_eq!(i32::from_le_bytes(b[1..5].try_into().unwrap()) as i128, displacement);
    }
    for target in [site - 0x80000000 + 4, site + 0x80000000 + 5, u64::MAX] {
        let mut b = [0xff, 0x25, 1, 2, 3, 4]; let original = b;
        assert!(!rel32chain::link_unpublished(&mut b, site, site, target)); assert_eq!(b, original);
    }
    for mut b in [vec![0xff, 0x25], vec![0xe9, 0, 0, 0, 0, 0x90]] {
        let old = b.clone(); assert!(!rel32chain::link_unpublished(&mut b, site, site, site)); assert_eq!(b, old);
    }
}

extern "C" fn bump_epoch(_: *mut Cpu, _: u32, _: u64) -> u64 {
    std::thread::spawn(|| { EPOCH.fetch_add(1, Ordering::SeqCst); }).join().unwrap();
    0
}

#[test]
fn g3_rel32chain_published_execution_revocation_and_epoch() {
    assert_eq!(rel32chain::enabled(), token("rel32chain"));
    let arena = Arena::new(); let pc = arena.pc; let target = pc + 0x8000;
    arena.write(target, &[0xd28000e0, 0xd4001001]);
    // Publish on another native thread; the real entry wrapper serializes the consumer.
    let dest = std::thread::spawn(move || full(target)).join().unwrap();
    arena.write(pc, &[branch(pc, target)]); let source = full(pc);
    let (slot, direct_site, size) = {
        let guard = INDEX.lock().unwrap(); let ix = guard.as_ref().unwrap();
        let unit = ix.units.iter().find(|u| u.host == source).unwrap();
        let slot = ix.slot_cont.keys().copied().find(|&s| s >= source && s < source + unit.host_bytes).unwrap();
        let direct = ix.incoming.get(&dest).and_then(|links| links.iter().find_map(|link|
            if let IncomingLink::ImmutableRel32 { _site } = link { Some(*_site) } else { None }));
        (slot, direct, unit.host_bytes)
    };
    assert_eq!(direct_site.is_some(), token("rel32chain"), "actual publisher must produce E9 to an older guarded public entry");
    if let Some(site) = direct_site {
        let bytes = unsafe { std::slice::from_raw_parts(site as *const u8, 6) };
        assert_eq!((bytes[0], bytes[5]), (0xe9, 0x90));
        assert_eq!((site as i128 + 5 + i32::from_le_bytes(bytes[1..5].try_into().unwrap()) as i128) as u64, {
            let ix = INDEX.lock().unwrap();
            let ix = ix.as_ref().unwrap();
            ix.units[ix.guarded_entries[&dest] as usize].guard_host
        });
    }
    let mut cpu = Cpu::new(); setup_cpu(&mut cpu); cpu.x[0] = 99;
    enter(&mut cpu, pc, source);
    if !token("rel32chain") {
        assert_eq!(cpu.x[0], 99); assert_eq!(cpu.exit_reason, EXIT_CONTINUE);
        chain_slot(slot, target, dest); enter(&mut cpu, pc, source);
    }
    assert_eq!((cpu.x[0], cpu.exit_reason), (7, EXIT_SVC));
    arena.write(target, &[0xd2800540]);
    enter(&mut cpu, pc, source); assert_eq!(cpu.x[0], 7); assert_ne!(cpu.x[0], 42);
    let before = unsafe { std::slice::from_raw_parts(source as *const u8, size as usize) }.to_vec();
    invalidate_range(target, 4);
    if token("rel32chain") {
        assert_eq!(unsafe { std::slice::from_raw_parts(source as *const u8, size as usize) }, before);
    }
    cpu.x[0] = 99; enter(&mut cpu, pc, source);
    assert_eq!((cpu.x[0], cpu.pc(), cpu.exit_reason), (99, target, EXIT_CONTINUE), "dead guarded target must not execute old instructions");
    if token("rel32chain") { assert_eq!(cpu.exit_data, 0); }
    let new_dest = full(target); assert_ne!(dest, new_dest);
    if token("rel32chain") {
        chain_slot(slot, target, new_dest);
        cpu.x[0] = 99; enter(&mut cpu, pc, source); assert_eq!(cpu.x[0], 99, "no hot relink of an immutable edge");
        // A still-alive target rejects an epoch changed while a source runs.
        let helper_pc = pc + 0x10000;
        arena.write(helper_pc, &[0, branch(helper_pc + 4, target)]);
        let helper_source = full(helper_pc);
        cpu.helpers[HELPER_INTERP] = bump_epoch as *const () as u64;
        cpu.x[0] = 99; enter(&mut cpu, helper_pc, helper_source);
        assert_eq!((cpu.x[0], cpu.pc(), cpu.exit_reason, cpu.exit_data), (99, target, EXIT_CONTINUE, 0));
        let ix = INDEX.lock().unwrap(); let ix = ix.as_ref().unwrap();
        let id = ix.guarded_entries[&new_dest];
        assert_eq!(ix.units[id as usize].alive_guard.as_ref().unwrap().load(Ordering::SeqCst), 1);
    }
    invalidate_range(pc, 4); let new_source = full(pc);
    cpu.x[0] = 99; enter(&mut cpu, pc, new_source);
    if !token("rel32chain") {
        assert_eq!(cpu.x[0], 99);
        enter(&mut cpu, target, new_dest);
    }
    assert_eq!((cpu.x[0], cpu.exit_reason), (42, EXIT_SVC));
    eprintln!("g3 rel32chain: actual E9/disp32, cross-thread publication, no hot patch, dead/alive-epoch guards; stale 7 != fresh 42");
}

fn adr(rd: u32, offset: i32) -> u32 {
    let imm = offset as u32 & 0x1fffff;
    0x10000000 | ((imm & 3) << 29) | ((imm >> 2) << 5) | rd
}
fn notbi_unit(pc: u64, words: &[u32], commpage: bool, split: bool) -> (emit::Unit, usize) {
    let mut u = emit::Unit::new(StubKind::Plain, commpage);
    u.no_internal_jumps = true;
    let l = u.a.create_label(); u.labels.insert(pc, l);
    if split {
        let l = u.a.create_label(); u.labels.insert(pc + 4, l);
    }
    let read = |p: u64| p.checked_sub(pc).and_then(|n| words.get(n as usize / 4)).copied();
    emit::emit_block(&mut u, pc, &read);
    if split { emit::emit_block(&mut u, pc + 4, &read); }
    u.finish();
    let masks = u.a.instructions().iter().filter(|i| {
        i.op0_register() == iced_x86::Register::RAX
            && (i.mnemonic() == iced_x86::Mnemonic::And
                || (i.mnemonic() == iced_x86::Mnemonic::Shr && i.immediate8() == 8))
    }).count();
    (u, masks)
}

#[test]
fn g3_notbi_emission_native_memory_and_barriers() {
    let arena = Arena::new(); let pc = arena.pc; let data = pc + 0x100;
    let words = [adr(1, 0x100), 0xf9400420, 0x91002021, 0xf9000420, 0xf85f8022, 0xd4001001];
    let active = token("notbi");
    for commpage in [false, true] {
        let (mut u, masks) = notbi_unit(pc, &words, commpage, false);
        assert_eq!(u.insns_fallback, 0);
        assert_eq!(masks, if active && !commpage { 0 } else { 3 }, "proof must remove actual memory masks, never commpage redirection");
        let cc = code_cache(); let ip = cc.cursor();
        let res = u.assemble_options(ip, BlockEncoderOptions::NONE).unwrap();
        assert_eq!(cc.alloc(res.inner.code_buffer.len() as u64), ip);
        unsafe { std::ptr::copy_nonoverlapping(res.inner.code_buffer.as_ptr(), ip as *mut u8, res.inner.code_buffer.len()); }
        unsafe { *(data as *mut u64) = 0x1234; *((data + 8) as *mut u64) = 0x5678; *((data + 16) as *mut u64) = 0; }
        let mut reference = Cpu::new(); reference.set_pc(pc);
        for &w in &words { if let Err(Exit::Svc(_)) = interp::exec(&mut reference, w) { break; } }
        let memory = unsafe { *((data + 16) as *const u64) };
        unsafe { *((data + 16) as *mut u64) = 0; }
        let mut cpu = Cpu::new(); setup_cpu(&mut cpu); enter(&mut cpu, pc, ip);
        assert_eq!(&cpu.x[..3], &reference.x[..3]);
        assert_eq!((cpu.pc(), cpu.nzcv(), cpu.exit_reason), (reference.pc(), reference.nzcv(), EXIT_SVC));
        assert_eq!(unsafe { *((data + 16) as *const u64) }, memory);
    }
    // A separate public entry cannot inherit the preceding ADR fact.
    let (_, masks) = notbi_unit(pc, &[adr(1, 0x100), 0xf9400020, 0xd4001001], false, true);
    assert_eq!(masks, 1);
    // Unknown/partial/writeback/helper/conditional-label paths kill facts.
    for barrier in [0xd503201f, 0xf2800021, 0xf8408422, 0, 0xdac01042] {
        let (_, masks) = notbi_unit(pc, &[adr(1, 0x100), barrier, 0xf9400020, 0xd4001001], false, false);
        assert!(masks >= 1, "barrier {barrier:08x} must retain mask");
    }
    // Known tagged base must strip; load-to-base alias kills the next proof.
    let (_, masks) = notbi_unit(pc, &[0xd2f56001, 0xf9400020, 0xd4001001], false, false);
    assert_eq!(masks, 1);
    let (_, masks) = notbi_unit(pc, &[adr(1, 0x100), 0xf9400021, 0xf9400020, 0xd4001001], false, false);
    assert_eq!(masks, if active { 1 } else { 2 });
    eprintln!("g3 notbi: actual omitted masks, native/interpreter scalar load/store/negative offset, commpage and entry/helper/alias barriers");
}

static FIX2_LEASE_OBSERVED: AtomicBool = AtomicBool::new(false);
extern "C" fn fix2_check_execution_lease(_: *mut Cpu, _: u32, _: u64) -> u64 {
    FIX2_LEASE_OBSERVED.store(chain_graph().try_write().is_err(), Ordering::Relaxed);
    0
}

#[test]
fn fix2_rel32_forward_call_and_graph_publication() {
    if !token("rel32chain") || tiered_enabled() || verify_mode() { return; }
    let arena = Arena::new(); let pc = arena.pc; let target = pc + 0x8000;
    // Source published FIRST: this call cannot have an immutable older-target E9.
    arena.write(pc, &[branch(pc, target) | 0x80000000, 0xd4001001]);
    let source = full(pc);
    let split_entry = {
        let ix = INDEX.lock().unwrap(); let ix = ix.as_ref().unwrap();
        let unit = &ix.units[ix.guarded_entries[&source] as usize];
        unit.guard_host > source && unit.guard_host < source + unit.host_bytes
    };
    assert!(split_entry, "ordinary entries must bypass the immutable-edge guard");
    arena.write(target, &[0x91000400, 0xd65f03c0]); // ADD X0,#1; RET
    let dest = std::thread::spawn(move || full(target)).join().unwrap();
    let mut cpu = Cpu::new(); setup_cpu(&mut cpu); cpu.x[0] = 99;
    enter(&mut cpu, pc, source);
    assert_eq!((cpu.pc(), cpu.exit_reason, cpu.x[0]), (target, EXIT_CONTINUE, 99));
    let slot = cpu.exit_data;
    assert_ne!(slot, 0);
    let fallback = unsafe { *(slot as *const u64) };
    {
        // Never block an executing (possibly nonterminating) guest to publish.
        let reader = chain_graph().read().unwrap();
        chain_slot(slot, target, dest);
        assert_eq!(unsafe { *(slot as *const u64) }, fallback, "busy graph must defer new edges");
        drop(reader);
    }
    chain_slot(slot, target, dest);
    assert_eq!(unsafe { *(slot as *const u64) }, dest, "late call target must be linked, not pay dispatcher forever");
    enter(&mut cpu, pc, source);
    assert_eq!(cpu.x[0], 100, "one entry must execute the late call target");
    assert_eq!(cpu.exit_reason, if emit::callret_enabled() { EXIT_SVC } else { EXIT_CONTINUE });
    if emit::callret_enabled() { assert_eq!(cpu.pc(), pc + 8, "native return reaches the in-unit SVC continuation"); }
    // Real generated execution must hold the read lease, including helpers.
    let helper_pc = pc + 0x10000;
    arena.write(helper_pc, &[0, 0xd4001001]); let helper = full(helper_pc);
    cpu.helpers[HELPER_INTERP] = fix2_check_execution_lease as *const () as u64;
    enter(&mut cpu, helper_pc, helper);
    assert_eq!(cpu.exit_reason, EXIT_SVC);
    assert!(FIX2_LEASE_OBSERVED.load(Ordering::Relaxed), "native execution must hold the publication read lease");
    // Retiring a late target restores its mutable slot, unlike immutable E9s.
    invalidate_range(target, 4);
    assert_eq!(unsafe { *(slot as *const u64) }, fallback);
    cpu.x[0] = 77; enter(&mut cpu, pc, source);
    assert_eq!((cpu.pc(), cpu.x[0], cpu.exit_reason), (target, 77, EXIT_CONTINUE));
}

#[test]
fn fix2_rel32_fork_replaces_inherited_graph_leases() {
    if !token("rel32chain") { return; }
    crate::aot::set_enabled(false);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let _lease = chain_graph().read().unwrap();
        ready_tx.send(()).unwrap(); stop_rx.recv().unwrap();
    });
    ready_rx.recv().unwrap();
    let pid = crate::aot::fork_guest();
    if pid == 0 {
        let available = chain_graph().try_write().is_ok();
        unsafe { libc::_exit(if available { 0 } else { 23 }); }
    }
    let mut status = 0;
    if pid > 0 { unsafe { libc::waitpid(pid, &mut status, 0); } }
    stop_tx.send(()).unwrap(); reader.join().unwrap();
    assert!(pid > 0); assert_eq!(status, 0, "child must not inherit another thread's execution lease");
}

#[test]
fn fix2_rel32_guard_preserves_hot_body_and_slot_layout() {
    let arena = Arena::new(); let pc = arena.pc;
    let alive = AtomicU64::new(1);
    // Integer/conditional loop, native call+continuation, and FP constant pool.
    for words in [
        vec![0x91000400, 0xf1000400, 0x54ffffc1, 0xd4001001],
        vec![0x94002000, 0x91000400, 0xd4001001],
        vec![0x1e6e1000, 0x1e602800, 0xd4001001],
    ] {
        arena.write(pc, &words);
        for pad in [0, 3, 7] {
            let (mut plain, _, _) = emit_unit_with_dependencies(pc, pad, false, None);
            let (mut guarded, _, guard) = emit_unit_with_dependencies(pc, pad, false,
                Some(&alive as *const AtomicU64 as u64));
            let ip = 0x1234_0000;
            let a = plain.assemble_options(ip, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
            let b = guarded.assemble_options(ip, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
            assert_eq!(&b.inner.code_buffer[..a.inner.code_buffer.len()], a.inner.code_buffer,
                "revocation guard must not shift or change OFF's hot body, stubs, slots or constants");
            assert_eq!(a.label_ip(&plain.labels[&pc]).unwrap(), b.label_ip(&guarded.labels[&pc]).unwrap());
            assert_eq!(b.label_ip(&guard.unwrap()).unwrap(), ip + a.inner.code_buffer.len() as u64);
            if let Some(slot) = plain.first_slot {
                assert_eq!(a.label_ip(&slot).unwrap(), b.label_ip(&guarded.first_slot.unwrap()).unwrap());
            }
        }
    }
}


#[test]
fn fix2_rel32_dispatcher_installs_late_slot() {
    if !token("rel32chain") || tiered_enabled() || verify_mode() { return; }
    // Exercise run(), not just chain_slot(): either of the original two gates
    // is sufficient to permanently disable late links.
    crate::aot::set_enabled(false);
    let arena = Arena::new(); let pc = arena.pc; let target = pc + 0x8000;
    arena.write(pc, &[branch(pc, target)]);
    let source = full(pc); // target deliberately absent at source publication
    arena.write(target, &[0x91000400, 0xd4001001]); // ADD X0,#1; SVC
    let dest = full(target);
    let mut cpu = Cpu::new(); setup_cpu(&mut cpu);
    enter(&mut cpu, pc, source);
    let slot = cpu.exit_data;
    assert_ne!(slot, 0);
    let fallback = unsafe { *(slot as *const u64) };
    assert_ne!(fallback, dest);
    cpu.set_pc(pc); cpu.x[0] = 99;
    // Harmless getpid SVC, with a thread-local stop request. No host process
    // exit, shared syscall hooks, or test-only dispatcher branch is needed.
    cpu.x[16] = 20;
    crate::engine::exit_current_thread();
    run(&mut cpu);
    assert_eq!(unsafe { *(slot as *const u64) }, dest,
        "production EXIT_CONTINUE must install the late slot with rel32chain ON");
    cpu.x[0] = 99;
    enter(&mut cpu, pc, source);
    assert_eq!((cpu.x[0], cpu.exit_reason), (100, EXIT_SVC));
    assert_eq!(cpu.pc(), target + 8);
}

/// Isolate guarded-direct and predicted-slot costs from dispatch, translation and CPUID.
/// This is a diagnostic, NOT a timing assertion or proof that every possible
/// rel32 implementation is inherently slow. Run alone, with --ignored.
#[test]
#[ignore = "serial guard-vs-predicted-slot performance diagnostic"]
fn fix2_rel32_guard_edge_cost() {
    use iced_x86::code_asm::*;
    if !token("rel32chain") { return; }
    let alive = AtomicU64::new(1);
    let mut hosts = Vec::new();
    for guarded in [false, true] {
        let mut a = CodeAssembler::new(64).unwrap();
        let mut body = a.create_label(); let mut done = a.create_label();
        let mut guard = a.create_label(); let mut slot = a.create_label();
        a.set_label(&mut body).unwrap();
        a.inc(qword_ptr(r15 + attesor_core::cpu::off::X + 8)).unwrap();
        a.dec(qword_ptr(r15 + attesor_core::cpu::off::X)).unwrap();
        a.jz(done).unwrap();
        if guarded { a.jmp(guard).unwrap(); a.nop().unwrap(); }
        else { a.jmp(qword_ptr(slot)).unwrap(); }
        a.set_label(&mut done).unwrap();
        a.mov(qword_ptr(r15 + attesor_core::cpu::off::EXIT_REASON), EXIT_SVC as i32).unwrap();
        a.jmp(qword_ptr(r15 + attesor_core::cpu::off::JIT_EXIT)).unwrap();
        a.set_label(&mut slot).unwrap(); a.dq(&[0]).unwrap();
        if guarded {
            a.set_label(&mut guard).unwrap();
            rel32chain::emit_guard(&mut a, body, 0, &alive as *const AtomicU64 as u64);
        }
        let size = a.assemble(0).unwrap().len();
        let host = code_cache().alloc(size as u64);
        let result = a.assemble_options(host, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping(result.inner.code_buffer.as_ptr(), host as *mut u8, result.inner.code_buffer.len());
            std::ptr::write_unaligned(result.label_ip(&slot).unwrap() as *mut u64, host);
        }
        hosts.push(host);
    }
    for round in 0..7 {
        for mode in if round % 2 == 0 { [0, 1] } else { [1, 0] } {
            let mut cpu = Cpu::new(); setup_cpu(&mut cpu);
            cpu.x[0] = 10_000_000;
            let start = std::time::Instant::now();
            unsafe { attesor_jit_enter(&mut *cpu, hosts[mode]); }
            let ns = start.elapsed().as_nanos();
            assert_eq!((cpu.x[0], cpu.x[1], cpu.exit_reason), (0, 10_000_000, EXIT_SVC));
            eprintln!("[guard-cost] round={} guarded={} edges=10000000 ns={ns}", round + 1, mode);
        }
    }
}
