//! Host-only companion FFT setups. The map key is guest identity, never a host
//! pointer returned to guest code. Unregistered setups fall back to the guest.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use super::native::ptr;

#[repr(C)]
struct Split { real: *mut f32, imag: *mut f32 }
#[link(name = "Accelerate", kind = "framework")]
extern "C" {
    fn vDSP_create_fftsetup(n: u64, radix: i32) -> *mut libc::c_void;
    fn vDSP_destroy_fftsetup(s: *mut libc::c_void);
    fn vDSP_fft_zip(s: *mut libc::c_void, z: *const Split, stride: i64, n: u64, direction: i32);
    fn vDSP_fft_zrip(s: *mut libc::c_void, z: *const Split, stride: i64, n: u64, direction: i32);
    fn vDSP_vadd(a: *const f32, sa: i64, b: *const f32, sb: i64, c: *mut f32, sc: i64, n: u64);
    fn vDSP_vmul(a: *const f32, sa: i64, b: *const f32, sb: i64, c: *mut f32, sc: i64, n: u64);
    fn vDSP_vsmul(a: *const f32, sa: i64, b: *const f32, c: *mut f32, sc: i64, n: u64);
    fn vDSP_vsadd(a: *const f32, sa: i64, b: *const f32, c: *mut f32, sc: i64, n: u64);
    fn vDSP_dotpr(a: *const f32, sa: i64, b: *const f32, sb: i64, c: *mut f32, n: u64);
    fn vDSP_sve(a: *const f32, sa: i64, c: *mut f32, n: u64);
}
struct Setup { address: usize, gate: Mutex<()> }
impl Drop for Setup {
    fn drop(&mut self) { unsafe { vDSP_destroy_fftsetup(self.address as *mut libc::c_void); } }
}
fn setups() -> &'static Mutex<HashMap<u64, Arc<Setup>>> {
    static MAP: OnceLock<Mutex<HashMap<u64, Arc<Setup>>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}
// true means actual numerical host work, not setup bookkeeping or fallback.
pub unsafe fn call(a: &[u64; 6]) -> (u64, bool) {
    let _env = super::native::FloatEnv::new();
    match a[0] {
        33 => {
            let s = vDSP_create_fftsetup(a[2], a[3] as i32);
            if !s.is_null() {
                setups().lock().unwrap().insert(ptr(a[1]), Arc::new(Setup { address: s as usize, gate: Mutex::new(()) }));
            }
            (0, false)
        }
        34 => { setups().lock().unwrap().remove(&ptr(a[1])); (0, false) }
        35 | 36 => {
            let s = setups().lock().unwrap().get(&ptr(a[1])).cloned();
            let Some(s) = s else { return (0, false); };
            // Serialize uses of one setup; distinct setups remain concurrent.
            // Holding Arc protects the host allocation through this call.
            let _guard = s.gate.lock().unwrap();
            let mut z = std::ptr::read_unaligned(ptr(a[2]) as *const Split);
            z.real = ptr(z.real as u64) as *mut f32;
            z.imag = ptr(z.imag as u64) as *mut f32;
            let f = if a[0] == 35 { vDSP_fft_zip } else { vDSP_fft_zrip };
            f(s.address as *mut libc::c_void, &z, a[3] as i64, a[4], a[5] as i32);
            (1, true)
        }
        37..=42 => {
            let v = std::ptr::read_unaligned(ptr(a[1]) as *const [u64; 7]);
            let x = ptr(v[0]) as *const f32;
            let y = ptr(v[2]) as *const f32;
            let z = ptr(v[4]) as *mut f32;
            match a[0] {
                37 => vDSP_vadd(x, v[1] as i64, y, v[3] as i64, z, v[5] as i64, v[6]),
                38 => vDSP_vmul(x, v[1] as i64, y, v[3] as i64, z, v[5] as i64, v[6]),
                39 => vDSP_vsmul(x, v[1] as i64, y, z, v[5] as i64, v[6]),
                40 => vDSP_vsadd(x, v[1] as i64, y, z, v[5] as i64, v[6]),
                41 => vDSP_dotpr(x, v[1] as i64, y, v[3] as i64, z, v[6]),
                42 => vDSP_sve(x, v[1] as i64, z, v[6]),
                _ => unreachable!(),
            }
            (0, true)
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unregistered_setup_falls_back_without_reading_guest_memory() {
        unsafe { assert_eq!(call(&[35, 1, 0, 1, 4, 1]), (0, false)); }
    }
    #[test]
    fn host_companion_lifetime() {
        let key = 0x1234_5678;
        unsafe {
            assert_eq!(call(&[33, key, 4, 2, 0, 0]), (0, false));
            assert!(setups().lock().unwrap().contains_key(&key));
            call(&[34, key, 0, 0, 0, 0]);
            assert!(!setups().lock().unwrap().contains_key(&key));
            assert_eq!(call(&[36, key, 0, 1, 4, 1]), (0, false));
        }
    }
}
