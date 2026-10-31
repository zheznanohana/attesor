//! Per-basic-block SP range versioning, not an ABI/recursive balance assumption.
//! Recheck actual raw SP at EVERY block with optimized frame accesses. Thus a
//! callee, nonlocal return, RAS overflow or external entry cannot inherit stale
//! range facts. Slow bodies retain the original native per-access TBI path.
use super::*;
use std::collections::BTreeMap;
const MASK:u64=0x00ff_ffff_ffff_ffff;
pub(super) struct Plan {words:BTreeMap<u64,u32>,sites:HashMap<u64,u32>,lo:i64,hi:i64}
pub(super) fn pair_word(w:u32)->bool {
    w&0x3a000000==0x28000000 && w&0xc4000000==0x80000000
        && bits(w,9,5)==31 && bits(w,24,23)!=0
        && !(bit(w,22)!=0 && w&31==bits(w,14,10))
}
pub(super) fn plan(u:&Unit,pc:u64,read:&dyn Fn(u64)->Option<u32>)->Option<Plan> {
    if !u.funstack||u.quick||u.function_entry.is_none()||u.region_labels.is_empty()
        ||u.commpage_checks||disabled_classes()!=0||!u.funstack_sites.is_empty() {return None;}
    u.region?.slot(31)?;
    let mut p=Plan{words:BTreeMap::new(),sites:HashMap::new(),lo:0,hi:0};let mut delta=0i64;
    for i in 0..MAX_BLOCK_INSNS {
        let at=pc.checked_add(i as u64*4)?;
        if i>0&&u.labels.contains_key(&at) {break;}
        let w=read(at)?;p.words.insert(at,w);
        // Terminator effects are AFTER all optimized accesses. No call/helper
        // is admitted between the guard and a proven stack access.
        if is_terminator(w) {
            if !(w&0x7c000000==0x14000000||w&0xff000010==0x54000000
                ||w&0x7e000000==0x34000000||w&0x7e000000==0x36000000||w==0xd65f03c0) {return None;}
            break;
        }
        if pair_word(w) {
            let mode=bits(w,24,23);let off=sext(bits(w,21,15)as u64,7)*8;
            let ea=delta+if mode==1 {0}else {off};
            p.lo=p.lo.min(ea);p.hi=p.hi.max(ea+15);p.sites.insert(at,w);
            if mode==1||mode==3 {delta+=off;}
        }else if w&0x1f800000==0x11000000 && (flags_safe_passthrough(w)||flags_safe_writer(w)) {
            // ADD/SUB immediate can write SP only with S=0,Rd=31.
            if bit(w,29)==0&&w&31==31 {
                if bit(w,31)==0||bits(w,9,5)!=31 {return None;}
                let imm=(bits(w,21,10)as i64)<<if bit(w,22)!=0 {12}else {0};
                delta+=if bit(w,30)!=0 {-imm}else {imm};
            }
        }else if (w&0x1f200000==0x0b000000||w&0x1f000000==0x0a000000
                ||w&0x1f800000==0x12800000||w==0xd503201f)
            &&(flags_safe_passthrough(w)||flags_safe_writer(w)) {
            // Shifted-register ADD/SUB/logical and move-wide use ZR, NOT SP.
        }else {return None;}
        p.lo=p.lo.min(delta);p.hi=p.hi.max(delta);
        if p.lo < -(1<<20)||p.hi>1<<20 {return None;}
    }
    if p.sites.is_empty() {return None;}
    Some(p)
}
pub(super) fn guard(u:&mut Unit,stack_reg:AsmRegister64,lo:i64,hi:i64,slow:CodeLabel) {
    if lo<0 {u.a.cmp(stack_reg,(-lo)as i32).unwrap();u.a.jb(slow).unwrap();}
    u.a.mov(rax,MASK-hi as u64).unwrap();u.a.cmp(stack_reg,rax).unwrap();u.a.ja(slow).unwrap();
}
pub(super) fn emit(u:&mut Unit,pc:u64,p:Plan)->u64 {
    let r=u.region.unwrap();let stack_reg=LOOPREGS[r.slot(31).unwrap()];let mut slow=u.a.create_label();
    guard(u,stack_reg,p.lo,p.hi,slow);
    let look=u.look.clone();let mask_live=u.tbi_mask_live;let translated=u.insns_translated;
    let read=|at|p.words.get(&at).copied();
    u.funstack_sites=p.sites;
    let start=u.a.instructions().len();
    let fast_end=emit_block_body(u,pc,&read,None,false,start);
    let fast_count=u.insns_translated-translated;
    u.funstack_sites.clear();u.a.set_label(&mut slow).unwrap();
    // Compile-time state from FAST must NOT flow into a mutually exclusive SLOW.
    u.region=Some(r);u.rc=RegCache::new();u.vc=VCache::new();u.vzero=[0;2];u.fuse=None;
    u.tbi_mask_live=mask_live;u.look=look;u.rc_frozen=false;u.dead_flags=false;
    u.loop_exit_flags=None;u.loop_exit_compare=None;u.pending_funedge=None;
    if u.notbi_enabled {u.notbi.clear();}
    let start=u.a.instructions().len();let slow_end=emit_block_body(u,pc,&read,None,false,start);
    assert_eq!(fast_end,slow_end);assert_eq!(u.insns_translated-translated,fast_count*2);
    u.insns_translated=translated+fast_count;u.funstack_emissions+=1;
    if std::env::var_os("ATTESOR_FUNSTACK_LOG").is_some() {eprintln!("funstack-block\t{pc:x}\tspan={}..{}",p.lo,p.hi);}
    slow_end
}
pub(super) fn pair(e:&mut E<'_>,w:u32)->bool {
    assert!(pair_word(w));let r=e.u.region.unwrap();let stack_reg=LOOPREGS[r.slot(31).unwrap()];
    let off=(sext(bits(w,21,15)as u64,7)*8)as i32;let mode=bits(w,24,23);let disp=if mode==1 {0}else {off};
    // Same shared memory backend, same two 8-byte accesses in source order.
    // The range guard proved BOTH complete EAs; raw SP/data never lose tags.
    e.commpage_fix_known(true);
    assert!(e.access_gpr_at(w&31,3,bit(w,22)!=0,stack_reg,disp));
    e.commpage_fix_known(true);
    assert!(e.access_gpr_at(bits(w,14,10),3,bit(w,22)!=0,stack_reg,disp+8));
    // Keep writeback AFTER both accesses, including pre-indexed instructions.
    if mode==1||mode==3 {e.u.a.lea(stack_reg,qword_ptr(stack_reg+off)).unwrap();e.stx(31,stack_reg,true);}
    true
}
