//! Immutable, publish-time rel32 links. There is deliberately no hot-patch API.
//!
//! See docs/REL32-CHAIN.md for the publication graph and revocation protocol.

use attesor_core::cpu::off;
use iced_x86::code_asm::*;
use std::sync::atomic::{compiler_fence, Ordering};
use std::sync::OnceLock;

/// Exact opt-in tokens; OFF wins. Never inherits another chaining option.
pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let has = |name| std::env::var(name).unwrap_or_default().split(',').any(|s| s == "rel32chain");
        has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF")
    })
}

/// The separate entry used by immutable incoming rel32 edges. Its data cell is separately allocated and permanent;
/// only that cell, never these instructions, is revoked after publication.
/// Scratch registers/flags are not guest state at a canonical public entry.
pub(super) fn emit_guard(a: &mut CodeAssembler, body: CodeLabel, pc: u64, alive: u64) {
    let mut reject = a.create_label();
    a.mov(rax, qword_ptr(r15 + off::EPOCH_PTR)).unwrap();
    a.mov(rax, qword_ptr(rax)).unwrap();
    a.cmp(rax, qword_ptr(r15 + off::ENTRY_EPOCH)).unwrap();
    a.jne(reject).unwrap();
    a.mov(rax, alive).unwrap();
    a.cmp(qword_ptr(rax), 0).unwrap();
    a.jne(body).unwrap();
    a.set_label(&mut reject).unwrap();
    a.mov(rax, pc).unwrap();
    a.mov(qword_ptr(r15 + off::PC), rax).unwrap();
    a.mov(qword_ptr(r15 + off::EXIT_DATA), 0).unwrap();
    a.mov(qword_ptr(r15 + off::EXIT_REASON), super::emit::EXIT_CONTINUE as i32).unwrap();
    a.jmp(qword_ptr(r15 + off::JIT_EXIT)).unwrap();
}

/// Replace an FF /4 RIP-relative six-byte jump in an UNPUBLISHED Vec only.
/// `target` must already be published and have a permanent liveness guard.
/// Range/shape failures retain the original indirect dispatcher fallback.
pub(super) fn link_unpublished(bytes: &mut [u8], base: u64, site: u64, target: u64) -> bool {
    let Some(offset) = site.checked_sub(base).and_then(|n| usize::try_from(n).ok()) else { return false };
    let Some(end) = offset.checked_add(6) else { return false };
    let Some(jump) = bytes.get_mut(offset..end) else { return false };
    if jump[..2] != [0xff, 0x25] {
        return false;
    }
    let displacement = i128::from(target) - (i128::from(site) + 5);
    let Ok(displacement) = i32::try_from(displacement) else { return false };
    jump[0] = 0xe9;
    jump[1..5].copy_from_slice(&displacement.to_le_bytes());
    jump[5] = 0x90;
    true
}

/// The execution-side serializing instruction follows acquisition of the code
/// pointer. Compiler fences prevent motion across this architectural boundary.
/// Runtime slot publication requires an exclusive graph lease at quiescence.
/// Each execution holds a read lease across this barrier and the full run, so
/// its reachable graph cannot acquire newer code behind the barrier. Immutable
/// rel32 edges themselves still lead only to older published guarded units.
pub(super) fn serialize_before_entry() {
    compiler_fence(Ordering::SeqCst);
    std::arch::x86_64::__cpuid(0);
    compiler_fence(Ordering::SeqCst);
}
