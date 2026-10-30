//! Host backing for the shared JIT/AOT code cache.
//!
//! Only the exact `hugepages` token opts in. A large or aligned ordinary mmap
//! is not a superpage allocation; use Darwin's explicit 2 MiB request instead.

use std::ffi::c_void;
use std::sync::OnceLock;

pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let has_token = |name| {
            std::env::var(name).unwrap_or_default().split(',').any(|v| v == "hugepages")
        };
        !has_token("ATTESOR_JIT_OFF") && has_token("ATTESOR_JIT_ON")
    })
}

/// Preserve the existing code-cache permissions and mmap fallback. This is
/// host-only backing: neither allocation path maps at a guest address.
pub(super) fn allocate(len: usize) -> *mut c_void {
    if enabled() {
        #[cfg(target_os = "macos")]
        if let Some(p) = darwin::allocate(len) {
            return p;
        }
        #[cfg(not(target_os = "macos"))]
        eprintln!("[attesor] hugepages: Darwin 2 MiB API unavailable; using ordinary mmap");
    }
    unsafe {
        libc::mmap(std::ptr::null_mut(), len,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0)
    }
}

#[cfg(target_os = "macos")]
mod darwin {
    use super::c_void;

    const SUPERPAGE_SIZE: u64 = 2 * 1024 * 1024;
    const CACHE_PROTECTION: libc::vm_prot_t =
        libc::VM_PROT_READ | libc::VM_PROT_WRITE | libc::VM_PROT_EXECUTE;

    // Signatures: macOS SDK mach/mach_vm.h. mach_vm_address_t and
    // mach_vm_size_t are 64-bit even on targets with smaller pointer types.
    extern "C" {
        fn mach_vm_allocate(task: libc::mach_port_t, address: *mut u64,
            size: u64, flags: libc::c_int) -> libc::kern_return_t;
        fn mach_vm_protect(task: libc::mach_port_t, address: u64, size: u64,
            set_maximum: libc::boolean_t, protection: libc::vm_prot_t) -> libc::kern_return_t;
        fn mach_vm_deallocate(task: libc::mach_port_t, address: u64,
            size: u64) -> libc::kern_return_t;
    }

    /// An unpublished mapping must be removed before attempting mmap fallback.
    /// A cleanup failure is fatal rather than silently leaking wired memory.
    struct Pending {
        task: libc::mach_port_t,
        base: u64,
        len: u64,
        release: unsafe extern "C" fn(libc::mach_port_t, u64, u64) -> i32,
    }

    impl Drop for Pending {
        fn drop(&mut self) {
            let kr = unsafe { (self.release)(self.task, self.base, self.len) };
            assert_eq!(kr, 0, "cannot release rejected JIT superpage allocation (Mach {kr})");
        }
    }

    pub(super) fn allocate(len: usize) -> Option<*mut c_void> {
        allocate_with(len, unsafe { libc::mach_task_self() }, mach_vm_allocate, mach_vm_protect, mach_vm_deallocate)
    }

    fn allocate_with(
        len: usize, task: libc::mach_port_t,
        request: unsafe extern "C" fn(libc::mach_port_t, *mut u64, u64, i32) -> i32,
        protect: unsafe extern "C" fn(libc::mach_port_t, u64, u64, libc::boolean_t, libc::vm_prot_t) -> i32,
        release: unsafe extern "C" fn(libc::mach_port_t, u64, u64) -> i32,
    ) -> Option<*mut c_void> {
        let len = len as u64;
        // Do not round or partially back the arena: the code/slot collision
        // bounds must describe exactly the mapping returned to CodeCache.
        if len == 0 || len % SUPERPAGE_SIZE != 0 {
            eprintln!("[attesor] hugepages: cache length is not a nonzero 2 MiB multiple; using ordinary mmap");
            return None;
        }
        let mut base = 0;
        // SDK mach/vm_statistics.h: VM_FLAGS_SUPERPAGE_SIZE_2MB = 2 << 16.
        // SIZE_ANY and address alignment alone do not provide this guarantee.
        let kr = unsafe { request(task, &mut base, len,
            libc::VM_FLAGS_ANYWHERE | libc::VM_FLAGS_SUPERPAGE_SIZE_2MB) };
        if kr != 0 {
            eprintln!("[attesor] hugepages: 2 MiB allocation rejected (Mach {kr}); using ordinary mmap");
            return None;
        }
        let pending = Pending { task, base, len, release };
        if base == 0 || base % SUPERPAGE_SIZE != 0 || base.checked_add(len).is_none() {
            eprintln!("[attesor] hugepages: invalid allocation alignment/range; using ordinary mmap");
            return None; // pending deallocates before the caller can fall back
        }
        // mach_vm_allocate starts RW. Match, never relax, the ordinary arena's
        // RWX policy; no entitlement, MAP_JIT, W^X toggle or remapping bypass.
        // If execute permission is denied, discard the entire mapping.
        let kr = unsafe { protect(task, base, len, 0, CACHE_PROTECTION) };
        if kr != 0 {
            eprintln!("[attesor] hugepages: code-cache protection rejected (Mach {kr}); using ordinary mmap");
            return None;
        }
        std::mem::forget(pending); // process-lifetime cache, just like mmap
        eprintln!("[attesor] hugepages: Darwin accepted 2 MiB code-cache request ({len} bytes)");
        Some(base as *mut c_void)
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static RELEASED: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn request(_: u32, p: *mut u64, n: u64, flags: i32) -> i32 {
            assert_eq!(n, SUPERPAGE_SIZE);
            assert_eq!(flags, libc::VM_FLAGS_ANYWHERE | libc::VM_FLAGS_SUPERPAGE_SIZE_2MB);
            *p = SUPERPAGE_SIZE; 0
        }
        unsafe extern "C" fn reject(_: u32, _: u64, _: u64, _: u32, prot: i32) -> i32 {
            assert_eq!(prot, CACHE_PROTECTION); 2
        }
        unsafe extern "C" fn release(_: u32, p: u64, n: u64) -> i32 {
            assert_eq!((p,n),(SUPERPAGE_SIZE,SUPERPAGE_SIZE));
            RELEASED.fetch_add(1, Ordering::SeqCst); 0
        }
        #[test]
        fn g4_hugepages_protection_failure_releases_before_fallback() {
            assert!(allocate_with(SUPERPAGE_SIZE as usize, 1, request, reject, release).is_none());
            assert_eq!(RELEASED.load(Ordering::SeqCst), 1);
        }
    }

}

#[cfg(test)]
mod tests {
    #[test]
    fn g4_hugepages_policy_and_fallback() {
        let has = |k| std::env::var(k).unwrap_or_default().split(',').any(|t| t == "hugepages");
        assert_eq!(super::enabled(), has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF"));
        // Invalid superpage size must fall back to a usable ordinary RWX mapping.
        // Never request the production 4 GiB wired arena in this allocator test.
        let p = super::allocate(4096);
        assert_ne!(p, libc::MAP_FAILED);
        unsafe {
            std::ptr::copy_nonoverlapping([0xb8u8, 42, 0, 0, 0, 0xc3].as_ptr(), p.cast(), 6);
            let f: extern "C" fn() -> u32 = std::mem::transmute(p);
            assert_eq!(f(), 42);
            assert_eq!(libc::munmap(p, 4096), 0);
        }
    }
}
