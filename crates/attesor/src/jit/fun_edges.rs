//! Conditional packing in closed recursive GPR scopes. A self call is NOT a
//! transparent boundary: it kills incoming NZCV only with a separately captured
//! native entry-prefix proof. Its returned NEW flags/epoch guard remain intact.
use super::*;
use std::collections::{BTreeMap,BTreeSet};
#[derive(Clone,Copy)]
pub(super) struct Edge {pub cmp_pc:u64,pub cmp:u32,pub branch:u32,pub taken:u64,pub not_taken:u64,pub live_yes:bool,pub live_no:bool}
#[derive(Default)]
pub(super) struct Analysis {pub edges:HashMap<u64,Edge>,pub proof:BTreeMap<u64,u32>}
// The exact small native templates, not the broad profitability decoder.
fn pure(w:u32)->bool {
    fun_stack::pair_word(w)||((w&0x1f800000==0x11000000
        ||w&0x1f200000==0x0b000000||w&0x1f000000==0x0a000000
        ||w&0x1f800000==0x12800000||w&0x1f000000==0x10000000||w==0xd503201f)
        && flags_safe_passthrough(w))
}
fn kill(w:u32)->bool {writes_all_flags(w)&&flags_safe_writer(w)}
fn branch_target(pc:u64,w:u32)->u64 {pc.wrapping_add((sext((w&0x03ffffff)as u64,26)<<2)as u64)}
// Deliberately linear. Any branch, call, helper, missing word or system before
// the first overwrite fails the self-entry proof. Private entry has no public
// reload/marker/FP guard; possible funstack guards observe no guest NZCV.
pub(super) fn prefix(l:&super::super::loops::Loop)->Option<BTreeMap<u64,u32>> {
    let mut p=l.header;let mut proof=BTreeMap::new();
    for _ in 0..MAX_BLOCK_INSNS {
        let w=*l.words.get(&p)?;proof.insert(p,w);
        if kill(w) {return Some(proof);}
        if !pure(w) {return None;}
        p=p.checked_add(4)?;
    }None
}
pub(super) fn dead(l:&super::super::loops::Loop,start:u64,entry:Option<&BTreeMap<u64,u32>>)->Option<BTreeMap<u64,u32>> {
    let mut todo=vec![(start,BTreeSet::new())];let mut proof=BTreeMap::new();let mut budget=256;
    while let Some((p,mut path))=todo.pop() {
        if budget==0||!path.insert(p) {return None;}budget-=1;
        let w=*l.words.get(&p)?;proof.insert(p,w);
        if kill(w) {continue;}
        if reads_flags(w) {return None;}
        if w&0xfc000000==0x94000000 {
            if branch_target(p,w)!=l.header {return None;}
            proof.extend(entry?.iter().map(|(&k,&v)|(k,v)));continue;
        }
        if w&0xfc000000==0x14000000 {todo.push((branch_target(p,w),path));}
        else if w&0x7e000000==0x34000000||w&0x7e000000==0x36000000 {
            for next in super::super::loops::successors(p,w) {todo.push((next,path.clone()));}
        }else {
            if !pure(w) {return None;}
            todo.push((p.checked_add(4)?,path));
        }
    }Some(proof)
}
pub(super) fn analyze(l:&super::super::loops::Loop)->Analysis {
    assert!(GUEST_FAULTS_FATAL);let mut out=Analysis::default();let entry=prefix(l);
    for (&pc,&w) in &l.words {
        if !matches!(fusable_setter(w),Some(FuseKind::Sub))||w&31!=31||!kill(w)||l.starts.contains(&(pc+4)) {continue;}
        let Some(&b)=l.words.get(&(pc+4)) else {continue;};
        if b&0xff000010!=0x54000000||b&15>=14||x86_cc(FuseKind::Sub,b&15).is_none() {continue;}
        let yes=(pc+4).wrapping_add((sext(bits(b,23,5)as u64,19)<<2)as u64);let no=pc+8;
        if !l.starts.contains(&yes)||!l.starts.contains(&no) {continue;}
        let y=dead(l,yes,entry.as_ref());let n=dead(l,no,entry.as_ref());
        if y.is_none()&&n.is_none() {continue;}
        let e=Edge{cmp_pc:pc,cmp:w,branch:b,taken:yes,not_taken:no,live_yes:y.is_none(),live_no:n.is_none()};
        for proof in [y,n].into_iter().flatten() {out.proof.extend(proof);}
        out.proof.insert(pc,w);out.proof.insert(pc+4,b);out.edges.insert(pc,e);
    }out
}
