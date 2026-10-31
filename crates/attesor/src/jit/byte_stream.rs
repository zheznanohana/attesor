//! Native counted byte-stream versioning: full remaining-EA range, not a
//! pointer-untagging/ABI assumption. SAME raw mapping, original native TBI slow.
use super::*;
use std::collections::BTreeMap;
const MASK:u64=0x00ff_ffff_ffff_ffff;
pub(super) struct Plan {words:BTreeMap<u64,u32>,at:u64,word:u32,ptr:u32,count:u32,end:u64}
fn byte_post_one(w:u32)->bool {w&0xffe00c00==0x38400400&&bits(w,20,12)==1}
fn native_gp(w:u32)->bool {
    if w==0xd503201f {return true;}
    let major=(w>>25)&15;
    matches!(major,5|8|9|13) && (flags_safe_passthrough(w)||flags_safe_writer(w)||w&0x3fe00800==0x1a800000)
}
pub(super) fn plan(u:&Unit,pc:u64,r:LoopRa,read:&dyn Fn(u64)->Option<u32>)->Option<Plan> {
    if !u.bytestream||u.quick||u.no_internal_jumps||u.commpage_checks||disabled_classes()!=0
        ||u.function_entry.is_some()||u.leaf_livein.is_some()||!u.region_labels.is_empty()
        ||r.pc!=pc||r.flags!=0xff||u.bytestream_site.is_some()||r.vreg.iter().any(|&n|n!=0xff)||r.xreg.iter().any(|&n|n!=0xff) {return None;}
    pc.checked_add((MAX_BLOCK_INSNS as u64+1)*4)?;
    let mut words=BTreeMap::new();let mut memory=None;let mut previous=None;
    for i in 0..MAX_BLOCK_INSNS {
        let at=pc.checked_add(i as u64*4)?;
        if i>0&&u.labels.contains_key(&at) {return None;}
        let w=read(at)?;words.insert(at,w);
        if is_terminator(w) {
            if w&0xff00001f!=0x54000001||i<2 {return None;}
            let target=at.wrapping_add((sext(bits(w,23,5)as u64,19)<<2)as u64);if target!=pc {return None;}
            let sub=previous?;let count=sub&31;
            if count==31||sub!=(0xf1000400u32|(count<<5)|count) {return None;}
            let(mem_pc,mem_word)=memory?;let ptr=bits(mem_word,9,5);let rt=mem_word&31;
            if ptr==rt||count==ptr||count==rt {return None;}
            r.slot(ptr)?;r.slot(count)?;
            for (&p,&v) in &words {if p==mem_pc||p==at||p==at-4 {continue;}
                let a=cache_access(v);
                if v!=0xd503201f&&(a.xr==u32::MAX||a.xw&((1u32<<ptr)|(1u32<<count))!=0) {return None;}
            }
            return Some(Plan{words,at:mem_pc,word:mem_word,ptr,count,end:at+4});
        }
        if byte_post_one(w) {if memory.replace((at,w)).is_some(){return None;}}
        else if !native_gp(w) {return None;}
        previous=Some(w);
    }None
}
pub(super) fn guards(u:&mut Unit,r:LoopRa,ptr:u32,count:u32,slow:CodeLabel) {
    let pointer=LOOPREGS[r.slot(ptr).unwrap()];let counter=LOOPREGS[r.slot(count).unwrap()];
    u.a.mov(rax,counter).unwrap();u.a.sub(rax,1).unwrap();u.a.jb(slow).unwrap();
    u.a.mov(rcx,MASK).unwrap();u.a.sub(rcx,rax).unwrap();u.a.jb(slow).unwrap();
    u.a.cmp(pointer,rcx).unwrap();u.a.ja(slow).unwrap();
}
fn body(u:&mut Unit,pc:u64,p:&Plan,r:LoopRa,fast:bool,mask_live:bool)->u64 {
    u.region=Some(r);u.rc=RegCache::new();u.vc=VCache::new();u.vzero=[0;2];u.fuse=None;
    u.tbi_mask_live=mask_live;u.rc_frozen=false;u.dead_flags=false;u.dead_v=0;u.vc_dest=[0xff;2];
    u.loop_exit_flags=None;u.loop_exit_compare=None;u.pending_funedge=None;u.skip_next=false;u.skip_more=0;
    if u.notbi_enabled {u.notbi.clear();}
    let mut label=r.fast;u.a.set_label(&mut label).unwrap();
    #[cfg(test)] u.a.mov(qword_ptr(r15+off::AOT_LOOP_REGIONS),if fast {1i32}else {2i32}).unwrap();
    if super::super::loops::census_enabled() {u.a.mov(rax,pc).unwrap();u.a.mov(qword_ptr(r15+off::PC),rax).unwrap();}
    let read=|at|p.words.get(&at).copied();u.look.clear();let mut at=pc+4;
    while u.look.len()<LOOKAHEAD {let Some(w)=read(at)else {break;};u.look.push(w);if is_terminator(w) {break;}at+=4;}
    u.bytestream_site=fast.then_some((p.at,p.word));let start=u.a.instructions().len();let fallback=u.insns_fallback;
    let end=emit_block_body(u,pc,&read,None,false,start);u.bytestream_site=None;
    assert_eq!(u.insns_fallback,fallback,"byte-stream whitelist must remain native");
    assert!(!u.a.instructions()[start..].iter().any(|i|i.mnemonic()==iced_x86::Mnemonic::Call),"byte stream acquired a helper");
    end
}
pub(super) fn emit(u:&mut Unit,pc:u64,p:Plan,r:LoopRa)->u64 {
    assert!(GUEST_FAULTS_FATAL);let slow=u.a.create_label();let mask_live=u.tbi_mask_live;
    guards(u,r,p.ptr,p.count,slow);let before=u.insns_translated;
    let end=body(u,pc,&p,r,true,mask_live);let n=u.insns_translated-before;
    assert_eq!(body(u,pc,&p,LoopRa{fast:slow,..r},false,mask_live),end);
    assert_eq!(u.insns_translated-before,n*2);u.insns_translated=before+n;
    assert_eq!(end,p.end);u.bytestream_emissions+=1;
    if std::env::var_os("ATTESOR_BYTESTREAM_LOG").is_some() {eprintln!("bytestream-loop\t{pc:x}\tload={:x}",p.at);}
    end
}
pub(super) fn load(e:&mut E<'_>,w:u32)->bool {
    assert!(byte_post_one(w));let rn=bits(w,9,5);let base=LOOPREGS[e.u.region.unwrap().slot(rn).unwrap()];
    e.commpage_fix_known(true);assert!(e.access_gpr_at(w&31,0,true,base,0));
    e.u.a.lea(base,qword_ptr(base+1)).unwrap();e.stx(rn,base,true);true
}
