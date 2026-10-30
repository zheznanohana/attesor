//! Guest path redirection.
//!
//! On an Intel Mac the arm64 dyld shared cache is not installed; the user
//! supplies it (e.g. extracted from an Apple Silicon IPSW of the same macOS
//! version) and points Attesor at it with `--sysroot`. dyld's lookups in the
//! standard cache directories are redirected there.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Mutex;

static CACHE_DIR: Mutex<Option<String>> = Mutex::new(None);
static CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);
static PATH_CACHE_ENABLED: AtomicBool = AtomicBool::new(false);
static PATH_CACHE_PID: AtomicI32 = AtomicI32::new(0);

// Payload bounds are per host thread. Only successful, purely lexical dyld
// redirects are retained; no filesystem or container metadata is cached.
const PATH_CACHE_ENTRIES: usize = 128;
const PATH_CACHE_BYTES: usize = 128 * 1024;
const PATH_CACHE_PATH_BYTES: usize = 4096;

#[derive(Default)]
struct RedirectCache {
    pid: libc::pid_t,
    generation: u64,
    bytes: usize,
    entries: HashMap<String, String>,
}

thread_local! {
    static REDIRECT_CACHE: RefCell<RedirectCache> = RefCell::new(RedirectCache::default());
}

impl RedirectCache {
    fn reset(&mut self, pid: libc::pid_t, generation: u64) {
        self.entries.clear();
        self.bytes = 0;
        self.pid = pid;
        self.generation = generation;
    }

    fn redirect(&mut self, path: &str, rest: &str, pid: libc::pid_t) -> Option<String> {
        let generation = CACHE_GENERATION.load(Ordering::Acquire);
        if self.pid != pid || self.generation != generation {
            self.reset(pid, generation);
        }
        if let Some(mapped) = self.entries.get(path) {
            return Some(mapped.clone());
        }

        // Pair the directory and its generation under the existing lock. A
        // concurrent setter may have published a new generation since lookup.
        let dir = CACHE_DIR.lock().unwrap();
        let generation = CACHE_GENERATION.load(Ordering::Relaxed);
        if self.generation != generation {
            self.reset(pid, generation);
        }
        let mapped = format!("{}{rest}", dir.as_ref()?);
        if mapped.len() <= PATH_CACHE_PATH_BYTES {
            let bytes = path.len() + mapped.len();
            if self.entries.len() >= PATH_CACHE_ENTRIES || self.bytes + bytes > PATH_CACHE_BYTES {
                // Whole-cache eviction avoids an unbounded recency queue or
                // retaining duplicate keys; the table never exceeds the cap.
                self.reset(pid, generation);
            }
            self.entries.insert(path.to_owned(), mapped.clone());
            self.bytes += bytes;
        }
        Some(mapped)
    }
}

const CACHE_PREFIXES: &[&str] = &[
    "/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld",
    "/System/Cryptexes/OS/System/Library/dyld",
    "/System/Library/dyld",
];

pub fn set_cache_dir(d: &str) {
    // Select the feature at sysroot configuration time, before guest execution.
    // An environment change alone is not a live reconfiguration API.
    let on = std::env::var("ATTESOR_JIT_ON").unwrap_or_default();
    let off = std::env::var("ATTESOR_JIT_OFF").unwrap_or_default();
    set_cache_dir_with_policy(d, pathcache_selected(&on, &off));
}

fn pathcache_selected(on: &str, off: &str) -> bool {
    on.split(',').any(|flag| flag == "pathcache")
        && !off.split(',').any(|flag| flag == "pathcache")
}

fn set_cache_dir_with_policy(d: &str, selected: bool) {
    let mut dir = CACHE_DIR.lock().unwrap();
    *dir = Some(d.trim_end_matches('/').to_string());
    // Do not wrap the identity: once exhausted, permanently use the old path.
    // All publishers hold CACHE_DIR, so no CAS loop or wrapping increment is needed.
    let advanced = if let Some(next) = CACHE_GENERATION.load(Ordering::Relaxed).checked_add(1) {
        CACHE_GENERATION.store(next, Ordering::Release);
        true
    } else { false };
    PATH_CACHE_PID.store(unsafe { libc::getpid() }, Ordering::Release);
    PATH_CACHE_ENABLED.store(selected && advanced, Ordering::Release);
}

/// If `path` should be redirected, return the replacement.
pub fn redirect(path: &str) -> Option<String> {
    if PATH_CACHE_ENABLED.load(Ordering::Acquire) {
        let pid = unsafe { libc::getpid() };
        // A fork child must not reuse the parent's TLS answers or initialize a
        // new cache from potentially inherited configuration locks. Until the
        // child explicitly configures its sysroot, retain the preexisting path.
        if pid == PATH_CACHE_PID.load(Ordering::Acquire) && path.len() <= PATH_CACHE_PATH_BYTES {
            // Only absolute, exact-prefix mappings enter the cache. Relative
            // openat paths are resolved by the existing caller on every call.
            let rest = cache_suffix(path)?;
            if let Ok(Some(mapped)) = REDIRECT_CACHE.try_with(|cache| {
                cache.try_borrow_mut().ok().map(|mut cache| cache.redirect(path, rest, pid))
            }) {
                return mapped;
            }
            // TLS teardown/reentrancy is not a reason to change path semantics.
        }
    }
    redirect_uncached(path)
}

fn redirect_uncached(path: &str) -> Option<String> {
    let dir = CACHE_DIR.lock().unwrap().clone()?;
    Some(format!("{dir}{}", cache_suffix(path)?))
}

fn cache_suffix(path: &str) -> Option<&str> {
    for p in CACHE_PREFIXES {
        if let Some(rest) = path.strip_prefix(p) {
            if rest.is_empty() || rest.starts_with('/') {
                return Some(rest);
            }
        }
    }
    None
}

pub fn cache_dir() -> Option<String> {
    CACHE_DIR.lock().unwrap().clone()
}

/// Find a directory holding the arm64 dyld shared cache when `--sysroot` was not given:
/// `$ATTESOR_SYSROOT`, then `~/.attesor/sysroot` (a directory, or a file containing one),
/// then the newest `~/attesor-sysroot/*` subdirectory that has a `dyld_shared_cache_arm64e`.
pub fn discover_sysroot() -> Option<String> {
    let has_cache = |d: &std::path::Path| d.join("dyld_shared_cache_arm64e").is_file();
    if let Ok(d) = std::env::var("ATTESOR_SYSROOT") {
        if has_cache(std::path::Path::new(&d)) {
            return Some(d);
        }
    }
    let home = std::env::var("HOME").ok()?;
    let cfg = std::path::Path::new(&home).join(".attesor/sysroot");
    if cfg.is_dir() && has_cache(&cfg) {
        return Some(cfg.to_string_lossy().into_owned());
    }
    if let Ok(t) = std::fs::read_to_string(&cfg) {
        let p = std::path::PathBuf::from(t.trim());
        if has_cache(&p) {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    let root = std::path::Path::new(&home).join("attesor-sysroot");
    let mut dirs: Vec<_> = std::fs::read_dir(&root).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| has_cache(p)).collect();
    dirs.sort();
    dirs.pop().map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pathcache_directed_flags_bounds_reconfigure_threads_fork() {
        assert!(pathcache_selected("other,pathcache", ""));
        assert!(!pathcache_selected("pathcache", "other,pathcache"));
        assert!(!pathcache_selected("pathcache-extra,not-pathcache", ""));
        assert!(!pathcache_selected("", ""));
        let saved = (CACHE_DIR.lock().unwrap().clone(), CACHE_GENERATION.load(Ordering::Relaxed),
            PATH_CACHE_ENABLED.load(Ordering::Relaxed), PATH_CACHE_PID.load(Ordering::Relaxed));
        set_cache_dir_with_policy("/private/test-root///", true);
        for prefix in CACHE_PREFIXES {
            for suffix in ["", "/cache", "/../cache", "/./file", "//nested"] {
                let path = format!("{prefix}{suffix}");
                let expected = Some(format!("/private/test-root{suffix}"));
                assert_eq!(redirect(&path), expected);
                assert_eq!(redirect(&path), redirect_uncached(&path));
            }
            assert_eq!(redirect(&format!("{prefix}-adjacent")), None);
        }
        for path in ["relative/cache", "dyld", "/tmp/other", "System/Library/dyld"] {
            assert_eq!(redirect(path), None);
        }
        let path = "/System/Library/dyld/cache";
        assert_eq!(redirect(path).as_deref(), Some("/private/test-root/cache"));
        // Directed causal control: a cached hit must use TLS rather than silently
        // rerun the uncached mapping. Deliberately poison only the test's entry.
        REDIRECT_CACHE.with(|c| { c.borrow_mut().entries.insert(path.into(), "test-only-hit".into()); });
        assert_eq!(redirect(path).as_deref(), Some("test-only-hit"));
        assert_ne!(redirect(path), redirect_uncached(path));
        set_cache_dir_with_policy("/private/new-root", true);
        assert_eq!(redirect(path).as_deref(), Some("/private/new-root/cache"), "generation must reject stale TLS answers");
        REDIRECT_CACHE.with(|c| {
            let _borrow = c.borrow_mut();
            assert_eq!(redirect(path), redirect_uncached(path), "reentrant borrow must fall back");
        });
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (next_tx, next_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            assert_eq!(redirect(path).as_deref(), Some("/private/new-root/cache"));
            ready_tx.send(()).unwrap(); next_rx.recv().unwrap();
            assert_eq!(redirect(path).as_deref(), Some("/private/thread-root/cache"));
        });
        ready_rx.recv().unwrap(); set_cache_dir_with_policy("/private/thread-root", true);
        next_tx.send(()).unwrap(); worker.join().unwrap();
        // Whole-cache eviction remains bounded under both count and byte pressure.
        for suffix_len in [4, 2000] {
            set_cache_dir_with_policy("/private/thread-root", true);
            for n in 0..300 {
                let key = format!("/System/Library/dyld/{n:04}{}", "x".repeat(suffix_len));
                assert_eq!(redirect(&key), redirect_uncached(&key));
                REDIRECT_CACHE.with(|c| {
                    let c = c.borrow();
                    assert!(c.entries.len() <= PATH_CACHE_ENTRIES);
                    assert!(c.bytes <= PATH_CACHE_BYTES);
                    assert_eq!(c.bytes, c.entries.iter().map(|(k,v)| k.len()+v.len()).sum::<usize>());
                });
            }
        }
        let long = format!("/System/Library/dyld/{}", "x".repeat(4096));
        assert_eq!(redirect(&long), redirect_uncached(&long));
        REDIRECT_CACHE.with(|c| assert!(!c.borrow().entries.contains_key(&long)));
        set_cache_dir_with_policy(&format!("/{}", "x".repeat(4096)), true);
        assert_eq!(redirect(path), redirect_uncached(path));
        REDIRECT_CACHE.with(|c| assert!(c.borrow().entries.is_empty()));
        set_cache_dir_with_policy("/private/parent-root", true);
        redirect(path);
        REDIRECT_CACHE.with(|c| { c.borrow_mut().entries.insert(path.into(), "stale-parent".into()); });
        // Only self-created child, no signals, and all helper threads joined.
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            let bypassed = redirect(path).as_deref() == Some("/private/parent-root/cache");
            set_cache_dir_with_policy("/private/child-root", true);
            let reconfigured = redirect(path).as_deref() == Some("/private/child-root/cache");
            unsafe { libc::_exit(if bypassed && reconfigured { 0 } else { 1 }); }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert!(libc::WIFEXITED(status)); assert_eq!(libc::WEXITSTATUS(status), 0);
        set_cache_dir_with_policy("/private/off-root", false);
        assert_eq!(redirect(path).as_deref(), Some("/private/off-root/cache"));
        CACHE_GENERATION.store(u64::MAX, Ordering::Release);
        set_cache_dir_with_policy("/private/exhausted", true);
        assert!(!PATH_CACHE_ENABLED.load(Ordering::Acquire));
        assert_eq!(CACHE_GENERATION.load(Ordering::Relaxed), u64::MAX);
        assert_eq!(redirect(path).as_deref(), Some("/private/exhausted/cache"));
        // Restore global configuration for the remaining L0 suites.
        *CACHE_DIR.lock().unwrap() = saved.0;
        CACHE_GENERATION.store(saved.1, Ordering::Release);
        PATH_CACHE_PID.store(saved.3, Ordering::Release);
        PATH_CACHE_ENABLED.store(saved.2, Ordering::Release);
        REDIRECT_CACHE.with(|c| c.borrow_mut().reset(0, 0));
    }
}
