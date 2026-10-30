//! Offline-only dead Cpu GPR store elimination over the emitted region CFG.
//! Offline-only Cpu GPR store elimination and conservative constant forwarding.
//! the exact jitcost entry marker (a preserved immediate write to private sampling state).
use iced_x86::{BlockEncoder, BlockEncoderOptions, Code, FlowControl, Instruction,
    InstructionBlock, InstructionInfoFactory, Mnemonic, OpAccess, OpKind, Register};
use std::collections::{HashMap, HashSet, VecDeque};
use super::emit::Unit;

pub static REMOVED_STORES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static FOLDED_LOADS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn enabled() -> bool {
    ["ATTESOR_JIT_ON", "ATTESOR_JIT_OFF"].map(|name|
        std::env::var(name).unwrap_or_default().split(',').any(|s| s == "offline-dse")) == [true, false]
}

fn constprop_enabled() -> bool {
    let selected = |name: &str| std::env::var(name).unwrap_or_default()
        .split(',').any(|s| s == "offline-constprop");
    selected("ATTESOR_JIT_ON") && !selected("ATTESOR_JIT_OFF")
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Counts { pub before: usize, pub removed: usize }

#[derive(Default)]
struct Node { read: u32, write: u32, store: u32, successors: Vec<usize>, exits: bool }

fn writes(a: OpAccess) -> bool {
    matches!(a, OpAccess::Write | OpAccess::CondWrite | OpAccess::ReadWrite | OpAccess::ReadCondWrite)
}
fn reads(a: OpAccess) -> bool {
    matches!(a, OpAccess::Read | OpAccess::CondRead | OpAccess::ReadWrite | OpAccess::ReadCondWrite)
}

/// Explicit allowlist: unsupported/exceptional operations remain observers.
fn pure(m: Mnemonic) -> bool {
    use Mnemonic::*;
    matches!(m, Mov | Movzx | Movsx | Movsxd | Lea | Add | Adc | Sub | Sbb | And | Or | Xor |
        Not | Neg | Inc | Dec | Shl | Shr | Sar | Shld | Shrd | Rol | Ror | Rcl | Rcr |
        Imul | Mul | Bswap | Bsf | Bsr | Lzcnt | Tzcnt | Popcnt | Test | Cmp | Nop |
        Seta | Setae | Setb | Setbe | Sete | Setne | Setg | Setge | Setl | Setle |
        Seto | Setno | Setp | Setnp | Sets | Setns |
        Cmova | Cmovae | Cmovb | Cmovbe | Cmove | Cmovne | Cmovg | Cmovge |
        Cmovl | Cmovle | Cmovo | Cmovno | Cmovp | Cmovnp | Cmovs | Cmovns)
}

fn graph(insns: &[Instruction]) -> Vec<Node> {
    let labels: HashMap<_, _> = insns.iter().enumerate()
        .filter(|(_, i)| i.ip() != 0).map(|(n, i)| (i.ip(), n)).collect();
    let mut factory = InstructionInfoFactory::new();
    insns.iter().enumerate().map(|(n, i)| {
        let mut node = Node::default();
        let flow = i.flow_control();
        let branch = matches!(flow, FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch)
            && i.op0_kind() == OpKind::NearBranch64;
        if i.has_lock_prefix() || i.has_rep_prefix() || i.has_repne_prefix()
            || !(branch || (flow == FlowControl::Next && (pure(i.mnemonic()) || i.code() == Code::Zero_bytes))) {
            node.exits = true; return node;
        }
        let info = factory.info(i);
        if info.used_registers().iter().any(|r| r.register().full_register() == Register::R15 && writes(r.access())) {
            node.exits = true; return node;
        }
        for mem in info.used_memory() {
            let off = mem.displacement();
            let size = mem.memory_size().size() as u64;
            // jitcost marks every public entry, including the per-instruction
            // entries used by AOT. Its immediate sampling-state store neither
            // reads architectural state nor aliases X[]. Keep the probe itself,
            // but allow liveness to flow across it so both opt-ins still work.
            // No other Cpu fields, marker reads/partial writes, guest memory or
            // register-sourced stores receive this exception.
            if mem.base() == Register::R15 && mem.index() == Register::None
                && mem.segment() == Register::DS
                && off == attesor_core::cpu::off::PROFILE_STATE as u64 && size == 8
                && mem.access() == OpAccess::Write && i.code() == Code::Mov_rm64_imm32
                && i.op0_kind() == OpKind::Memory
                && i.immediate32() == crate::profile::State::Jit as u32 {
                continue;
            }
            // Includes implicit/unknown memory, partial overlaps outside X[],
            // all guest loads/stores, helpers, flags/PC/FP state and segment aliases.
            if mem.base() != Register::R15 || mem.index() != Register::None
                || mem.segment() != Register::DS || size == 0 || off >= 256 || size > 256 - off {
                node.exits = true; return node;
            }
            let first = off / 8;
            let last = (off + size - 1) / 8;
            let mask = ((u32::MAX >> (31 - (last - first))) as u64) << first;
            if reads(mem.access()) { node.read |= mask as u32; }
            if mem.access() == OpAccess::Write && off % 8 == 0 && size == 8
                && matches!(i.code(), Code::Mov_rm64_r64 | Code::Mov_rm64_imm32)
                && i.op0_kind() == OpKind::Memory {
                node.write |= 1 << first;
                node.store = 1 << first;
            }
        }
        if branch {
            match labels.get(&i.near_branch_target()) {
                Some(&target) => node.successors.push(target),
                None => node.exits = true,
            }
        }
        if flow == FlowControl::Next || flow == FlowControl::ConditionalBranch {
            if n + 1 < insns.len() { node.successors.push(n + 1); }
            else { node.exits = true; }
        }
        node
    }).collect()
}

fn outgoing(node: &Node, dead: &[u32]) -> u32 {
    if node.exits || node.successors.is_empty() { return 0; }
    node.successors.iter().fold(u32::MAX, |v, &s| v & dead[s])
}

pub fn optimize(insns: &[Instruction]) -> (Vec<Instruction>, Counts) {
    let nodes = graph(insns);
    let mut predecessors = vec![Vec::new(); nodes.len()];
    for (n, node) in nodes.iter().enumerate() {
        for &s in &node.successors { predecessors[s].push(n); }
    }
    // Least fixed point: a cycle without a guaranteed overwrite proves nothing.
    let mut dead = vec![0; nodes.len()];
    let mut work: VecDeque<_> = (0..nodes.len()).rev().collect();
    let mut queued = vec![true; nodes.len()];
    while let Some(n) = work.pop_front() {
        queued[n] = false;
        let node = &nodes[n];
        let value = if node.exits { 0 } else { (outgoing(node, &dead) | node.write) & !node.read };
        if value != dead[n] {
            dead[n] = value;
            for &p in &predecessors[n] {
                if !queued[p] { queued[p] = true; work.push_back(p); }
            }
        }
    }
    let mut result = insns.to_vec();
    let mut counts = Counts { before: insns.len(), removed: 0 };
    for (n, node) in nodes.iter().enumerate() {
        if node.store != 0 && outgoing(node, &dead) & node.store != 0 {
            // Keep indices AND symbolic IPs, including a label attached to the
            // removed store. BlockEncoder relocates it to the next real byte.
            result[n] = Instruction::with(Code::Zero_bytes);
            result[n].set_ip(insns[n].ip());
            counts.removed += 1;
        }
    }
    (result, counts)
}

/// A full Cpu.x slot, not guest memory. The caller also requires graph()'s
/// conservative instruction/alias checks before inspecting this operand.
fn constant_slot(i: &Instruction) -> Option<usize> {
    let off = i.memory_displacement64();
    (i.memory_base() == Register::R15 && i.memory_index() == Register::None
        && i.memory_segment() == Register::DS && off < 256 && off % 8 == 0)
        .then_some((off / 8) as usize)
}

/// Forward only in one straight-line, observer-free segment. Every symbolic
/// label is a possible public/private entry: clear facts there rather than
/// guessing which predecessors are reachable. No facts cross a branch, loop,
/// helper, Cpu non-GPR access, guest memory operation or write to the Cpu base.
///
/// address_insns identify image-address MOVs requiring load-time relocation.
/// Their values MUST NOT be propagated into new unrelocated immediates, even
/// when an address happens to look like an ordinary integer constant.
fn forward_constants(insns: &[Instruction], address_insns: &[usize]) -> (Vec<Instruction>, usize) {
    let nodes = graph(insns);
    let addresses: HashSet<usize> = address_insns.iter().copied().collect();
    let mut result = insns.to_vec();
    let mut slots = [None; 32];
    let mut registers: HashMap<Register, u64> = HashMap::new();
    let mut factory = InstructionInfoFactory::new();
    let mut folded = 0;
    for (n, i) in insns.iter().enumerate() {
        if i.ip() != 0 { slots.fill(None); registers.clear(); }
        // DSE permits the jitcost marker, but constant forwarding treats every
        // non-GPR access as a barrier (and must not index slots with its offset).
        let non_gpr = factory.info(i).used_memory().iter().any(|m| {
            let off = m.displacement();
            let size = m.memory_size().size() as u64;
            off >= 256 || size == 0 || size > 256 - off
        });
        if nodes[n].exits || non_gpr || i.flow_control() != FlowControl::Next {
            slots.fill(None); registers.clear(); continue;
        }
        let source = |r: Register| registers.get(&r.full_register()).copied();
        // Compute operands before invalidating written host registers, so
        // aliases such as mov eax,eax retain their exact zero-extension.
        let value = if addresses.contains(&n) { None } else {
            match i.code() {
                Code::Mov_r64_imm64 => Some(i.immediate64()),
                Code::Mov_r32_imm32 => Some(i.immediate32() as u64),
                Code::Mov_rm64_imm32 if i.op0_kind() == OpKind::Register =>
                    Some(i.immediate32to64() as u64),
                Code::Mov_rm32_imm32 if i.op0_kind() == OpKind::Register =>
                    Some(i.immediate32() as u64),
                Code::Mov_r64_rm64 => match i.op1_kind() {
                    OpKind::Register => source(i.op1_register()),
                    OpKind::Memory => constant_slot(i).and_then(|s| slots[s]),
                    _ => None,
                },
                Code::Mov_r32_rm32 => match i.op1_kind() {
                    OpKind::Register => source(i.op1_register()).map(|v| v as u32 as u64),
                    OpKind::Memory => constant_slot(i).and_then(|s| slots[s]).map(|v| v as u32 as u64),
                    _ => None,
                },
                Code::Mov_rm64_r64 if i.op0_kind() == OpKind::Register => source(i.op1_register()),
                Code::Mov_rm32_r32 if i.op0_kind() == OpKind::Register =>
                    source(i.op1_register()).map(|v| v as u32 as u64),
                _ => None,
            }
        };
        let stored = if i.op0_kind() == OpKind::Memory {
            match i.code() {
                Code::Mov_rm64_imm32 => constant_slot(i).map(|s| (s, i.immediate32to64() as u64)),
                Code::Mov_rm64_r64 => constant_slot(i).and_then(|s|
                    source(i.op1_register()).map(|value| (s, value))),
                _ => None,
            }
        } else { None };
        if i.op0_kind() == OpKind::Register && i.op1_kind() == OpKind::Memory {
            if let Some(value) = value {
                let code = match i.code() {
                    Code::Mov_r64_rm64 => Some(Code::Mov_r64_imm64),
                    Code::Mov_r32_rm32 => Some(Code::Mov_r32_imm32),
                    _ => None,
                };
                let replacement = code.and_then(|code| match code {
                    Code::Mov_r32_imm32 => Instruction::with2(code, i.op0_register(), value as u32).ok(),
                    _ => Instruction::with2(code, i.op0_register(), value).ok(),
                });
                if let Some(mut replacement) = replacement {
                    replacement.set_ip(i.ip());
                    result[n] = replacement;
                    folded += 1;
                }
            }
        }
        let info = factory.info(i);
        for mem in info.used_memory() {
            if writes(mem.access()) {
                // graph() already proved this complete access lies in Cpu.x.
                // A partial/conditional write invalidates every overlapped slot.
                let first = mem.displacement() as usize / 8;
                let last = (mem.displacement() as usize + mem.memory_size().size() - 1) / 8;
                slots[first..=last].fill(None);
            }
        }
        for r in info.used_registers() {
            if writes(r.access()) { registers.remove(&r.register().full_register()); }
        }
        if i.op0_kind() == OpKind::Register {
            if let Some(value) = value { registers.insert(i.op0_register().full_register(), value); }
        }
        if let Some((slot, value)) = stored { slots[slot] = Some(value); }
    }
    (result, folded)
}

/// Keep CodeAssembler's label validation/index table. On an encoder error the
/// original chunk is retained, never a partially optimized translation.
pub fn assemble(u: &mut Unit, base: u64) -> Result<(iced_x86::code_asm::CodeAssemblerResult, Counts), iced_x86::IcedError> {
    let options = BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS;
    let mut original = u.assemble_options(base, options)?;
    let (dse, constprop) = (enabled(), constprop_enabled());
    if !dse && !constprop { return Ok((original, Counts::default())); }
    let (forwarded, folded) = if constprop {
        // This tree persists only fixed-address images (MCLAOT05), with no
        // load-time image relocation. If relocatable AOT is added, pass its
        // address-immediate instruction indices here before enabling forwarding.
        forward_constants(u.a.instructions(), &[])
    } else { (u.a.instructions().to_vec(), 0) };
    let (insns, counts) = if dse { optimize(&forwarded) }
        else { (forwarded, Counts { before: u.a.instructions().len(), removed: 0 }) };
    original.inner = match BlockEncoder::encode(64, InstructionBlock::new(&insns, base), options) {
        Ok(encoded) => encoded,
        Err(error) => {
            if super::stats_enabled() { eprintln!("[attesor] offline optimizer: retaining original chunk: {error}"); }
            return Ok((original, Counts { before: counts.before, removed: 0 }));
        }
    };
    REMOVED_STORES.fetch_add(counts.removed as u64, std::sync::atomic::Ordering::Relaxed);
    FOLDED_LOADS.fetch_add(folded as u64, std::sync::atomic::Ordering::Relaxed);
    if folded != 0 && super::stats_enabled() {
        eprintln!("[attesor] offline-constprop: forwarded_cpu_loads={folded}");
    }
    Ok((original, counts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_x86::code_asm::*;
    use attesor_core::{cpu::Cpu, interp};
    use crate::jit::emit::{self, StubKind};

    fn code(f: impl FnOnce(&mut CodeAssembler)) -> Vec<Instruction> {
        let mut a = CodeAssembler::new(64).unwrap();
        f(&mut a);
        a.jmp(qword_ptr(r15 + attesor_core::cpu::off::JIT_EXIT)).unwrap();
        a.take_instructions()
    }
    #[test]
    fn constprop_directed_and_sensitivity() {
        let insns = code(|a| {
            a.mov(rax, 0x1234567887654321u64).unwrap();
            a.mov(rcx, rax).unwrap();
            a.mov(qword_ptr(r15), rcx).unwrap();
            a.mov(rdx, qword_ptr(r15)).unwrap();
            a.mov(qword_ptr(r15 + 8), rdx).unwrap();
            a.mov(edx, dword_ptr(r15)).unwrap();
            a.mov(qword_ptr(r15 + 16), rdx).unwrap();
        });
        let (optimized, folded) = forward_constants(&insns, &[]);
        assert_eq!(folded, 2);
        assert_eq!(optimized[3].code(), Code::Mov_r64_imm64);
        assert_eq!(optimized[5].immediate32(), 0x87654321);
        let execute = |instructions: &[Instruction]| {
            let result = BlockEncoder::encode(64, InstructionBlock::new(instructions, 0), 0).unwrap();
            let base = crate::jit::code_cache().alloc(result.code_buffer.len() as u64);
            unsafe { std::ptr::copy_nonoverlapping(result.code_buffer.as_ptr(), base as *mut u8, result.code_buffer.len()); }
            let mut cpu = Cpu::new();
            crate::jit::setup_cpu(&mut cpu);
            unsafe { crate::jit::attesor_jit_enter(&mut *cpu, base); }
            cpu.x
        };
        let expected = execute(&insns);
        assert_eq!(expected[1], 0x1234567887654321);
        assert_eq!(expected[2], 0x87654321);
        assert_eq!(execute(&optimized), expected);
        let mut mutant = optimized;
        mutant[3].set_immediate64(42);
        assert_ne!(execute(&mutant), expected, "bad forwarded constant must be detected");
        assert_eq!(forward_constants(&insns, &[0]).1, 0, "relocations cannot propagate");
    }

    #[test]
    fn constprop_barriers_partial_alias_and_labels() {
        for variant in 0..7 {
            let mut insns = code(|a| {
                a.mov(qword_ptr(r15), 7).unwrap();
                match variant {
                    0 => a.mov(byte_ptr(r15 + 1), 9).unwrap(),
                    1 => a.cmovne(rax, qword_ptr(r15)).unwrap(),
                    2 => a.call(qword_ptr(r15 + 1000)).unwrap(),
                    3 => a.mov(rax, qword_ptr(rdx)).unwrap(),
                    4 => a.mov(qword_ptr(r15 + attesor_core::cpu::off::PROFILE_STATE), crate::profile::State::Jit as i32).unwrap(),
                    _ => a.nop().unwrap(),
                }
                a.mov(rax, qword_ptr(r15)).unwrap();
            });
            if variant == 5 { insns[2].set_ip(123); }
            assert_eq!(forward_constants(&insns, &[]).1, usize::from(variant == 1 || variant == 6), "variant {variant}");
        }
        let insns = code(|a| {
            a.mov(rax, 123u64).unwrap();
            a.mov(al, 7).unwrap();
            a.mov(qword_ptr(r15), rax).unwrap();
            a.mov(rcx, qword_ptr(r15)).unwrap();
        });
        assert_eq!(forward_constants(&insns, &[]).1, 0);
    }

    fn stores(a: &mut CodeAssembler) {
        a.mov(qword_ptr(r15), 7).unwrap();
        a.mov(qword_ptr(r15), 9).unwrap();
    }
    #[test]
    fn full_stores_and_label_indices() {
        let mut a = CodeAssembler::new(64).unwrap();
        let mut entry = a.create_label();
        a.set_label(&mut entry).unwrap();
        stores(&mut a);
        a.jmp(qword_ptr(r15 + 256)).unwrap();
        let mut baseline = a.assemble_options(0, BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
        let (optimized, counts) = optimize(a.instructions());
        assert_eq!(counts.removed, 1);
        assert_eq!(optimized[0].ip(), a.instructions()[0].ip());
        baseline.inner = BlockEncoder::encode(64, InstructionBlock::new(&optimized, 0),
            BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS).unwrap();
        assert_eq!(baseline.label_ip(&entry).unwrap(), 0);
        assert_eq!(baseline.inner.new_instruction_offsets[0], baseline.inner.new_instruction_offsets[1]);
    }
    #[test]
    fn reads_partial_writes_and_aliases_are_observers() {
        for variant in 0..6 {
            let insns = code(|a| {
                a.mov(qword_ptr(r15), 7).unwrap();
                match variant {
                    0 => a.mov(eax, dword_ptr(r15 + 4)).unwrap(),
                    1 => a.mov(ax, word_ptr(r15 + 7)).unwrap(),
                    2 => a.mov(byte_ptr(r15), 1).unwrap(),
                    3 => a.mov(dword_ptr(r15), 1).unwrap(),
                    4 => a.add(qword_ptr(r15), 1).unwrap(),
                    _ => a.mov(rax, qword_ptr(r15)).unwrap(),
                }
                if variant < 2 || variant >= 4 { a.mov(qword_ptr(r15), 9).unwrap(); }
            });
            assert_eq!(optimize(&insns).1.removed, 0, "variant {variant}");
        }
    }
    #[test]
    fn memory_helpers_base_writes_and_unknown_operations_are_barriers() {
        for variant in 0..9 {
            let insns = code(|a| {
                a.mov(qword_ptr(r15), 7).unwrap();
                match variant {
                    0 => a.mov(rax, qword_ptr(rdx)).unwrap(),
                    1 => a.mov(qword_ptr(rdx), rax).unwrap(),
                    2 => a.call(qword_ptr(r15 + 1000)).unwrap(),
                    3 => a.mov(r15, rax).unwrap(),
                    4 => a.mov(byte_ptr(r15 + 264), al).unwrap(),
                    5 => a.mov(qword_ptr(r15 + 252), rax).unwrap(),
                    6 => a.div(rcx).unwrap(),
                    7 => a.mov(rax, qword_ptr(r15 + rdx * 2)).unwrap(),
                    _ => a.ud2().unwrap(),
                }
                a.mov(qword_ptr(r15), 9).unwrap();
            });
            assert_eq!(optimize(&insns).1.removed, 0, "variant {variant}");
        }
    }
    #[test]
    fn jitcost_entry_marker_is_preserved_without_blocking_dse() {
        use attesor_core::cpu::off;
        for variant in 0..7 {
            let insns = code(|a| {
                a.mov(qword_ptr(r15), 7).unwrap();
                match variant {
                    0 => a.mov(qword_ptr(r15 + off::PROFILE_STATE), crate::profile::State::Jit as i32).unwrap(),
                    1 => a.mov(qword_ptr(r15 + off::PC), crate::profile::State::Jit as i32).unwrap(),
                    2 => a.mov(dword_ptr(r15 + off::PROFILE_STATE), crate::profile::State::Jit as i32).unwrap(),
                    3 => a.mov(qword_ptr(r15 + off::PROFILE_STATE), rax).unwrap(),
                    4 => a.mov(rax, qword_ptr(r15 + off::PROFILE_STATE)).unwrap(),
                    5 => a.mov(qword_ptr(r15 + rdx + off::PROFILE_STATE), crate::profile::State::Jit as i32).unwrap(),
                    _ => a.mov(qword_ptr(r15 + off::PROFILE_STATE), crate::profile::State::Interp as i32).unwrap(),
                }
                a.mov(qword_ptr(r15), 9).unwrap();
            });
            let (optimized, counts) = optimize(&insns);
            assert_eq!(counts.removed, usize::from(variant == 0), "variant {variant}");
            assert_eq!(optimized[1], insns[1], "diagnostic probe must never be removed or modified");
        }
    }

    #[test]
    fn diamond_requires_both_successors_and_unknown_edges_fail_closed() {
        for variant in 0..3 {
            let insns = code(|a| {
                let mut right = a.create_label();
                let mut exit = a.create_label();
                a.mov(qword_ptr(r15), 7).unwrap();
                if variant == 2 { a.je(0x100000u64).unwrap(); }
                else { a.je(right).unwrap(); }
                a.mov(qword_ptr(r15), 9).unwrap();
                a.jmp(exit).unwrap();
                a.set_label(&mut right).unwrap();
                if variant == 0 { a.mov(qword_ptr(r15), 11).unwrap(); }
                else { a.mov(rax, qword_ptr(r15)).unwrap(); }
                a.set_label(&mut exit).unwrap();
                a.nop().unwrap();
            });
            assert_eq!(optimize(&insns).1.removed, if variant == 0 { 1 } else { 0 });
        }
    }
    #[test]
    fn cycles_do_not_create_a_proof_and_all_gpr_slots_are_independent() {
        let insns = code(|a| {
            let mut spin = a.create_label();
            a.mov(qword_ptr(r15), 7).unwrap();
            a.set_label(&mut spin).unwrap();
            a.nop().unwrap();
            a.jmp(spin).unwrap();
        });
        assert_eq!(optimize(&insns).1.removed, 0);
        let insns = code(|a| {
            for n in 0..32 { a.mov(qword_ptr(r15 + n * 8), rax).unwrap(); }
            for n in 0..32 { a.mov(qword_ptr(r15 + n * 8), 9).unwrap(); }
        });
        assert_eq!(optimize(&insns).1.removed, 32);
    }

    fn sensitivity(branch: bool) {
        const PC: u64 = 0x2d00_0000;
        let words = if branch {
            // Only the untaken successor overwrites x0. Ignoring the taken
            // successor's observer would lose the initial 7 in x1.
            vec![0xd280_00e0, 0xb400_0062, 0xd280_0120, 0x1400_0002, 0x9100_0001, 0xd400_1001]
        } else {
            vec![0xd280_00e0, 0x9100_0001, 0xd280_0120, 0xd400_1001]
        };
        let read = |p: u64| words.get(p.wrapping_sub(PC) as usize / 4).copied();
        let mut u = Unit::new(StubKind::Plain, false);
        for n in 0..words.len() { let l = u.a.create_label(); u.labels.insert(PC + n as u64 * 4, l); }
        for n in 0..words.len() { emit::emit_block(&mut u, PC + n as u64 * 4, &read); }
        u.finish();
        let (optimized, _) = optimize(u.a.instructions());
        let first = graph(u.a.instructions()).iter().position(|n| n.store == 1).unwrap();
        assert_ne!(optimized[first].code(), Code::Zero_bytes, "live store removed");
        let mut reference = Cpu::new();
        reference.set_pc(PC);
        loop {
            let w = read(reference.pc()).unwrap();
            match interp::exec(&mut reference, w) {
                Ok(()) => {}, Err(interp::Exit::Svc(_)) => break,
                Err(e) => panic!("{e:?}"),
            }
        }
        let execute = |instructions: &[Instruction]| {
            let result = BlockEncoder::encode(64, InstructionBlock::new(instructions, 0), 0).unwrap();
            let base = crate::jit::code_cache().alloc(result.code_buffer.len() as u64);
            unsafe { std::ptr::copy_nonoverlapping(result.code_buffer.as_ptr(), base as *mut u8, result.code_buffer.len()); }
            let mut cpu = Cpu::new(); cpu.set_pc(PC);
            crate::jit::setup_cpu(&mut cpu);
            unsafe { crate::jit::attesor_jit_enter(&mut *cpu, base); }
            cpu.x
        };
        assert_eq!(execute(&optimized), reference.x);
        // Test-only mutation: deliberately violate read/all-successor liveness.
        // No environment-controlled broken optimizer exists in production.
        let mut mutant = optimized;
        mutant[first] = Instruction::with(Code::Zero_bytes);
        mutant[first].set_ip(u.a.instructions()[first].ip());
        let actual = execute(&mutant);
        let mismatches = actual.iter().zip(reference.x).filter(|(a, b)| **a != *b).count();
        eprintln!("offline sensitivity branch={branch}: interpreter mismatched slots={mismatches}");
        assert!(mismatches > 0, "sensitivity mutation was not detected");
    }
    #[test]
    fn sensitivity_read_observer_vs_interpreter() { sensitivity(false); }
    #[test]
    fn sensitivity_branch_successor_vs_interpreter() { sensitivity(true); }
}
