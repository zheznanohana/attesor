//! Guest AArch64 register state.
//!
//! The layout is `#[repr(C)]` and stable because JIT/AOT generated host code
//! addresses fields directly relative to a pinned host register.

use std::mem::offset_of;
use std::sync::atomic::{AtomicU64, Ordering};

/// Number of host helper slots reachable from generated code (`call [r15+..]`).
pub const HELPER_SLOTS: usize = 32;

#[repr(C, align(64))]
pub struct Cpu {
    /// X0..X30; index 31 holds SP (XZR is never stored).
    pub x: [u64; 32],
    /// Aligned atomic PC, also stored by existing x86 MOVs in generated code.
    pub pc: AtomicU64,
    /// NZCV flags, one byte each (0 or 1).
    pub nf: u8,
    pub zf: u8,
    pub cf: u8,
    pub vf: u8,
    pub _pad0: u32,
    /// V0..V31, little-endian 128-bit (lo, hi).
    pub v: [[u64; 2]; 32],
    pub fpcr: u64,
    pub fpsr: u64,
    pub tpidr_el0: u64,
    pub tpidrro_el0: u64,
    /// Exclusive monitor emulation (LDXR/STXR): address, observed values, size.
    pub excl_addr: u64,
    pub excl_val: u64,
    pub excl_val2: u64,
    pub excl_size: u64,

    // ---- runtime/JIT plumbing (not architectural) ----
    /// Why generated code returned to the dispatcher.
    pub exit_reason: u64,
    /// Auxiliary exit data (e.g. SVC immediate, faulting instruction).
    pub exit_data: u64,
    /// Host address of the dispatcher-return trampoline.
    pub jit_exit: u64,
    /// Host pointer to the indirect-branch lookup table.
    pub ibtc: u64,
    /// Host pointer to the owning runtime thread object.
    pub thread: u64,
    /// Instruction budget used to bound time spent in chained code.
    pub budget: u64,
    /// Host stack pointer when generated code was entered; the exit trampoline restores it, which
    /// discards whatever host return addresses guest calls (BL -> host call) left behind.
    pub host_sp: u64,
    /// Host call frames pushed by translated BL/BLR still open (bounded; 0 after every entry).
    pub ras_depth: u64,
    /// Reserved legacy CALLRET scratch (retained to preserve Cpu offsets).
    pub ret_target: u64,
    /// Host pointer to the translation epoch (bumped by every invalidation).
    pub epoch_ptr: u64,
    /// Host function pointers called by generated code.
    pub helpers: [u64; HELPER_SLOTS],
    /// Sampling state, written only at Rust transitions. Appended to preserve all JIT offsets.
    pub profile_state: AtomicU64,
    /// Stats-only public entries into write-back regions from disk-loaded AOT code.
    /// Generated code uses r15-relative addressing so cache blobs stay relocatable.
    pub aot_loop_regions: u64,
    /// Translation epoch sampled by jit_enter, shared by all open host frames.
    pub entry_epoch: u64,
}

pub mod off {
    use super::*;
    pub const X: usize = offset_of!(Cpu, x);
    pub const PC: usize = offset_of!(Cpu, pc);
    pub const NF: usize = offset_of!(Cpu, nf);
    pub const ZF: usize = offset_of!(Cpu, zf);
    pub const CF: usize = offset_of!(Cpu, cf);
    pub const VF: usize = offset_of!(Cpu, vf);
    pub const V: usize = offset_of!(Cpu, v);
    pub const FPCR: usize = offset_of!(Cpu, fpcr);
    pub const FPSR: usize = offset_of!(Cpu, fpsr);
    pub const TPIDR: usize = offset_of!(Cpu, tpidr_el0);
    pub const TPIDRRO: usize = offset_of!(Cpu, tpidrro_el0);
    pub const EXCL_ADDR: usize = offset_of!(Cpu, excl_addr);
    pub const EXCL_VAL: usize = offset_of!(Cpu, excl_val);
    pub const EXCL_VAL2: usize = offset_of!(Cpu, excl_val2);
    pub const EXCL_SIZE: usize = offset_of!(Cpu, excl_size);
    pub const EXIT_REASON: usize = offset_of!(Cpu, exit_reason);
    pub const EXIT_DATA: usize = offset_of!(Cpu, exit_data);
    pub const JIT_EXIT: usize = offset_of!(Cpu, jit_exit);
    pub const IBTC: usize = offset_of!(Cpu, ibtc);
    pub const BUDGET: usize = offset_of!(Cpu, budget);
    pub const HOST_SP: usize = offset_of!(Cpu, host_sp);
    pub const RAS_DEPTH: usize = offset_of!(Cpu, ras_depth);
    pub const RET_TARGET: usize = offset_of!(Cpu, ret_target);
    pub const EPOCH_PTR: usize = offset_of!(Cpu, epoch_ptr);
    pub const HELPERS: usize = offset_of!(Cpu, helpers);
    pub const ENTRY_EPOCH: usize = offset_of!(Cpu, entry_epoch);
    pub const AOT_LOOP_REGIONS: usize = offset_of!(Cpu, aot_loop_regions);
    pub const fn xreg(n: usize) -> usize {
        X + n * 8
    }
    pub const fn vreg(n: usize) -> usize {
        V + n * 16
    }
    pub const fn helper(n: usize) -> usize {
        HELPERS + n * 8
    }
}

impl Default for Cpu {
    fn default() -> Self {
        // SAFETY: all-zero is a valid Cpu.
        unsafe { std::mem::zeroed() }
    }
}

impl Cpu {
    #[inline(always)]
    pub fn pc(&self) -> u64 { self.pc.load(Ordering::Relaxed) }
    #[inline(always)]
    pub fn set_pc(&self, pc: u64) { self.pc.store(pc, Ordering::Relaxed); }

    pub fn new() -> Box<Cpu> {
        Box::new(Cpu::default())
    }

    /// Read Xn where 31 means XZR.
    #[inline(always)]
    pub fn xr(&self, n: u32) -> u64 {
        if n == 31 {
            0
        } else {
            self.x[n as usize]
        }
    }
    /// Read Xn where 31 means SP.
    #[inline(always)]
    pub fn xs(&self, n: u32) -> u64 {
        self.x[n as usize]
    }
    /// Write Xn where 31 means XZR (discard).
    #[inline(always)]
    pub fn setx(&mut self, n: u32, val: u64) {
        if n != 31 {
            self.x[n as usize] = val;
        }
    }
    /// Write Xn where 31 means SP.
    #[inline(always)]
    pub fn setxs(&mut self, n: u32, val: u64) {
        self.x[n as usize] = val;
    }
    #[inline(always)]
    pub fn sp(&self) -> u64 {
        self.x[31]
    }

    #[inline(always)]
    pub fn nzcv(&self) -> u32 {
        ((self.nf as u32) << 3) | ((self.zf as u32) << 2) | ((self.cf as u32) << 1) | self.vf as u32
    }
    #[inline(always)]
    pub fn set_nzcv(&mut self, f: u32) {
        self.nf = ((f >> 3) & 1) as u8;
        self.zf = ((f >> 2) & 1) as u8;
        self.cf = ((f >> 1) & 1) as u8;
        self.vf = (f & 1) as u8;
    }

    /// Evaluate an AArch64 condition code.
    #[inline(always)]
    pub fn cond(&self, cond: u32) -> bool {
        let r = match cond >> 1 {
            0 => self.zf != 0,
            1 => self.cf != 0,
            2 => self.nf != 0,
            3 => self.vf != 0,
            4 => self.cf != 0 && self.zf == 0,
            5 => self.nf == self.vf,
            6 => self.nf == self.vf && self.zf == 0,
            _ => true,
        };
        if cond & 1 != 0 && cond != 0xf {
            !r
        } else {
            r
        }
    }

    #[inline(always)]
    pub fn vq(&self, n: u32) -> u128 {
        let v = self.v[n as usize];
        (v[0] as u128) | ((v[1] as u128) << 64)
    }
    #[inline(always)]
    pub fn set_vq(&mut self, n: u32, val: u128) {
        self.v[n as usize] = [val as u64, (val >> 64) as u64];
    }
    #[inline(always)]
    pub fn vd(&self, n: u32) -> u64 {
        self.v[n as usize][0]
    }
    /// Scalar write: zero the upper bits of the vector register.
    #[inline(always)]
    pub fn set_vd(&mut self, n: u32, val: u64) {
        self.v[n as usize] = [val, 0];
    }

    pub fn dump(&self) -> String {
        let mut s = String::new();
        for i in 0..31 {
            s += &format!("x{:<2}={:016x}{}", i, self.x[i], if i % 4 == 3 { "\n" } else { "  " });
        }
        s += &format!(" sp={:016x}\n pc={:016x}  nzcv={}{}{}{}  tpidrro={:x}\n",
            self.x[31], self.pc(),
            if self.nf != 0 { 'N' } else { '-' },
            if self.zf != 0 { 'Z' } else { '-' },
            if self.cf != 0 { 'C' } else { '-' },
            if self.vf != 0 { 'V' } else { '-' },
            self.tpidrro_el0);
        s
    }
}

#[cfg(test)]
mod profile_layout_tests {
    use super::*;
    #[test]
    fn atomic_pc_preserves_jit_layout() {
        assert_eq!(off::PC, 256);
        assert_eq!(off::NF, 264);
        assert_eq!(off::V, 272);
        assert_eq!(off::HELPERS, 928);
        assert_eq!(std::mem::size_of::<AtomicU64>(), 8);
        assert!(off::PC % std::mem::align_of::<AtomicU64>() == 0);
        assert!(offset_of!(Cpu, profile_state) >= off::HELPERS + HELPER_SLOTS * 8);
    }
}
