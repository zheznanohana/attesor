//! Bounded loop CFG analysis for the census and opt-in register allocation. The graph is made from instruction successors,
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

impl Loop {
    /// In the private clone the header is the only public entry. Require a
    /// reducible CFG as well: removing dominance backedges must leave a DAG.
    /// This rejects irreducible interior SCCs instead of calling them natural.
    pub fn reducible(&self) -> bool {
        let mut edges: BTreeMap<u64, BTreeSet<u64>> = self.starts.iter().map(|&p| (p, BTreeSet::new())).collect();
        for (&pc, &w) in &self.words {
            let from = *self.starts.range(..=pc).next_back().unwrap();
            for to in successors(pc, w) {
                if self.starts.contains(&to) { edges.get_mut(&from).unwrap().insert(to); }
            }
        }
        let mut pred: BTreeMap<u64, Vec<u64>> = self.starts.iter().map(|&p| (p, vec![])).collect();
        for (&from, tos) in &edges { for to in tos { pred.get_mut(to).unwrap().push(from); } }
        let mut dom: BTreeMap<u64, BTreeSet<u64>> = self.starts.iter().map(|&p|
            (p, if p == self.header { BTreeSet::from([p]) } else { self.starts.clone() })).collect();
        loop {
            let mut changed = false;
            for &p in &self.starts {
                if p == self.header { continue; }
                if pred[&p].is_empty() { return false; }
                let mut d = self.starts.clone();
                for from in &pred[&p] { d.retain(|v| dom[from].contains(v)); }
                d.insert(p);
                if dom[&p] != d { dom.insert(p, d); changed = true; }
            }
            if !changed { break; }
        }
        if dom.values().any(|d| !d.contains(&self.header)) { return false; }
        let mut indeg: BTreeMap<u64, usize> = self.starts.iter().map(|&p| (p,0)).collect();
        for (&from, tos) in &mut edges {
            tos.retain(|to| !dom[&from].contains(to));
            for to in tos.iter() { *indeg.get_mut(to).unwrap() += 1; }
        }
        let mut queue: Vec<_> = indeg.iter().filter_map(|(&p,&n)| (n == 0).then_some(p)).collect();
        let mut n = 0;
        while let Some(p) = queue.pop() {
            n += 1;
            for to in &edges[&p] {
                let v = indeg.get_mut(to).unwrap(); *v -= 1;
                if *v == 0 { queue.push(*to); }
            }
        }
        n == self.starts.len()
    }
}

fn relative(pc: u64, imm: u32, bits: u32) -> u64 {
    pc.wrapping_add((((imm as i64) << (64 - bits)) >> (62 - bits)) as u64)
}

/// Exactly the indirect link forms already implemented by both interpreters:
/// BLR, BLRAAZ/BLRABZ. Sign/auth are identity in this project's current ISA
/// model; this optimizer does not introduce or remove a PAC implementation.
pub(super) fn indirect_call_reg(w:u32)->Option<u32> {
    (w&0xfffffc1f==0xd63f0000||w&0xfffff81f==0xd63f081f).then_some((w>>5)&31)
}

pub fn successors(pc: u64, w: u32) -> Vec<u64> {
    if w & 0xfc00_0000 == 0x1400_0000 {
        vec![relative(pc, w & 0x03ff_ffff, 26)]
    } else if w & 0xff00_0010 == 0x5400_0000 || w & 0x7e00_0000 == 0x3400_0000 {
        vec![relative(pc, (w >> 5) & 0x7ffff, 19), pc + 4]
    } else if w & 0x7e00_0000 == 0x3600_0000 {
        vec![relative(pc, (w >> 5) & 0x3fff, 14), pc + 4]
    } else if w & 0xfc00_0000 == 0x9400_0000 || indirect_call_reg(w).is_some() {
        // Census includes guest calls; allocation can reject them separately.
        vec![pc + 4]
    } else if is_terminator(w) { vec![] } else { vec![pc + 4] }
}

pub fn discover(header: u64, read: &dyn Fn(u64) -> Option<u32>) -> Option<Loop> {
    discover_impl(header, read, true, None)
}

/// Guest calls are canonical exits, not mandatory members of a write-back
/// region. In particular, a cold V8 runtime call must not pull its continuation
/// and a large reachable function tail into every hot-loop discovery.
pub fn allocation(header: u64, read: &dyn Fn(u64) -> Option<u32>) -> Option<Loop> {
    discover_impl(header, read, false, None)
}

pub(super) fn allocation_with_calls(header:u64,read:&dyn Fn(u64)->Option<u32>,call:&dyn Fn(u64,u32)->bool)->Option<Loop> {
    discover_impl(header,read,false,Some(call))
}

fn discover_impl(header: u64, read: &dyn Fn(u64) -> Option<u32>, census: bool, call_allowed:Option<&dyn Fn(u64,u32)->bool>) -> Option<Loop> {
    let mut graph = BTreeMap::new();
    let mut queue = std::collections::VecDeque::from([header]);
    let mut seen = BTreeSet::from([header]);
    let mut pred: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    while let Some(pc) = queue.pop_front() {
        if graph.len() == if census { MAX_INSNS } else { 256 } { break; }
        let Some(w) = read(pc) else { continue };
        let call = w & 0xfc00_0000 == 0x9400_0000 || indirect_call_reg(w).is_some();
        if !census && call && !call_allowed.is_some_and(|f|f(pc,w)) { continue; }
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
    *ON.get_or_init(|| {
        let has = |name| std::env::var(name).unwrap_or_default().split(',').any(|s| s == "loopcensus");
        has("ATTESOR_JIT_ON") && !has("ATTESOR_JIT_OFF")
    })
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
        assert!(l.reducible());
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
    #[test]
    fn irreducible_interior_cycle_and_budget_are_rejected() {
        let l = find(&[0xb4000060,0x14000002,0xd4001001,0xb5ffffc1,0x17fffffc]).unwrap();
        assert!(!l.reducible());
        let mut long = vec![0xd503201f; MAX_INSNS + 1];
        long.push(0x14000000 | ((-(long.len() as i32) as u32) & 0x03ffffff));
        assert!(find(&long).is_none());
    }

    #[test]
    fn allocation_cuts_guest_call_arm_but_census_keeps_it() {
        let words = [0x36000060,0x94000005,0x14000002,0x91000421,0xf1000442,0x54ffff61,0xd4001001];
        let read = |p: u64| p.checked_sub(0x1000).and_then(|v| words.get((v/4) as usize)).copied();
        let all = discover(0x1000,&read).unwrap();
        let alloc = allocation(0x1000,&read).unwrap();
        assert!(all.words.contains_key(&0x1004));
        assert!(!alloc.words.contains_key(&0x1004));
        assert!(alloc.words.contains_key(&0x100c));
        assert!(alloc.reducible());
    }

}
