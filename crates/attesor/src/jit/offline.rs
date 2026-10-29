//! Offline-only dead Cpu GPR store elimination over the emitted region CFG.
//! No guest-memory alias/fault assumptions: every non-GPR memory access is an observer.
use iced_x86::{BlockEncoder, BlockEncoderOptions, Code, FlowControl, Instruction,
    InstructionBlock, InstructionInfoFactory, Mnemonic, OpAccess, OpKind, Register};
use std::collections::{HashMap, VecDeque};
use super::emit::Unit;

pub static REMOVED_STORES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn enabled() -> bool {
    std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|s| s == "offline-dse")
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

/// Keep CodeAssembler's label validation/index table. On an encoder error the
/// original chunk is retained, never a partially optimized translation.
pub fn assemble(u: &mut Unit, base: u64) -> Result<(iced_x86::code_asm::CodeAssemblerResult, Counts), iced_x86::IcedError> {
    let options = BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS;
    let mut original = u.a.assemble_options(base, options)?;
    if !enabled() { return Ok((original, Counts::default())); }
    let (insns, counts) = optimize(u.a.instructions());
    original.inner = match BlockEncoder::encode(64, InstructionBlock::new(&insns, base), options) {
        Ok(encoded) => encoded,
        Err(error) => {
            if super::stats_enabled() { eprintln!("[attesor] offline-dse: retaining original chunk: {error}"); }
            return Ok((original, Counts { before: counts.before, removed: 0 }));
        }
    };
    REMOVED_STORES.fetch_add(counts.removed as u64, std::sync::atomic::Ordering::Relaxed);
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
