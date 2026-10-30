//! Bounded closed GPR function CFG for opt-in recursive register residency.
//! Not an algorithm recognizer: never replaces guest computation. Only direct
//! calls back to this entry are admitted; all returns remain LR-checked.
use std::collections::{BTreeMap,BTreeSet,VecDeque};
use super::{flags_safe_passthrough,flags_safe_writer,sext};
use super::super::loops::{Loop,successors};

pub(super) fn recursive(header:u64,read:&dyn Fn(u64)->Option<u32>)->Option<Loop> {
    let mut words=BTreeMap::new();let mut starts=BTreeSet::from([header]);
    let mut pending=VecDeque::from([header]);let mut calls=0;let mut returns=0;
    while let Some(pc)=pending.pop_front() {
        if words.contains_key(&pc) {continue;}
        if words.len()>=128 || pc<header || pc-header>=4096 || pc&3!=0 {return None;}
        let w=read(pc)?;
        let call=w&0xfc000000==0x94000000;
        let ret=w==0xd65f03c0;
        let branch=w&0xfc000000==0x14000000 || w&0xff000010==0x54000000
            || w&0x7e000000==0x34000000 || w&0x7e000000==0x36000000;
        if call {
            let target=pc.wrapping_add((sext((w&0x03ffffff)as u64,26)<<2)as u64);
            if target!=header {return None;}
            calls+=1;
        } else if ret {returns+=1;}
        else if !branch {
            // Exclude SIMD and its memory forms, special scratch users and
            // unproven helpers/system state. GPR memory faults remain fatal.
            if (w>>25)&7==7 || ((w>>25)&5==4 && w&(1<<26)!=0)
                || (!flags_safe_passthrough(w)&&!flags_safe_writer(w)) {return None;}
        }
        words.insert(pc,w);
        for next in successors(pc,w) {
            if branch||call {starts.insert(next);}
            pending.push_back(next);
        }
    }
    (calls>0&&returns>0).then_some(Loop{header,words,starts,backedges:vec![]})
}
