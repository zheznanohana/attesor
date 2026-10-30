//! Bounded, thread-private BLR site cache. Only dispatcher transitions write it;
//! generated code only reads it. Never publish a shared target/host tuple.

use super::{Cpu, EPOCH, INDEX, TRANSLATE};
use std::cell::RefCell;
use std::mem::{offset_of, size_of};
use std::sync::atomic::Ordering;
use std::sync::OnceLock;

pub(super) const SITES: usize = 1024;

#[repr(C)]
#[derive(Clone, Copy)]
struct Entry {
    site: u64,
    target: u64,
    host: u64,
    epoch: u64,
}

pub(super) const SITE: usize = offset_of!(Entry, site);
pub(super) const TARGET: usize = offset_of!(Entry, target);
pub(super) const HOST: usize = offset_of!(Entry, host);
pub(super) const EPOCH_OFFSET: usize = offset_of!(Entry, epoch);
const EMPTY: Entry = Entry { site: 1, target: 1, host: 0, epoch: u64::MAX };

thread_local! {
    static TABLE: RefCell<Option<Box<[Entry]>>> = const { RefCell::new(None) };
}

pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let has = |name| std::env::var(name).unwrap_or_default().split(',').any(|v| v == "bliccache");
        has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF")
    })
}

/// Collisions replace data, not code or reusable site IDs. The generated path
/// always checks the complete site and target PCs, never just this hash.
pub(super) fn offset(site: u64) -> usize {
    let hash = (site >> 2) ^ (site >> 12) ^ (site >> 22);
    (hash as usize & (SITES - 1)) * size_of::<Entry>()
}

pub(super) fn setup(cpu: &mut Cpu) {
    if !enabled() {
        return;
    }
    TABLE.with(|table| {
        let mut table = table.borrow_mut();
        let entries = table.get_or_insert_with(|| vec![EMPTY; SITES].into_boxed_slice());
        cpu.bliccache = entries.as_mut_ptr() as u64;
    });
}

/// Called after the dispatcher resolved the target normally, before JIT entry.
/// The translation lock serializes this validation and generation snapshot with
/// invalidation/publication. BLOCKS alone is insufficient: externally registered
/// AOT entries currently have no UnitRec/source invalidation registration.
pub(super) fn publish(site: u64, target: u64, host: u64) {
    let _translate = TRANSLATE.lock().unwrap();
    if super::lookup(target) != Some(host) {
        return;
    }
    let index = INDEX.lock().unwrap();
    let registered = index.as_ref().is_some_and(|index| {
        if index.cold.get(&target).is_some_and(|c| c.host == host) { return false; }
        index.pages.get(&(target >> 14)).is_some_and(|ids| ids.iter().any(|&id| {
            let unit = &index.units[id as usize];
            unit.alive && unit.entry == target && unit.host == host
        }))
    });
    let entry = Entry {
        site,
        target,
        // A negative result never stores an unregistered host entry. Its hit
        // takes the original IBTC path (no extra dispatcher round trip).
        host: if registered { host } else { 0 },
        epoch: EPOCH.load(Ordering::Acquire),
    };
    TABLE.with(|table| {
        if let Some(entries) = table.borrow_mut().as_mut() {
            entries[offset(site) / size_of::<Entry>()] = entry;
        }
    });
}

/// Untrusted profile hint only: never call the old host or trust its epoch.
/// The emitter re-reads/tracks the source, guards the full target and checks
/// the current entry epoch. A promotion itself can advance EPOCH.
pub(super) fn target_hint(site:u64)->Option<u64> {
    TABLE.with(|table| {
        let t=table.borrow();let e=t.as_ref()?.get(offset(site)/size_of::<Entry>())?;
        (e.site==site&&e.host!=0&&e.target&3==0).then_some(e.target)
    })
}

thread_local! {static PROMOTED:RefCell<std::collections::BTreeSet<u64>>=const {RefCell::new(std::collections::BTreeSet::new())};}
/// Bounded one-shot per observed site, after publish released its locks.
/// Invalidation withdraws the old caller; no executable instruction is patched.
pub(super) fn promote_leaf(site:u64,target:u64) {
    if !super::emit::leafinline_enabled()||!super::precise_invalidation_enabled()||!enabled() {return;}
    let reader=super::guest_reader();
    if target_hint(site)!=Some(target)||inline_blocked(site)||!super::emit::inline_leaf_candidate(target,&reader) {return;}
    let once=PROMOTED.with(|p| {let mut p=p.borrow_mut();p.len()<SITES&&p.insert(site)});
    if once {
        if std::env::var_os("ATTESOR_LEAFINLINE_LOG").is_some() {eprintln!("leafinline-observed\t{site:x}\t{target:x}\t{:08x}\tcommpage={}",reader(site).unwrap_or(0),super::commpage_checks());}
        super::invalidate_range(site,4);
    }
}

#[cfg(test)]
pub(super) fn set_hint_for_test(site:u64,target:u64) {
    TABLE.with(|t| {
        let mut t=t.borrow_mut();let entries=t.get_or_insert_with(||vec![EMPTY;SITES].into_boxed_slice());
        entries[offset(site)/size_of::<Entry>()]=Entry{site,target,host:1,epoch:u64::MAX};
    });
}

// Lock-free, full-tag, bounded negative feedback. No new mutex can be
// inherited locked across guest fork. Never evict a blocked site on collision.
static INLINE_BLOCKED:[std::sync::atomic::AtomicU64;SITES]=[const {std::sync::atomic::AtomicU64::new(1)};SITES];
static INLINE_STOP:std::sync::atomic::AtomicBool=std::sync::atomic::AtomicBool::new(false);
pub(super) fn inline_blocked(site:u64)->bool {
    if INLINE_STOP.load(Ordering::Acquire) {return true;}
    let start=offset(site)/size_of::<Entry>();
    for n in 0..SITES {let v=INLINE_BLOCKED[(start+n)&(SITES-1)].load(Ordering::Acquire);
        if v==site {return true;}if v==1 {return false;}}
    true
}
pub(super) fn block_inline(site:u64) {
    let start=offset(site)/size_of::<Entry>();
    for n in 0..SITES {let e=&INLINE_BLOCKED[(start+n)&(SITES-1)];
        let v=e.load(Ordering::Acquire);if v==site {return;}
        if v==1 {match e.compare_exchange(1,site,Ordering::AcqRel,Ordering::Acquire) {Ok(_)=>return,Err(x)if x==site=>return,_=>{}}}
    }
    INLINE_STOP.store(true,Ordering::Release);
}
