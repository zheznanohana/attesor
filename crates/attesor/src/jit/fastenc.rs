//! Opt-in, bounded byte encoding for common legacy x86-64 templates.
//!
//! This is not a replacement assembler: CodeAssembler still selects instructions,
//! and iced still lays out every unit, resolves labels and encodes every miss.
//! A hit becomes one DeclareByte instruction, retaining instruction indexes for
//! label_ip(). No branch, relocation, labelled instruction or IP-relative operand
//! is encoded here. MOV r64/imm64 is deliberately left to iced as well: address
//! materialization may acquire relocation metadata independently of this module.
//! Keep this allowlist narrow until independently reviewed.

use iced_x86::code_asm::{CodeAssembler, CodeAssemblerResult};
use iced_x86::{BlockEncoderOptions, Code, IcedError, Instruction, OpKind, Register, RoundingControl};

pub(super) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let contains = |name| std::env::var(name).unwrap_or_default().split(',').any(|g| g == "fastenc");
        !contains("ATTESOR_JIT_OFF") && contains("ATTESOR_JIT_ON")
    })
}

/// Only called on finished Units. Preserve the original assembler, all label IDs,
/// and exactly one instruction per input (including zero-byte label anchors).
/// Other option sets use iced unchanged: DeclareByte constant-offset metadata is
/// not the original instruction's immediate/displacement metadata.
pub(super) fn assemble(
    original: &mut CodeAssembler,
    ip: u64,
    options: u32,
) -> Result<CodeAssemblerResult, IcedError> {
    if !enabled() || original.bitness() != 64
        || options != BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS
    {
        return original.assemble_options(ip, options);
    }
    let mut direct = CodeAssembler::new(64)?;
    let mut hits = 0;
    for instruction in original.instructions() {
        let replacement = encode(instruction);
        hits += usize::from(replacement.is_some());
        if direct.add_instruction(replacement.unwrap_or(*instruction)).is_err() {
            return original.assemble_options(ip, options);
        }
    }
    if hits == 0 {
        return original.assemble_options(ip, options);
    }
    // A mixed-layout error must not remove the complete, original iced path.
    direct.assemble_options(ip, options).or_else(|_| original.assemble_options(ip, options))
}

#[derive(Clone, Copy)]
enum Form {
    RmReg,
    RegRm,
    RmImm8,
    RmImm32,
    RegImm32,
    AccImm32,
}

/// (64-bit operand, opcode, ModRM group extension, operand form).
fn template(code: Code) -> Option<(bool, u8, u8, Form)> {
    use Code::*;
    use Form::*;
    // Explicit Code variants preserve operand width and sign-extension semantics.
    // No mnemonic-based guessing and no immediate-width shrinking.
    Some(match code {
        Mov_rm32_r32 => (false, 0x89, 0, RmReg),
        Mov_rm64_r64 => (true, 0x89, 0, RmReg),
        Mov_r32_rm32 => (false, 0x8b, 0, RegRm),
        Mov_r64_rm64 => (true, 0x8b, 0, RegRm),
        Mov_r32_imm32 => (false, 0xb8, 0, RegImm32),
        Mov_rm32_imm32 => (false, 0xc7, 0, RmImm32),
        Mov_rm64_imm32 => (true, 0xc7, 0, RmImm32),
        Add_rm32_r32 => (false, 0x01, 0, RmReg),
        Add_rm64_r64 => (true, 0x01, 0, RmReg),
        Add_r32_rm32 => (false, 0x03, 0, RegRm),
        Add_r64_rm64 => (true, 0x03, 0, RegRm),
        Or_rm32_r32 => (false, 0x09, 0, RmReg),
        Or_rm64_r64 => (true, 0x09, 0, RmReg),
        Or_r32_rm32 => (false, 0x0b, 0, RegRm),
        Or_r64_rm64 => (true, 0x0b, 0, RegRm),
        And_rm32_r32 => (false, 0x21, 0, RmReg),
        And_rm64_r64 => (true, 0x21, 0, RmReg),
        And_r32_rm32 => (false, 0x23, 0, RegRm),
        And_r64_rm64 => (true, 0x23, 0, RegRm),
        Sub_rm32_r32 => (false, 0x29, 0, RmReg),
        Sub_rm64_r64 => (true, 0x29, 0, RmReg),
        Sub_r32_rm32 => (false, 0x2b, 0, RegRm),
        Sub_r64_rm64 => (true, 0x2b, 0, RegRm),
        Xor_rm32_r32 => (false, 0x31, 0, RmReg),
        Xor_rm64_r64 => (true, 0x31, 0, RmReg),
        Xor_r32_rm32 => (false, 0x33, 0, RegRm),
        Xor_r64_rm64 => (true, 0x33, 0, RegRm),
        Cmp_rm32_r32 => (false, 0x39, 0, RmReg),
        Cmp_rm64_r64 => (true, 0x39, 0, RmReg),
        Cmp_r32_rm32 => (false, 0x3b, 0, RegRm),
        Cmp_r64_rm64 => (true, 0x3b, 0, RegRm),
        Test_rm32_r32 => (false, 0x85, 0, RmReg),
        Test_rm64_r64 => (true, 0x85, 0, RmReg),
        Add_rm32_imm8 => (false, 0x83, 0, RmImm8),
        Add_rm64_imm8 => (true, 0x83, 0, RmImm8),
        Or_rm32_imm8 => (false, 0x83, 1, RmImm8),
        Or_rm64_imm8 => (true, 0x83, 1, RmImm8),
        And_rm32_imm8 => (false, 0x83, 4, RmImm8),
        And_rm64_imm8 => (true, 0x83, 4, RmImm8),
        Sub_rm32_imm8 => (false, 0x83, 5, RmImm8),
        Sub_rm64_imm8 => (true, 0x83, 5, RmImm8),
        Xor_rm32_imm8 => (false, 0x83, 6, RmImm8),
        Xor_rm64_imm8 => (true, 0x83, 6, RmImm8),
        Cmp_rm32_imm8 => (false, 0x83, 7, RmImm8),
        Cmp_rm64_imm8 => (true, 0x83, 7, RmImm8),
        Add_rm32_imm32 => (false, 0x81, 0, RmImm32),
        Add_rm64_imm32 => (true, 0x81, 0, RmImm32),
        Or_rm32_imm32 => (false, 0x81, 1, RmImm32),
        Or_rm64_imm32 => (true, 0x81, 1, RmImm32),
        And_rm32_imm32 => (false, 0x81, 4, RmImm32),
        And_rm64_imm32 => (true, 0x81, 4, RmImm32),
        Sub_rm32_imm32 => (false, 0x81, 5, RmImm32),
        Sub_rm64_imm32 => (true, 0x81, 5, RmImm32),
        Xor_rm32_imm32 => (false, 0x81, 6, RmImm32),
        Xor_rm64_imm32 => (true, 0x81, 6, RmImm32),
        Cmp_rm32_imm32 => (false, 0x81, 7, RmImm32),
        Cmp_rm64_imm32 => (true, 0x81, 7, RmImm32),
        Add_EAX_imm32 => (false, 0x05, 0, AccImm32),
        Add_RAX_imm32 => (true, 0x05, 0, AccImm32),
        Or_EAX_imm32 => (false, 0x0d, 0, AccImm32),
        Or_RAX_imm32 => (true, 0x0d, 0, AccImm32),
        And_EAX_imm32 => (false, 0x25, 0, AccImm32),
        And_RAX_imm32 => (true, 0x25, 0, AccImm32),
        Sub_EAX_imm32 => (false, 0x2d, 0, AccImm32),
        Sub_RAX_imm32 => (true, 0x2d, 0, AccImm32),
        Xor_EAX_imm32 => (false, 0x35, 0, AccImm32),
        Xor_RAX_imm32 => (true, 0x35, 0, AccImm32),
        Cmp_EAX_imm32 => (false, 0x3d, 0, AccImm32),
        Cmp_RAX_imm32 => (true, 0x3d, 0, AccImm32),
        _ => return None,
    })
}

/// No subregisters, high bytes or address-size overrides enter this encoder.
fn gpr(reg: Register, wide: bool) -> Option<u8> {
    let first = if wide { Register::RAX } else { Register::EAX } as u32;
    let n = (reg as u32).checked_sub(first)?;
    (n < 16).then_some(n as u8)
}

struct Bytes {
    data: [u8; 15],
    len: usize,
}

impl Bytes {
    fn new() -> Self { Self { data: [0; 15], len: 0 } }

    fn append(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.len.checked_add(bytes.len())?;
        self.data.get_mut(self.len..end)?.copy_from_slice(bytes);
        self.len = end;
        Some(())
    }

    fn byte(&mut self, byte: u8) -> Option<()> { self.append(&[byte]) }

    fn rex(&mut self, wide: bool, reg: u8, base: u8) -> Option<()> {
        let rex = 0x40 | (u8::from(wide) << 3) | ((reg >> 3) << 2) | (base >> 3);
        if rex != 0x40 { self.byte(rex)?; }
        Some(())
    }
}

/// Only base64 + signed disp32, without an index, segment or address-size prefix.
/// SIB is emitted for RSP/R12; RBP/R13 require disp8=0 even at zero displacement.
struct Rm {
    base: u8,
    mode: u8,
    displacement: i32,
    displacement_bytes: usize,
}

fn rm(i: &Instruction, operand: u32, wide: bool) -> Option<Rm> {
    match i.op_kind(operand) {
        OpKind::Register => Some(Rm {
            base: gpr(i.op_register(operand), wide)?, mode: 3,
            displacement: 0, displacement_bytes: 0,
        }),
        OpKind::Memory => {
            let base = gpr(i.memory_base(), true)?; // rejects RIP/EIP/no-base/32-bit base
            if i.memory_index() != Register::None || i.memory_index_scale() != 1 { return None; }
            let displacement = i.memory_displacement64() as i64;
            let displacement = i32::try_from(displacement).ok()?;
            let size = match i.memory_displ_size() {
                0 if displacement == 0 => if base & 7 == 5 { 1 } else { 0 },
                1 => if i8::try_from(displacement).is_ok() { 1 } else { 4 },
                // iced uses address_size / 8 as the forced full displacement
                // size, i.e. 8 for base64, but emits a signed disp32 on the wire.
                8 => 4,
                _ => return None,
            };
            Some(Rm { base, mode: match size { 0 => 0, 1 => 1, _ => 2 },
                displacement, displacement_bytes: size })
        }
        _ => None,
    }
}

fn modrm(bytes: &mut Bytes, wide: bool, opcode: u8, reg: u8, rm: Rm) -> Option<()> {
    bytes.rex(wide, reg, rm.base)?;
    bytes.byte(opcode)?;
    bytes.byte((rm.mode << 6) | ((reg & 7) << 3) | (rm.base & 7))?;
    if rm.mode != 3 && rm.base & 7 == 4 {
        // No index (100), scale 1, base RSP/R12. REX.X is deliberately clear.
        bytes.byte(0x24)?;
    }
    bytes.append(&rm.displacement.to_le_bytes()[..rm.displacement_bytes])
}

fn imm32(i: &Instruction, wide: bool) -> Option<u32> {
    match (wide, i.op1_kind()) {
        (false, OpKind::Immediate32) => Some(i.immediate32()),
        (true, OpKind::Immediate32to64) => Some(i.immediate32to64() as u32),
        _ => None,
    }
}

fn encode(i: &Instruction) -> Option<Instruction> {
    // CodeAssembler labels live in Instruction::ip(). Leave labelled instructions
    // entirely to iced; branches and RIP-relative constants miss the allowlist.
    if i.ip() != 0 || i.op_count() != 2 || i.has_lock_prefix() || i.has_rep_prefix()
        || i.has_repne_prefix() || i.has_segment_prefix() || i.has_op_mask()
        || i.zeroing_masking() || i.is_broadcast() || i.suppress_all_exceptions()
        || i.rounding_control() != RoundingControl::None
    {
        return None;
    }
    let (wide, opcode, extension, form) = template(i.code())?;
    let mut bytes = Bytes::new();
    match form {
        Form::RmReg | Form::RegRm => {
            let (rm_operand, reg_operand) = if matches!(form, Form::RmReg) { (0, 1) } else { (1, 0) };
            if i.op_kind(reg_operand) != OpKind::Register { return None; }
            let reg = gpr(i.op_register(reg_operand), wide)?;
            modrm(&mut bytes, wide, opcode, reg, rm(i, rm_operand, wide)?)?;
        }
        Form::RmImm8 => {
            let value = match (wide, i.op1_kind()) {
                (false, OpKind::Immediate8to32) => i.immediate8to32() as u8,
                (true, OpKind::Immediate8to64) => i.immediate8to64() as u8,
                _ => return None,
            };
            modrm(&mut bytes, wide, opcode, extension, rm(i, 0, wide)?)?;
            bytes.byte(value)?;
        }
        Form::RmImm32 => {
            let value = imm32(i, wide)?;
            modrm(&mut bytes, wide, opcode, extension, rm(i, 0, wide)?)?;
            bytes.append(&value.to_le_bytes())?;
        }
        Form::RegImm32 => {
            if i.op0_kind() != OpKind::Register { return None; }
            let reg = gpr(i.op0_register(), wide)?;
            bytes.rex(wide, 0, reg)?;
            bytes.byte(opcode + (reg & 7))?;
            if i.op1_kind() != OpKind::Immediate32 { return None; }
            bytes.append(&i.immediate32().to_le_bytes())?;
        }
        Form::AccImm32 => {
            if i.op0_kind() != OpKind::Register || gpr(i.op0_register(), wide)? != 0 { return None; }
            bytes.rex(wide, 0, 0)?;
            bytes.byte(opcode)?;
            bytes.append(&imm32(i, wide)?.to_le_bytes())?;
        }
    }
    Instruction::with_declare_byte(&bytes.data[..bytes.len]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced_x86::code_asm::*;
    use iced_x86::Encoder;

    fn bytes(i: &Instruction) -> Vec<u8> {
        let mut e = Encoder::new(64);
        e.encode(i, 0).unwrap();
        e.take_buffer()
    }

    #[test]
    fn all_templates_rex_displacements_and_immediates() {
        let mut hits = 0;
        for code in Code::values() {
            let Some((wide, _, _, form)) = template(code) else { continue };
            for r in 0..16u32 {
                let reg = Register::values().nth((if wide { Register::RAX } else { Register::EAX }) as usize + r as usize).unwrap();
                for b in 0..16u32 {
                    let base = Register::values().nth(Register::RAX as usize + b as usize).unwrap();
                    for disp in [i32::MIN, -129, -128, -1, 0, 1, 127, 128, i32::MAX] {
                        for full in [false, true] {
                            let mut i = Instruction::default();
                            i.set_code(code);
                            let rmop = if matches!(form, Form::RegRm) { 1 } else { 0 };
                            i.set_op_kind(rmop, OpKind::Memory);
                            i.set_memory_base(base);
                            i.set_memory_index_scale(1);
                            i.set_memory_displacement64(disp as i64 as u64);
                            i.set_memory_displ_size(if full { 8 } else if disp == 0 { 0 } else { 1 });
                            match form {
                                Form::RmReg | Form::RegRm => {
                                    let n = 1 - rmop; i.set_op_kind(n, OpKind::Register); i.set_op_register(n, reg);
                                }
                                Form::RmImm8 => {
                                    i.set_op1_kind(if wide { OpKind::Immediate8to64 } else { OpKind::Immediate8to32 });
                                    i.set_immediate8(0x80);
                                }
                                Form::RmImm32 => {
                                    i.set_op1_kind(if wide { OpKind::Immediate32to64 } else { OpKind::Immediate32 });
                                    i.set_immediate32(0x8000_0000);
                                }
                                Form::RegImm32 | Form::AccImm32 => {
                                    i.set_op0_kind(OpKind::Register);
                                    i.set_op0_register(if matches!(form, Form::AccImm32) {
                                        if wide { Register::RAX } else { Register::EAX }
                                    } else { reg });
                                    i.set_op1_kind(if wide { OpKind::Immediate32to64 } else { OpKind::Immediate32 });
                                    i.set_immediate32(0x8765_4321);
                                }
                            }
                            let direct = encode(&i).expect("allowlist must be hit, not silently fallback");
                            assert_eq!(bytes(&direct), bytes(&i), "{i:?}");
                            hits += 1;
                            if matches!(form, Form::RmReg | Form::RegRm | Form::RmImm8 | Form::RmImm32) {
                                i.set_op_kind(rmop, OpKind::Register); i.set_op_register(rmop, reg);
                                assert_eq!(bytes(&encode(&i).unwrap()), bytes(&i));
                            }
                        }
                    }
                }
            }
        }
        assert!(hits > 200_000);
        eprintln!("fastenc directed: {hits} memory/immediate cases plus register cases");
    }

    #[test]
    fn mixed_labels_metadata_fallback_and_execution_sensitivity() {
        let has = |name| std::env::var(name).unwrap_or_default().split(',').any(|s| s == "fastenc");
        assert_eq!(enabled(), has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF"));
        let mut a = CodeAssembler::new(64).unwrap();
        let mut target = a.create_label();
        a.mov(eax, 7u32).unwrap();
        a.jmp(target).unwrap();
        a.mov(eax, 99u32).unwrap();
        a.set_label(&mut target).unwrap();
        a.add(eax, 5).unwrap(); // labelled template must stay with iced
        a.ret().unwrap();
        assert!(encode(&a.instructions()[0]).is_some());
        assert!(encode(a.instructions().iter().find(|i| i.ip() != 0).unwrap()).is_none());
        let options = BlockEncoderOptions::RETURN_NEW_INSTRUCTION_OFFSETS;
        for base in [0, 0x1000, 0x7fff_0000_0000] {
            let expected = a.assemble_options(base, options).unwrap();
            let actual = assemble(&mut a, base, options).unwrap();
            assert_eq!(actual.inner.code_buffer, expected.inner.code_buffer);
            assert_eq!(actual.label_ip(&target).unwrap(), expected.label_ip(&target).unwrap());
            assert_eq!(actual.inner.new_instruction_offsets, expected.inner.new_instruction_offsets);
            let opts = options | BlockEncoderOptions::RETURN_CONSTANT_OFFSETS | BlockEncoderOptions::RETURN_RELOC_INFOS;
            let fallback = assemble(&mut a, base, opts).unwrap();
            assert_eq!(fallback.inner.code_buffer, a.assemble_options(base, opts).unwrap().inner.code_buffer);
        }
        let result = assemble(&mut a, 0, options).unwrap();
        let execute = |code: &[u8]| unsafe {
            let p = libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANON, -1, 0);
            assert_ne!(p, libc::MAP_FAILED);
            std::ptr::copy_nonoverlapping(code.as_ptr(), p as *mut u8, code.len());
            let f: extern "C" fn() -> u32 = std::mem::transmute(p);
            let value = f(); libc::munmap(p, 4096); value
        };
        assert_eq!(execute(&result.inner.code_buffer), 12);
        // Deliberately corrupt the directly encoded immediate: the execution
        // oracle must detect this, rather than accepting a fallback-only test.
        let mut broken = result.inner.code_buffer.clone();
        assert_eq!(broken[0], 0xb8); broken[1] = 8;
        assert_eq!(execute(&broken), 13);
        assert_ne!(execute(&broken), 12);
        eprintln!("fastenc sensitivity: corrupt imm32 => 13, oracle expected 12 (detected)");
        let mut unsupported = CodeAssembler::new(64).unwrap();
        unsupported.mov(rax, 0x123456789abcdef0u64).unwrap();
        unsupported.mov(rax, qword_ptr(0x1234u64)).unwrap();
        assert!(unsupported.instructions().iter().all(|i| encode(i).is_none()));
    }
}
