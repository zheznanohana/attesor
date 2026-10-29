//! Persistent pre-slid data, independent of the code-generator version.
//!
//! Only host pages actually written by rebasing are stored. The remaining
//! pages retain their original file backing. All integers are little-endian.
//! The checksum covers the header, key and extent table only: readers must not read the whole
//! payload at every process start (that cost ~190 ms and faulted in ~360 MB). The payload is
//! trusted because the writer fsyncs a temporary file and renames it into place.
use super::{MappingSlideNp, SharedFileNp, VM_PROT_ZF};
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"DSCSLD01";
const VERSION: u64 = 2;
const HEAD: usize = 56;
const ALIGN: u64 = 0x4000;
const HASH_INIT: u64 = 0xcbf2_9ce4_8422_2325;

fn put(out: &mut Vec<u8>, n: u64) { out.extend_from_slice(&n.to_le_bytes()); }
fn get(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }
fn align(n: u64) -> Option<u64> { Some(n.checked_add(ALIGN - 1)? & !(ALIGN - 1)) }
fn invalid() -> io::Error { io::Error::new(io::ErrorKind::InvalidData, "invalid pre-slid cache") }

/// Word-wise FNV-style corruption checksum (not a cryptographic authenticator).
/// Every input to this function is a multiple of eight bytes.
fn hash(mut h: u64, bytes: &[u8]) -> u64 {
    for word in bytes.chunks_exact(8) {
        h = (h ^ u64::from_le_bytes(word.try_into().unwrap())).wrapping_mul(0x100_0000_01b3);
    }
    h
}

pub(super) struct Identity {
    bytes: Vec<u8>,
    path: PathBuf,
    page: u64,
    max_payload: u64,
    max_extents: u64,
}

impl Identity {
    pub(super) fn new(files: &[SharedFileNp], maps: &[MappingSlideNp], slide: u64, dir: &Path) -> Option<Self> {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        if page == 0 || !page.is_power_of_two() || ALIGN % page != 0 { return None; }
        let mut bytes = Vec::new();
        put(&mut bytes, VERSION);
        put(&mut bytes, slide);
        put(&mut bytes, page);
        put(&mut bytes, files.len() as u64);
        let mut mi = 0;
        let mut max_payload = 0u64;
        for f in files {
            put(&mut bytes, f.mappings_count as u64);
            // Include every subcache UUID, not descriptor numbers (which change
            // between processes). Dynamic config is never persisted.
            let mut uuid = [0; 16];
            if f.fd >= 0 && !super::pread_all(f.fd, &mut uuid, 88) { return None; }
            bytes.extend_from_slice(&uuid);
            put(&mut bytes, (f.fd < 0) as u64);
            for _ in 0..f.mappings_count {
                let m = maps.get(mi)?;
                mi += 1;
                for v in [m.address, m.size, m.file_offset, m.slide_size, m.slide_start,
                          m.max_prot as u64, m.init_prot as u64] { put(&mut bytes, v); }
                if m.slide_size != 0 {
                    // Unsupported inputs simply take the original rebasing path.
                    if f.fd < 0 || m.max_prot & VM_PROT_ZF != 0 || m.init_prot & 1 == 0
                        || m.address % page != 0 || m.size % page != 0 { return None; }
                    max_payload = max_payload.checked_add(m.size)?;
                }
            }
        }
        if mi != maps.len() || max_payload == 0 { return None; }
        let path = dir.join(format!("dscslide-v{VERSION}-{:016x}.dscslide", hash(HASH_INIT, &bytes)));
        Some(Self { bytes, path, page, max_payload, max_extents: max_payload / page })
    }

    pub(super) fn load(&self, maps: &[MappingSlideNp]) -> io::Result<Cached> {
        let file = File::open(&self.path)?;
        let len = file.metadata()?.len();
        let max_head = align(HEAD as u64 + self.bytes.len() as u64 + self.max_extents * 24).ok_or_else(invalid)?;
        if len < HEAD as u64 || len > max_head + self.max_payload || len > usize::MAX as u64 { return Err(invalid()); }
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len as usize, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED { return Err(io::Error::last_os_error()); }
        let mut cached = Cached { file, ptr, len: len as usize, extents: Vec::new(), data: 0 };
        let b = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
        let (extents, data) = self.validate(b, maps)?;
        cached.extents = extents;
        cached.data = data;
        Ok(cached)
    }

    fn validate(&self, b: &[u8], maps: &[MappingSlideNp]) -> io::Result<(Vec<Extent>, u64)> {
        if b.len() < HEAD || &b[..8] != MAGIC || get(b, 8) != VERSION || get(b, 16) != self.bytes.len() as u64 { return Err(invalid()); }
        let n = get(b, 24);
        let data = get(b, 32);
        let payload = get(b, 40);
        if n > self.max_extents || payload > self.max_payload
            || data != align(HEAD as u64 + self.bytes.len() as u64 + n * 24).ok_or_else(invalid)?
            || data.checked_add(payload) != Some(b.len() as u64)
            || data > b.len() as u64
            || b.get(HEAD..HEAD + self.bytes.len()) != Some(self.bytes.as_slice()) { return Err(invalid()); }
        if hash(hash(HASH_INIT, &b[..48]), &b[HEAD..data as usize]) != get(b, 48) { return Err(invalid()); }
        let mut extents = Vec::new();
        let mut total = 0u64;
        let mut previous = None;
        for i in 0..n as usize {
            let p = HEAD + self.bytes.len() + i * 24;
            let e = Extent { mapping: get(b, p), offset: get(b, p + 8), size: get(b, p + 16) };
            let m = maps.get(usize::try_from(e.mapping).map_err(|_| invalid())?).ok_or_else(invalid)?;
            if m.slide_size == 0 || e.size == 0 || e.offset % self.page != 0 || e.size % self.page != 0
                || e.offset.checked_add(e.size).is_none_or(|end| end > m.size)
                || previous.is_some_and(|(index, end)| e.mapping < index || (e.mapping == index && e.offset < end)) { return Err(invalid()); }
            previous = Some((e.mapping, e.offset + e.size));
            total = total.checked_add(e.size).ok_or_else(invalid)?;
            extents.push(e);
        }
        if total != payload { return Err(invalid()); }
        Ok((extents, data))
    }

    /// Nonblocking: another starter can rebase normally rather than stall on a
    /// large write. Recheck after acquiring the lock to close the miss/write race.
    pub(super) fn writer(&self) -> io::Result<File> {
        std::fs::create_dir_all(self.path.parent().ok_or_else(invalid)?)?;
        let lock = OpenOptions::new().create(true).truncate(false).write(true).open(self.path.with_extension("dscslide.lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 { return Err(io::Error::last_os_error()); }
        Ok(lock)
    }

    pub(super) unsafe fn publish(&self, maps: &[MappingSlideNp], slide: u64, extents: &[Extent]) -> io::Result<u64> {
        let data = align(HEAD as u64 + self.bytes.len() as u64 + extents.len() as u64 * 24).ok_or_else(invalid)?;
        let payload: u64 = extents.iter().map(|e| e.size).sum();
        let mut header = MAGIC.to_vec();
        for v in [VERSION, self.bytes.len() as u64, extents.len() as u64, data, payload, 0] { put(&mut header, v); }
        header.extend_from_slice(&self.bytes);
        for e in extents { for v in [e.mapping, e.offset, e.size] { put(&mut header, v); } }
        header.resize(data as usize, 0);
        let tmp = self.path.with_extension(format!("dscslide.tmp.{}.{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()));
        // Drop guard also cleans up failures, never exposing a partial final file.
        struct Temp(PathBuf);
        impl Drop for Temp { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }
        let _tmp = Temp(tmp.clone());
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(&header)?;
        let checksum = hash(hash(HASH_INIT, &header[..48]), &header[HEAD..]);
        for e in extents {
            let addr = maps[e.mapping as usize].address.wrapping_add(slide) + e.offset;
            let bytes = std::slice::from_raw_parts(addr as *const u8, e.size as usize);
            file.write_all(bytes)?;
        }
        file.seek(SeekFrom::Start(48))?;
        file.write_all(&checksum.to_le_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(data + payload)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Extent { pub mapping: u64, pub offset: u64, pub size: u64 }

pub(super) struct Cached {
    file: File,
    ptr: *mut libc::c_void,
    len: usize,
    extents: Vec<Extent>,
    data: u64,
}
impl Drop for Cached { fn drop(&mut self) { unsafe { libc::munmap(self.ptr, self.len); } } }
impl Cached {
    pub(super) fn pages(&self, page: u64) -> u64 { self.extents.iter().map(|e| e.size / page).sum() }
    pub(super) fn size(&self) -> usize { self.len }
    pub(super) unsafe fn map(&self, maps: &[MappingSlideNp], slide: u64) -> io::Result<()> {
        let mut offset = self.data;
        for e in &self.extents {
            let m = maps[e.mapping as usize];
            let addr = m.address.wrapping_add(slide) + e.offset;
            let p = crate::profile::host_call(|| libc::mmap(addr as *mut _, e.size as usize,
                (m.init_prot & 3) as i32, libc::MAP_PRIVATE | libc::MAP_FIXED, self.file.as_raw_fd(), offset as i64));
            if p == libc::MAP_FAILED || p as u64 != addr { return Err(io::Error::last_os_error()); }
            offset += e.size;
        }
        // Touch the opened inode, not a path potentially replaced by a builder.
        libc::futimes(self.file.as_raw_fd(), std::ptr::null());
        Ok(())
    }
}

/// Lightweight housekeeping check; full payload validation happens before use.
pub(crate) fn compatible(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0; 16];
    File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok()
        && &head[..8] == MAGIC && get(&head, 8) == VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    struct Fixture { dir: PathBuf, source: File, maps: Vec<MappingSlideNp>, addr: u64, size: usize }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target").join(format!("dscslide-unit-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(&dir).unwrap();
            let size = 4 * unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let source = OpenOptions::new().read(true).write(true).create_new(true).open(dir.join("source")).unwrap();
            source.set_len(size as u64).unwrap();
            let addr = unsafe { libc::mmap(std::ptr::null_mut(), size, 3, libc::MAP_ANON | libc::MAP_PRIVATE, -1, 0) };
            assert_ne!(addr, libc::MAP_FAILED);
            unsafe { std::ptr::write_bytes(addr, 0x5a, size); }
            let maps = vec![MappingSlideNp { address: addr as u64, size: size as u64, file_offset: 0,
                slide_size: 32, slide_start: 0x1234, max_prot: 3, init_prot: 3 }];
            Self { dir, source, maps, addr: addr as u64, size }
        }
        fn files(&self) -> [SharedFileNp; 1] { [SharedFileNp { fd: self.source.as_raw_fd(), mappings_count: 1, slide: 0 }] }
        fn id(&self) -> Identity { Identity::new(&self.files(), &self.maps, 0, &self.dir).unwrap() }
        fn publish(&self) -> (Identity, Vec<u8>) {
            let id = self.id();
            let _lock = writer_wait(&id);
            let extents = [Extent { mapping: 0, offset: 0, size: id.page }, Extent { mapping: 0, offset: 2*id.page, size: id.page }];
            unsafe { id.publish(&self.maps, 0, &extents).unwrap(); }
            let bytes = std::fs::read(&id.path).unwrap();
            (id, bytes)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            unsafe { libc::munmap(self.addr as *mut _, self.size); }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    // Another workspace unit test uses a raw fork to probe a JIT fault. Its
    // short-lived child can inherit test-only descriptors outside the runtime
    // fork gate. Wait for that child to close them; production stays nonblocking.
    fn writer_wait(id: &Identity) -> File {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match id.writer() {
                Ok(lock) => return lock,
                Err(e) if std::time::Instant::now() < deadline && e.kind() == io::ErrorKind::WouldBlock =>
                    std::thread::sleep(std::time::Duration::from_millis(1)),
                Err(e) => panic!("writer lock: {e}"),
            }
        }
    }
    fn tidy_until_removed(f: &Fixture, path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while path.exists() && std::time::Instant::now() < deadline {
            crate::aot::tidy_cache_in(&f.dir);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!path.exists());
    }
    fn resign(b: &mut [u8]) {
        let data = get(b, 32) as usize;
        let checksum = hash(hash(HASH_INIT, &b[..48]), &b[HEAD..data.min(b.len())]);
        b[48..56].copy_from_slice(&checksum.to_le_bytes());
    }
    fn age(path: &Path) {
        let old = unsafe { libc::time(std::ptr::null_mut()) } - 31 * 86400;
        let times = [libc::timeval { tv_sec: old, tv_usec: 0 }; 2];
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) }, 0);
    }

    #[test]
    fn key_covers_uuid_slide_and_complete_mapping_table() {
        let f = Fixture::new();
        let original = f.id();
        let duplicate = f.source.try_clone().unwrap();
        let mut files = f.files();
        files[0].fd = duplicate.as_raw_fd();
        assert_eq!(original.bytes, Identity::new(&files, &f.maps, 0, &f.dir).unwrap().bytes);
        assert_ne!(original.bytes, Identity::new(&files, &f.maps, 0x4000, &f.dir).unwrap().bytes);
        for field in 0..7 {
            let mut maps = f.maps.clone();
            match field {
                0 => maps[0].address += original.page,
                1 => maps[0].size += original.page,
                2 => maps[0].file_offset += original.page,
                3 => maps[0].slide_size += 8,
                4 => maps[0].slide_start += 8,
                5 => maps[0].max_prot ^= 4,
                _ => maps[0].init_prot ^= 2,
            }
            let changed = Identity::new(&files, &maps, 0, &f.dir).unwrap();
            assert_ne!(original.path, changed.path);
            let (_, valid) = f.publish();
            assert!(changed.validate(&valid, &maps).is_err());
        }
        use std::os::unix::fs::FileExt;
        f.source.write_at(&[1; 16], 88).unwrap();
        assert_ne!(original.bytes, f.id().bytes);
        let mut maps = f.maps.clone(); maps[0].max_prot |= VM_PROT_ZF;
        assert!(Identity::new(&files, &maps, 0, &f.dir).is_none());
        assert!(Identity::new(&[], &f.maps, 0, &f.dir).is_none());
    }

    #[test]
    fn validates_header_size_key_checksum_and_extent_geometry() {
        let f = Fixture::new();
        let (id, bytes) = f.publish();
        assert!(id.validate(&bytes, &f.maps).is_ok());
        for len in (0..HEAD).chain([bytes.len()-1]) { assert!(id.validate(&bytes[..len], &f.maps).is_err()); }
        let mut extra = bytes.clone(); extra.push(0); assert!(id.validate(&extra, &f.maps).is_err());
        // Header, key and extent table only: the payload is not checksummed by readers (see module docs).
        for offset in [0, 8, 16, 24, 32, 40, 48, HEAD] {
            let mut bad = bytes.clone(); bad[offset] ^= 1;
            assert!(id.validate(&bad, &f.maps).is_err(), "offset {offset}");
        }
        let start = HEAD + id.bytes.len();
        for (offset, value) in [(start, 1), (start+8, 1), (start+16, 0),
            (start+16, u64::MAX), (start+24+8, 0), (start+24+8, f.size as u64)] {
            let mut bad = bytes.clone(); bad[offset..offset+8].copy_from_slice(&value.to_le_bytes()); resign(&mut bad);
            assert!(id.validate(&bad, &f.maps).is_err(), "extent offset {offset}");
        }
        std::fs::write(&id.path, &bytes[..100]).unwrap();
        assert!(id.load(&f.maps).is_err()); // miss/fallback, no mmap of unvalidated data
        std::fs::write(&id.path, &extra).unwrap(); assert!(id.load(&f.maps).is_err());
        std::fs::remove_file(&id.path).unwrap(); assert!(id.load(&f.maps).is_err());
        std::fs::create_dir(&id.path).unwrap(); assert!(id.load(&f.maps).is_err());
    }

    #[test]
    fn sparse_private_mapping_preserves_unmodified_pages_and_touches_on_use() {
        let f = Fixture::new();
        let (id, bytes) = f.publish();
        age(&id.path);
        let cached = id.load(&f.maps).unwrap();
        unsafe {
            std::ptr::write_bytes(f.addr as *mut u8, 0xa5, f.size);
            cached.map(&f.maps, 0).unwrap();
            assert_eq!(*(f.addr as *const u8), 0x5a);
            assert_eq!(*((f.addr + id.page) as *const u8), 0xa5);
            assert_eq!(*((f.addr + 2*id.page) as *const u8), 0x5a);
            *(f.addr as *mut u8) = 0xff;
        }
        assert_eq!(std::fs::read(&id.path).unwrap(), bytes); // MAP_PRIVATE, not shared
        assert!(std::fs::metadata(&id.path).unwrap().modified().unwrap().elapsed().unwrap().as_secs() < 60);
    }

    #[test]
    fn partially_failed_overlay_restores_all_original_bytes() {
        use std::os::unix::fs::FileExt;
        let f = Fixture::new();
        f.source.write_all_at(&vec![0x11; f.size], 0).unwrap();
        let (id, _) = f.publish();
        let mut cached = id.load(&f.maps).unwrap();
        // Force EINVAL on the SECOND overlay, after the first has succeeded.
        // No production fault-injection switch or global CACHE mutation needed.
        cached.extents[1].offset += 1;
        assert!(!super::super::map_with_cache(&f.files(), &f.maps, 0, Some(&cached)).unwrap());
        let restored = unsafe { std::slice::from_raw_parts(f.addr as *const u8, f.size) };
        assert!(restored.iter().all(|&b| b == 0x11), "must not slide partially cached data twice");
    }

    #[test]
    fn exclusive_nonblocking_writer_and_housekeeping() {
        let f = Fixture::new();
        let (id, _) = f.publish();
        let lock = writer_wait(&id);
        assert!(id.writer().is_err());
        age(&id.path);
        crate::aot::tidy_cache_in(&f.dir);
        assert!(id.path.exists(), "must not remove a locked publication");
        drop(lock);
        tidy_until_removed(&f, &id.path);
        assert!(id.path.with_extension("dscslide.lock").exists(), "stable lock inode");
        let (_, mut bytes) = f.publish();
        bytes[8..16].copy_from_slice(&(VERSION + 1).to_le_bytes());
        std::fs::write(&id.path, bytes).unwrap();
        assert!(!compatible(&id.path));
        tidy_until_removed(&f, &id.path);
        let (_, _) = f.publish();
        crate::aot::tidy_cache_in(&f.dir);
        assert!(id.path.exists(), "recent valid cache survives");
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let pid = child.id(); child.wait().unwrap();
        let tmp = id.path.with_extension(format!("dscslide.tmp.{pid}.1"));
        std::fs::write(&tmp, b"interrupted builder").unwrap();
        tidy_until_removed(&f, &tmp);
    }
}
