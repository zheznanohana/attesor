//! Bounded lazy-NZCV plan for one private loopra backedge.
//!
//! This is not a live host-flags cache. Only a final CMP-immediate followed by
//! CBZ/TBZ can defer its Cpu stores: the private edge discards them, and the
//! public edge recomputes the comparison from unchanged operands. Every source
//! word examined is inside the block the caller is currently emitting.

#[derive(Clone, Copy)]
pub(super) struct ExitCompare {
    pub sf: bool,
    pub rn: u32,
    pub imm: i32,
}

/// Exact opt-in; OFF wins even when ON contains the same token.
pub(crate) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let contains = |name| std::env::var(name).unwrap_or_default().split(',').any(|s| s == "flagreg");
        contains("ATTESOR_JIT_ON") && !contains("ATTESOR_JIT_OFF")
    })
}

/// The caller must already have a private loopra entry for `entry`, native
/// instruction classes enabled, and at least two remaining instruction slots.
/// `public_entry` excludes interior public labels (including the branch).
/// Accept only a complete, helper-free integer block. In particular no FP word
/// can introduce an entry guard that exits before the next iteration kills NZCV.
pub(super) fn exit_compare(
    entry: u64,
    setter: u64,
    max_insns: usize,
    public_entry: &dyn Fn(u64) -> bool,
    read: &dyn Fn(u64) -> Option<u32>,
) -> Option<ExitCompare> {
    let bytes = setter.checked_sub(entry)?;
    if bytes % 4 != 0 || bytes / 4 + 2 > max_insns as u64 { return None; }
    let w = read(setter)?;
    // CMP Wn/Xn, #imm{, LSL #12}; do not include SP, CMN or reserved encodings.
    if w & 0x7f80_001f != 0x7100_001f || (w >> 5) & 31 == 31 { return None; }
    let branch_pc = setter.checked_add(4)?;
    if public_entry(branch_pc) { return None; }
    let branch = read(branch_pc)?;
    let (imm, width) = if branch & 0x7e00_0000 == 0x3400_0000 {
        ((branch >> 5) & 0x7ffff, 19)
    } else if branch & 0x7e00_0000 == 0x3600_0000 {
        ((branch >> 5) & 0x3fff, 14)
    } else { return None; };
    let displacement = (((imm as i64) << (64 - width)) >> (64 - width)) << 2;
    if branch_pc.wrapping_add(displacement as u64) != entry { return None; }
    // All instructions before the deferred CMP are flag-independent and native.
    // A complete writer is reached no later than `setter` on every iteration;
    // there is no observer, helper, branch or side exit on the way there.
    let mut pc = entry;
    while pc < setter {
        if pc != entry && public_entry(pc) { return None; }
        let p = read(pc)?;
        let add_sub_imm = p & 0x1f80_0000 == 0x1100_0000;
        let mov_wide = p & 0x1f80_0000 == 0x1280_0000
            && (p >> 29) & 3 != 1
            && (p >> 31 != 0 || (p >> 21) & 3 < 2);
        if p != 0xd503_201f && !add_sub_imm && !mov_wide { return None; }
        pc += 4;
    }
    if setter != entry && public_entry(setter) { return None; }
    Some(ExitCompare {
        sf: w >> 31 != 0,
        rn: (w >> 5) & 31,
        imm: (((w >> 10) & 0xfff) << if w & (1 << 22) != 0 { 12 } else { 0 }) as i32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn g4_flagreg_proof_barriers_and_bounds() {
        let entry=0x1000;
        let check=|words:&[u32],max,public:Option<u64>|exit_compare(entry,entry+4,max,&|p|Some(p)==public,&|p|p.checked_sub(entry).and_then(|o|words.get((o/4) as usize)).copied());
        assert!(check(&[0xd1000442,0xf100041f,0xb5ffffc2],96,None).is_some());
        assert!(check(&[0xd1000442,0xf100041f,0xb5ffffc2],2,None).is_none());
        for public in [entry+4,entry+8] { assert!(check(&[0xd1000442,0xf100041f,0xb5ffffc2],96,Some(public)).is_none()); }
        for prefix in [0x9a020000,0xd53b4200,0xd51b4200,0xf9400000,0x1e612800,0x54000000,0x92800000 | (1<<29),0x12c00000] {
            assert!(check(&[prefix,0xf100041f,0xb5ffffc2],96,None).is_none(),"{prefix:x}");
        }
        assert!(check(&[0xd1000442,0xf10007ff,0xb5ffffc2],96,None).is_none()); // SP
        assert!(check(&[0xd1000442,0xf100041f,0xb5ffffe2],96,None).is_none()); // wrong backedge
    }
}
