//! Bounded, read-only loop census. The graph is made from instruction successors,
//! then reduced to the SCC containing the proposed header. Missing/unreadable or
//! out-of-budget successors are exits, never implicit members of a region.
use std::collections::{BTreeMap, BTreeSet};
use super::emit::is_terminator;

pub const MAX_INSNS: usize = 512;
pub const MAX_BYTES: u64 = 8192;

#[derive(Clone, Debug)]
pub struct Loop {
    pub header: u64,
    pub words: BTreeMap<u64, u32>,
    pub starts: BTreeSet<u64>,
    pub backedges: Vec<(u64, &'static str)>,
}

fn relative(pc: u64, imm: u32, bits: u32) -> u64 {
    pc.wrapping_add((((imm as i64) << (64 - bits)) >> (62 - bits)) as u64)
}

pub fn successors(pc: u64, w: u32) -> Vec<u64> {
    if w & 0xfc00_0000 == 0x1400_0000 {
        vec![relative(pc, w & 0x03ff_ffff, 26)]
    } else if w & 0xff00_0010 == 0x5400_0000 || w & 0x7e00_0000 == 0x3400_0000 {
        vec![relative(pc, (w >> 5) & 0x7ffff, 19), pc + 4]
    } else if w & 0x7e00_0000 == 0x3600_0000 {
        vec![relative(pc, (w >> 5) & 0x3fff, 14), pc + 4]
    } else if w & 0xfc00_0000 == 0x9400_0000 || w & 0xffff_fc1f == 0xd63f_0000 {
        // Census includes guest calls; allocation can reject them separately.
        vec![pc + 4]
    } else if is_terminator(w) { vec![] } else { vec![pc + 4] }
}

pub fn discover(header: u64, read: &dyn Fn(u64) -> Option<u32>) -> Option<Loop> {
    let mut graph = BTreeMap::new();
    let mut queue = std::collections::VecDeque::from([header]);
    let mut seen = BTreeSet::from([header]);
    let mut pred: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    while let Some(pc) = queue.pop_front() {
        if graph.len() == MAX_INSNS { break; }
        let Some(w) = read(pc) else { continue };
        for next in successors(pc, w) {
            pred.entry(next).or_default().push(pc);
            if next >= header && next - header < MAX_BYTES && seen.insert(next) { queue.push_back(next); }
        }
        graph.insert(pc, w);
    }
    let mut members = BTreeSet::from([header]);
    let mut stack = pred.get(&header)?.clone();
    while let Some(pc) = stack.pop() {
        if graph.contains_key(&pc) && members.insert(pc) {
            stack.extend(pred.get(&pc).into_iter().flatten().copied());
        }
    }
    let words: BTreeMap<_, _> = graph.into_iter().filter(|(pc, _)| members.contains(pc)).collect();
    let mut starts = BTreeSet::from([header]);
    let mut backedges = Vec::new();
    for (&pc, &w) in &words {
        let nexts = successors(pc, w);
        if is_terminator(w) {
            for &next in &nexts { if words.contains_key(&next) { starts.insert(next); } }
            if nexts.contains(&header) {
                backedges.push((pc, if w & 0xfc00_0000 == 0x1400_0000 { "b" }
                    else if w & 0xff00_0010 == 0x5400_0000 { "b.cond" }
                    else if w & 0x7e00_0000 == 0x3400_0000 { "cb" } else { "tb" }));
            }
        }
    }
    (!backedges.is_empty()).then_some(Loop { header, words, starts, backedges })
}

pub fn census_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("ATTESOR_JIT_ON").unwrap_or_default().split(',').any(|v| v == "loopcensus"))
}

pub fn census(pc: u64, read: &dyn Fn(u64) -> Option<u32>) {
    if !census_enabled() { return; }
    if let Some(l) = discover(pc, read) {
        // stderr is the raw metadata stream; no host pointers enter AOT code,
        // no new mutex participates in fork, no guest bytes are modified.
        let (image, _) = crate::symbols::attribute(pc);
        eprintln!("loop-census\t{:x}\t{}\t{}\t{}\t{}", l.header, image, l.starts.len(),
            l.backedges.iter().map(|(p,s)| format!("{p:x}:{s}")).collect::<Vec<_>>().join(","),
            l.words.keys().map(|p| format!("{p:x}")).collect::<Vec<_>>().join(","));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn find(w: &[u32]) -> Option<Loop> {
        discover(0x1000, &|p| p.checked_sub(0x1000).and_then(|v| w.get((v/4) as usize)).copied())
    }
    #[test]
    fn signed_backedges_and_diamond() {
        // cbz x0,arm; add; b join; sub; subs; b.ne header; svc
        let l = find(&[0xb4000060,0x91000421,0x14000002,0xd1000421,0xf1000442,0x54ffff61,0xd4001001]).unwrap();
        assert_eq!(l.starts, BTreeSet::from([0x1000,0x1004,0x100c,0x1010]));
        assert_eq!(l.words.len(),6);
        assert_eq!(l.backedges,vec![(0x1014,"b.cond")]);
    }
    #[test]
    fn self_loop_side_exit_and_unreadable() {
        assert_eq!(find(&[0x91000400,0xb5ffffe1]).unwrap().starts.len(),1);
        let l = find(&[0xb4000081,0x91000400,0xb5ffffc1,0xd4001001,0xd4001021]).unwrap();
        assert_eq!(l.words.len(),3);
        assert_eq!(l.starts.len(),2);
        assert!(find(&[0x94000000,0xd4001001]).is_none());
        assert!(find(&[]).is_none());
    }
}
