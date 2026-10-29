//! Real Darwin mappings plus scripted host results for otherwise nondeterministic paths.
use super::*;
use std::sync::atomic::AtomicU64;

const PAGE: u64 = 0x4000;
const FIXED: u64 = libc::MAP_FIXED as u64;
const ANON: u64 = (libc::MAP_ANON | libc::MAP_PRIVATE) as u64;
fn ok(addr: u64) -> SysResult { SysResult { rax: addr, rdx: 17, carry: false } }
fn error(errno: i32) -> SysResult { SysResult { rax: errno as u64, rdx: 0, carry: true } }

#[test]
fn first_try_preserves_hint_flags_offset_and_returns_after_one_call() {
    let args = [0x10000, 16385, 3, libc::MAP_SHARED as u64, 7, PAGE, 91, 92];
    let mut expected = args;
    expected[1] = 2 * PAGE;
    let mut calls = 0;
    let r = mmap_16k_with(&args, &AtomicU64::new(0), |n, a| {
        calls += 1;
        assert_eq!(n, SYS_MMAP);
        assert_eq!(*a, expected);
        ok(0x80000)
    });
    assert_eq!((calls, r.rax, r.rdx, r.carry), (1, 0x80000, 17, false));
}

#[test]
fn aligned_end_hint_is_optional_nonfixed_and_updated_only_on_success() {
    let hint = AtomicU64::new(0x80000);
    for (caller_hint, want_hint, result, next) in [
        (0, 0x80000, ok(0x80000), 0x88000),
        (0, 0x88000, ok(0x90000), 0x98000), // host can ignore a stale hint
        (0x40000, 0x40000, ok(0x40000), 0x48000),
        (0, 0x48000, error(libc::ENOMEM), 0x48000),
    ] {
        mmap_16k_with(&[caller_hint, PAGE + 1, 3, ANON, u64::MAX, 0, 0, 0], &hint, |n, a| {
            assert_eq!(n, SYS_MMAP);
            assert_eq!(a[0], want_hint);
            assert_eq!(a[1], 2 * PAGE);
            result
        });
        assert_eq!(hint.load(Ordering::Relaxed), next);
    }
    for args in [[0x11000, 1, 3, ANON | FIXED, u64::MAX, 0, 0, 0],
                 [0, 0, 3, ANON, u64::MAX, 0, 0, 0]] {
        mmap_16k_with(&args, &hint, |n, a| {
            assert_eq!((n, *a), (SYS_MMAP, args));
            ok(args[0])
        });
        assert_eq!(hint.load(Ordering::Relaxed), 0x48000);
    }
}

#[test]
fn zero_length_and_fixed_are_forwarded_unchanged() {
    for args in [[0x11000, 4097, 3, ANON | FIXED, u64::MAX, 0, 1, 2],
                 [0, 0, 3, ANON, u64::MAX, 0, 1, 2]] {
        let mut calls = 0;
        let r = mmap_16k_with(&args, &AtomicU64::new(0), |n, a| {
            calls += 1;
            assert_eq!(n, SYS_MMAP);
            assert_eq!(*a, args);
            error(libc::EINVAL)
        });
        assert!(r.carry);
        assert_eq!(calls, 1);
    }
}

#[test]
fn unaligned_first_try_is_unmapped_then_reserve_and_both_slack_sides_are_trimmed() {
    for base in [0x10000, 0x11000, 0x12000, 0x13000] {
        let args = [0x51000, PAGE + 1, 3, ANON, u64::MAX, 0, 91, 92];
        let len = 2 * PAGE;
        let aligned = (base + PAGE - 1) & !(PAGE - 1);
        let mut direct = args; direct[1] = len;
        let mut fixed = direct; fixed[0] = aligned; fixed[3] |= FIXED;
        let mut steps = vec![
            (SYS_MMAP, direct, ok(0x71000)),
            (SYS_MUNMAP, [0x71000, len, 0, 0, 0, 0, 0, 0], ok(0)),
            (SYS_MMAP, [0, len + PAGE, 0, ANON, u64::MAX, 0, 0, 0], ok(base)),
            (SYS_MMAP, fixed, ok(aligned)),
        ];
        if aligned > base {
            steps.push((SYS_MUNMAP, [base, aligned - base, 0, 0, 0, 0, 0, 0], ok(0)));
        }
        let (end, reserve_end) = (aligned + len, base + len + PAGE);
        if reserve_end > end {
            steps.push((SYS_MUNMAP, [end, reserve_end - end, 0, 0, 0, 0, 0, 0], ok(0)));
        }
        let mut steps = steps.into_iter();
        let hint = AtomicU64::new(0x88000);
        let r = mmap_16k_with(&args, &hint, |n, a| {
            let (want_n, want_a, result) = steps.next().expect("extra host call");
            assert_eq!((n, *a), (want_n, want_a));
            result
        });
        assert!(!r.carry);
        assert_eq!(r.rax, aligned);
        assert_eq!(hint.load(Ordering::Relaxed), end);
        assert!(steps.next().is_none(), "missing cleanup");
    }
}

#[test]
fn failures_propagate_and_failed_remap_releases_entire_reservation() {
    let args = [0, PAGE, 3, ANON, u64::MAX, 0, 0, 0];
    for failed_step in [0, 2, 3] {
        let mut calls = Vec::new();
        let hint = AtomicU64::new(0);
        let r = mmap_16k_with(&args, &hint, |n, a| {
            let i = calls.len();
            calls.push((n, *a));
            if i == failed_step { return error(libc::ENOMEM); }
            match i { 0 => ok(0x11000), 2 => ok(0x21000), _ => ok(0) }
        });
        assert!(r.carry);
        assert_eq!(r.rax, libc::ENOMEM as u64);
        assert_eq!(hint.load(Ordering::Relaxed), 0);
        assert_eq!(calls.len(), if failed_step == 3 { 5 } else { failed_step + 1 });
        if failed_step == 3 {
            assert_eq!(calls[4], (SYS_MUNMAP, [0x21000, 2 * PAGE, 0, 0, 0, 0, 0, 0]));
        }
    }
    let r = mmap_16k_with(&[0, u64::MAX, 3, ANON, u64::MAX, 0, 0, 0], &AtomicU64::new(0), |_, _| panic!("overflow forwarded"));
    assert!(r.carry);
    assert_eq!(r.rax, libc::EINVAL as u64);
}

// Go through the guest entry point, including PROT_EXEC / MAP_JIT stripping.
fn guest_mmap(args: [u64; 8]) -> SysResult {
    let mut cpu = Cpu::default();
    cpu.x[..8].copy_from_slice(&args);
    cpu.x[16] = SYS_MMAP;
    handle_svc(&mut cpu);
    SysResult { rax: cpu.x[0], rdx: cpu.x[1], carry: cpu.cf != 0 }
}
struct Mapping(u64, u64);
impl Mapping {
    fn new(args: [u64; 8]) -> Self {
        let r = guest_mmap(args);
        assert!(!r.carry, "mmap {args:?}: errno {}", r.rax);
        assert_eq!(r.rax % PAGE, 0);
        Self(r.rax, (args[1] + PAGE - 1) & !(PAGE - 1))
    }
    fn bytes(&self) -> &[u8] { unsafe { std::slice::from_raw_parts(self.0 as *const u8, self.1 as usize) } }
    fn bytes_mut(&mut self) -> &mut [u8] { unsafe { std::slice::from_raw_parts_mut(self.0 as *mut u8, self.1 as usize) } }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        assert!(!hostsys::unix(SYS_MUNMAP, &[self.0, self.1, 0, 0, 0, 0, 0, 0]).carry);
    }
}

#[test]
fn real_anonymous_sizes_hints_protections_and_rounded_tails() {
    let sizes = [1, 4095, 4096, 4097, 8192, 16383, 16384, 16385, 32767, 32768, 65537, 1048577];
    for len in sizes {
        for sharing in [libc::MAP_PRIVATE, libc::MAP_SHARED] {
            for prot in [0, 1, 3, 7] {
                // Both a free aligned hint and a deliberately 4 KiB-only hint.
                for hint in [0, 0x3800000000, 0x3800001000] {
                    let flags = (libc::MAP_ANON | sharing) as u64 | MAP_JIT;
                    let mut map = Mapping::new([hint, len, prot, flags, u64::MAX, 0, 0, 0]);
                    assert_eq!(unsafe { libc::mprotect(map.0 as *mut _, map.1 as usize, 3) }, 0);
                    assert!(map.bytes().iter().all(|&b| b == 0));
                    map.bytes_mut().fill(0xa5);
                    assert!(map.bytes().iter().all(|&b| b == 0xa5));
                }
            }
        }
    }
}

#[test]
fn real_file_backed_private_shared_offsets_hints_and_failures() {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsRawFd;
    let path = std::env::temp_dir().join(format!("attesor-mmap-{}.bin", std::process::id()));
    let mut file = std::fs::OpenOptions::new().create_new(true).read(true).write(true).open(&path).unwrap();
    // Unlink immediately: no stale fixture on test failure; fd remains usable.
    std::fs::remove_file(path).unwrap();
    let content: Vec<u8> = (0..2 * 1024 * 1024).map(|i| ((i * 31 + i / 4096) % 251) as u8).collect();
    file.write_all(&content).unwrap();
    for len in [1, 4095, 4097, 16383, 16384, 16385, 32769, 65537, 1048577] {
        for flags in [libc::MAP_PRIVATE, libc::MAP_SHARED] {
            for offset in [0, PAGE, 4 * PAGE] {
                for hint in [0, 0x3800000000, 0x3800001000] {
                    let mut map = Mapping::new([hint, len, 7, flags as u64 | MAP_JIT, file.as_raw_fd() as u64, offset, 0, 0]);
                    assert_eq!(map.bytes(), &content[offset as usize..(offset + map.1) as usize]);
                    // Private writes are COW; shared writes must reach the underlying file.
                    map.bytes_mut()[len as usize - 1] ^= 0xff;
                    assert_eq!(unsafe { libc::msync(map.0 as *mut _, map.1 as usize, libc::MS_SYNC) }, 0);
                    file.seek(SeekFrom::Start(offset + len - 1)).unwrap();
                    let mut byte = [0]; file.read_exact(&mut byte).unwrap();
                    let original = content[(offset + len - 1) as usize];
                    assert_eq!(byte[0], if flags == libc::MAP_SHARED { original ^ 0xff } else { original });
                    // Restore the shared backing bytes for the next case.
                    if flags == libc::MAP_SHARED { map.bytes_mut()[len as usize - 1] = original; }
                }
            }
        }
    }
    // Darwin's raw zero-length mmap can succeed (unlike libc's wrapper).
    // Preserve that native result rather than inventing an EINVAL check.
    let zero = [0, 0, 3, ANON, u64::MAX, 0, 0, 0];
    let native = hostsys::unix(SYS_MMAP, &zero);
    let guest = guest_mmap(zero);
    assert_eq!(guest.carry, native.carry);
    if native.carry { assert_eq!(guest.rax, native.rax); }
    let mut window = Mapping::new([0, 3 * PAGE, 3, ANON, u64::MAX, 0, 0, 0]);
    window.bytes_mut().fill(0x5a);
    let fixed = guest_mmap([window.0 + PAGE, 1, 7, libc::MAP_PRIVATE as u64 | FIXED | MAP_JIT,
                           file.as_raw_fd() as u64, PAGE, 0, 0]);
    assert!(!fixed.carry);
    assert_eq!(fixed.rax, window.0 + PAGE);
    assert_eq!(&window.bytes()[PAGE as usize..PAGE as usize + 4096], &content[PAGE as usize..PAGE as usize + 4096]);
    assert!(window.bytes()[..PAGE as usize].iter().all(|&b| b == 0x5a));
    assert!(window.bytes()[PAGE as usize + 4096..].iter().all(|&b| b == 0x5a));
    for (args, errno) in [
        ([0, PAGE, 1, libc::MAP_PRIVATE as u64, u64::MAX, 0, 0, 0], libc::EBADF),
        ([0, PAGE, 1, libc::MAP_PRIVATE as u64, file.as_raw_fd() as u64, 1, 0, 0], libc::EINVAL),
        ([1, PAGE, 3, ANON | FIXED, u64::MAX, 0, 0, 0], libc::EINVAL),
    ] {
        let r = guest_mmap(args);
        assert!(r.carry, "unexpected success: {args:?}");
        assert_eq!(r.rax, errno as u64);
    }
}

#[test]
fn real_fixed_does_not_round_length_or_replace_its_neighbours() {
    let mut map = Mapping::new([0, 3 * PAGE, 3, ANON, u64::MAX, 0, 0, 0]);
    map.bytes_mut().fill(0x5a);
    // Deliberately only 4 KiB aligned and 4 KiB long: MAP_FIXED stays untouched.
    let at = map.0 + 4096;
    let r = guest_mmap([at, 1, 3, ANON | FIXED, u64::MAX, 0, 0, 0]);
    assert!(!r.carry);
    assert_eq!(r.rax, at);
    assert!(map.bytes()[..4096].iter().all(|&b| b == 0x5a));
    assert!(map.bytes()[4096..8192].iter().all(|&b| b == 0));
    assert!(map.bytes()[8192..].iter().all(|&b| b == 0x5a));
}
