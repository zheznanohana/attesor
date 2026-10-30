//! Deliberately small, block-local proof domain for redundant TBI stripping.
//! Facts describe guest X registers, never host scratch registers. Unknown
//! instructions kill all facts; callers must also kill on entries and helpers.

const TOP_BYTE_MASK: u64 = 0x00ff_ffff_ffff_ffff;

pub(super) struct Facts {
    // SP/XZR is deliberately absent: register 31 is never a known address.
    x: [Option<u64>; 31],
}

struct Memory {
    rn: usize,
    rt: usize,
    offset: i64,
    load: bool,
    bytes: u64,
}

impl Facts {
    pub(super) fn new() -> Self {
        Self { x: [None; 31] }
    }

    pub(super) fn clear(&mut self) {
        self.x.fill(None);
    }

    /// Prove the *complete* effective address, including its immediate offset.
    /// Reject tagged bases, unsigned overflow, signed underflow, and accesses
    /// crossing bit 56. There is no signal-handler repair or runtime assumption.
    pub(super) fn untagged_address(&self, insn: u32) -> bool {
        let Some(m) = memory(insn) else { return false };
        let Some(base) = self.x.get(m.rn).copied().flatten() else { return false };
        if base > TOP_BYTE_MASK { return false; }
        let Some(address) = add_signed(base, m.offset) else { return false };
        address.checked_add(m.bytes - 1).is_some_and(|end| end <= TOP_BYTE_MASK)
    }

    /// Advance only after successful, unconditional native emission without a
    /// helper, fused successor, or label. The caller enforces those barriers.
    pub(super) fn advance(&mut self, insn: u32, pc: u64) {
        let rd = (insn & 31) as usize;
        let sf = insn >> 31 != 0;
        let value = if insn & 0x7f80_0000 == 0x5280_0000 {
            // MOVZ (not MOVK/MOVN); W writes clear the upper 32 bits.
            let hw = (insn >> 21) & 3;
            if !sf && hw > 1 { self.clear(); return; }
            Some((((insn >> 5) & 0xffff) as u64) << (16 * hw))
        } else if insn & 0x1f00_0000 == 0x1000_0000 {
            // ADR/ADRP: signed imm21; compute without wrapping a proof.
            let raw = (((insn >> 5) & 0x7ffff) << 2) | ((insn >> 29) & 3);
            let imm = ((raw as i64) << 43) >> 43;
            let (base, offset) = if sf { (pc & !0xfff, imm << 12) } else { (pc, imm) };
            add_signed(base, offset)
        } else if insn & 0x1f80_0000 == 0x1100_0000 {
            // ADD/SUB immediate (+ optional NZCV); SP input/output is excluded.
            let rn = ((insn >> 5) & 31) as usize;
            let imm = (((insn >> 10) & 0xfff) as i64) << (if insn & (1 << 22) != 0 { 12 } else { 0 });
            let offset = if insn & (1 << 30) != 0 { -imm } else { imm };
            self.x.get(rn).copied().flatten().and_then(|v| {
                let v = if sf { v } else { v & 0xffff_ffff };
                add_signed(v, offset).filter(|&n| sf || n <= u32::MAX as u64)
            })
        } else if let Some(m) = memory(insn) {
            // No writeback. A load invalidates its destination before the next
            // instruction; signed loads do not establish an untagged fact.
            if m.load && m.rt < 31 { self.x[m.rt] = None; }
            return;
        } else {
            // Includes partial writes, register offsets, pair/SIMD, atomics,
            // system instructions, control flow and every unaudited encoding.
            self.clear();
            return;
        };
        if rd < 31 { self.x[rd] = value; } else { self.clear(); }
    }
}

fn add_signed(base: u64, offset: i64) -> Option<u64> {
    if offset < 0 { base.checked_sub(offset.unsigned_abs()) }
    else { base.checked_add(offset as u64) }
}

/// Ordinary scalar unsigned-immediate or unscaled-immediate loads/stores only.
/// No SP, writeback, LDTR/STTR, prefetch, pair, SIMD, atomic or register offset.
fn memory(insn: u32) -> Option<Memory> {
    if insn & (1 << 26) != 0 { return None; }
    let size = (insn >> 30) & 3;
    let opc = (insn >> 22) & 3;
    if (opc == 2 && size == 3) || (opc == 3 && size >= 2) { return None; }
    let rn = ((insn >> 5) & 31) as usize;
    if rn == 31 { return None; }
    let offset = if insn & 0x3b00_0000 == 0x3900_0000 {
        (((insn >> 10) & 0xfff) as i64) << size
    } else if insn & 0x3b20_0c00 == 0x3800_0000 {
        let raw = ((insn >> 12) & 0x1ff) as i64;
        (raw << 55) >> 55
    } else { return None };
    Some(Memory { rn, rt: (insn & 31) as usize, offset, load: opc != 0, bytes: 1 << size })
}

#[cfg(test)]
mod tests {
    use super::*;
    const LDR: u32 = 0xf9400020; // LDR X0,[X1]
    fn movz(rd: u32, imm: u32, hw: u32, sf: bool) -> u32 {
        0x52800000 | ((sf as u32) << 31) | (hw << 21) | (imm << 5) | rd
    }
    #[test]
    fn g3_notbi_boundaries_decode_and_facts() {
        let mut f = Facts::new();
        for (base, word, expected) in [
            (0, LDR, true), (TOP_BYTE_MASK - 7, LDR, true),
            (TOP_BYTE_MASK - 6, LDR, false), // final-byte crossing, not just base
            (TOP_BYTE_MASK, LDR | (1 << 10), false),
            (1 << 56, LDR, false), (u64::MAX, LDR, false),
            (0, 0xf85ff020, false), // LDUR X0,[X1,#-1], underflow
            (8, 0xf85f8020, true),  // LDUR X0,[X1,#-8]
            (8, 0xf8400420, false), // post-index
            (8, 0xf8400820, false), // LDTR
            (8, 0x3dc00020, false), // SIMD
            (8, 0xa9400020, false), // pair
            (8, 0xf8626820, false), // register offset
            (8, 0xf9800020, false), // prefetch
            (8, 0xf94003e0, false), // SP
        ] {
            f.x[1] = Some(base);
            assert_eq!(f.untagged_address(word), expected, "base={base:#x} word={word:08x}");
        }
        assert_eq!(add_signed(0, -1), None); assert_eq!(add_signed(u64::MAX, 1), None);
        assert_eq!(add_signed(0, i64::MIN), None);
        for sf in [false, true] {
            for hw in 0..if sf { 4 } else { 2 } {
                f.advance(movz(1, 0x1234, hw, sf), 0);
                assert_eq!(f.x[1], Some(0x1234 << (16 * hw)));
            }
        }
        f.advance(movz(1, 8, 0, true), 0);
        f.advance(0x91002021, 0); assert_eq!(f.x[1], Some(16)); // ADD X1,#8
        f.advance(0xd1002021, 0); assert_eq!(f.x[1], Some(8));  // SUB X1,#8
        f.advance(0xd1002421, 0); assert_eq!(f.x[1], None);     // underflow
        f.x[1] = Some(0xffff_ffff); f.advance(0x11000421, 0); assert_eq!(f.x[1], None);
        f.x[1] = Some(0xab00_0000_0000_0008); f.advance(0x11000421, 0); assert_eq!(f.x[1], Some(9));
        f.advance(0x10000081, 0x1000); assert_eq!(f.x[1], Some(0x1010)); // ADR +16
        f.advance(0x10ffff81, 0x1000); assert_eq!(f.x[1], Some(0xff0));  // ADR -16
        f.advance(0xb0000001, 0x1234); assert_eq!(f.x[1], Some(0x2000)); // ADRP +4096
        f.advance(0xf0ffffe1, 0x1234); assert_eq!(f.x[1], Some(0));      // ADRP -4096
        f.x[1] = Some(8); f.advance(0xf9400021, 0); assert_eq!(f.x[1], None); // load alias
        for word in [0xf2800021, 0x92800001, 0xd503201f, 0xf8008420] { // MOVK/MOVN/unknown/writeback
            f.x[1] = Some(8); f.advance(word, 0); assert!(f.x.iter().all(Option::is_none));
        }
    }
}
