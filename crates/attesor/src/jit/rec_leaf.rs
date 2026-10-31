//! Guarded call-free path specialization of a closed recursive GP function.
//! No algorithm replacement: clone original prefix/leaf CFG and all memory
//! effects. Nonleaf/miss/RAS-full retains original host_call. No ABI assumptions.
use super::*;
use std::collections::{BTreeMap,BTreeSet};
#[derive(Clone)]
pub(super) struct Plan {header:u64,cmp:u32,branch_pc:u64,cond:u32,leaf:u64,taken:bool,words:BTreeMap<u64,u32>,starts:BTreeSet<u64>,lazy:BTreeMap<u64,BTreeMap<u64,u32>>}
#[derive(Clone,Copy)]
pub(super) struct Recipe {pub cmp:u32}
#[derive(Clone,Copy)]
pub(super) struct Context {pub branch_pc:u64,pub leaf:u64,pub recipe:Option<Recipe>,next:u64,continuation:CodeLabel}
fn gp(w:u32)->bool {
    fun_stack::pair_word(w)||w&0x3b000000==0x39000000&&bit(w,26)==0&&bits(w,23,22)<=1
        ||((w&0x1f800000==0x11000000||w&0x1f200000==0x0b000000
            ||w&0x1f000000==0x0a000000||w&0x1f800000==0x12800000
            ||w&0x1f000000==0x10000000||w==0xd503201f)
            &&(flags_safe_passthrough(w)||flags_safe_writer(w)))
}
fn bitreg(n:u32,allow_sp:bool)->u32 {if n==31&&!allow_sp {0}else {1<<n}}
fn writes(w:u32)->u32 {
    if fun_stack::pair_word(w) {
        let mode=bits(w,24,23);let mut wr=if mode==1||mode==3 {1<<31}else {0};
        if bit(w,22)!=0 {wr|=bitreg(w&31,false)|bitreg(bits(w,14,10),false);}wr
    }else {cache_access(w).xw}
}
fn cmp_inputs(w:u32)->Option<u32> {
    if w&0x7f80001f==0x7100001f {Some(bitreg(bits(w,9,5),true))}
    else if w&0x7fe0fc1f==0x6b00001f {Some(bitreg(bits(w,9,5),false)|bitreg(bits(w,20,16),false))}else {None}
}
// Prove every selected leaf path finite/native/call-free, not just one trace.
fn leaf(l:&super::super::loops::Loop,start:u64,forbidden:&BTreeMap<u64,u32>)->Option<BTreeMap<u64,u32>> {
    fn visit(l:&super::super::loops::Loop,p:u64,banned:&BTreeMap<u64,u32>,active:&mut BTreeSet<u64>,out:&mut BTreeMap<u64,u32>)->Option<()> {
        if banned.contains_key(&p)||active.contains(&p)||out.len()>=24 {return None;}
        if out.contains_key(&p) {return Some(());}
        let w=*l.words.get(&p)?;active.insert(p);out.insert(p,w);
        if w!=0xd65f03c0 {
            if !(gp(w)||w&0xfc000000==0x14000000||w&0xff000010==0x54000000
                ||w&0x7e000000==0x34000000||w&0x7e000000==0x36000000) {return None;}
            for n in super::super::loops::successors(p,w) {visit(l,n,banned,active,out)?;}
        }
        active.remove(&p);Some(())
    }
    let mut out=BTreeMap::new();visit(l,start,forbidden,&mut BTreeSet::new(),&mut out)?;Some(out)
}
pub(super) fn plan(l:&super::super::loops::Loop)->Option<Plan> {
    if l.words.values().filter(|&&w|w&0xfc000000==0x94000000).count()>4 {return None;}
    let mut prefix=BTreeMap::new();let mut modified=0;let mut previous=None;
    for i in 0..12 {
        let pc=l.header.checked_add(i*4)?;let w=*l.words.get(&pc)?;
        if w&0xff000010==0x54000000 && w&15<14 {
            let cmp=previous?;let inputs=cmp_inputs(cmp)?;
            // Last CMP writes no GPR. Probe after BL's LR update, but before
            // ANY prefix effects. Prefix cannot change any predicate operand.
            if modified&inputs!=0||x86_cc(FuseKind::Sub,w&15).is_none() {return None;}
            prefix.insert(pc,w);
            let yes=pc.wrapping_add((sext(bits(w,23,5)as u64,19)<<2)as u64);let no=pc+4;
            for (entry,taken) in [(yes,true),(no,false)] {
                if let Some(body)=leaf(l,entry,&prefix) {
                    let mut lazy=BTreeMap::new();
                    // Only CMP-immediate, no leaf NZCV readers/writers. Caller
                    // continuation must definitely kill the value before ANY
                    // observer; RET/public/missing/helper paths remain live.
                    if cmp&0x7f80001f==0x7100001f&&body.values().all(|&v|!reads_flags(v)&&!writes_all_flags(v)) {
                        let entry=fun_edges::prefix(l);
                        for (&at,&call) in &l.words {if call&0xfc000000==0x94000000 {
                            if let Some(proof)=fun_edges::dead(l,at+4,entry.as_ref()) {lazy.insert(at+4,proof);}
                        }}
                    }
                    let mut words=prefix.clone();words.extend(body);
                    if words.len()>32 {continue;}
                    let mut starts:BTreeSet<u64>=l.starts.iter().filter(|p|words.contains_key(p)).copied().collect();
                    starts.insert(l.header);starts.insert(entry);
                    return Some(Plan{header:l.header,cmp,branch_pc:pc,cond:w&15,leaf:entry,taken,words,starts,lazy});
                }
            }return None;
        }
        if !gp(w)||is_terminator(w) {return None;}
        modified|=writes(w);prefix.insert(pc,w);previous=Some(w);
    }None
}
fn reset(u:&mut Unit,r:LoopRa,pc:u64,read:&dyn Fn(u64)->Option<u32>) {
    u.region=Some(r);u.rc=RegCache::new();u.vc=VCache::new();u.vzero=[0;2];u.fuse=None;u.rc_frozen=false;
    u.pending_funedge=None;u.dead_flags=false;u.dead_v=0;u.vc_dest=[0xff;2];u.tbi_mask_live=!jit_off("tbimask");
    u.loop_exit_flags=None;u.loop_exit_compare=None;if u.notbi_enabled {u.notbi.clear();}
    u.look.clear();let mut at=pc+4;while u.look.len()<LOOKAHEAD {let Some(w)=read(at)else {break;};u.look.push(w);if is_terminator(w){break;}at+=4;}
}
pub(super) fn witnesses(p:&Plan)->BTreeMap<u64,u32> {
    p.lazy.values().flat_map(|w|w.iter().map(|(&pc,&w)|(pc,w))).collect()
}
pub(super) fn emit(e:&mut E<'_>,p:Plan,next:u64) {
    assert!(e.u.recleaf_context.is_none());let r=e.u.region.unwrap();let original_labels=e.u.region_labels.clone();
    let continuation=original_labels[&next];let mask=e.u.tbi_mask_live;let look=e.u.look.clone();
    let mut miss=e.new_label();let mut full=e.new_label();let sf=bit(p.cmp,31)!=0;let n=bits(p.cmp,9,5);
    if p.cmp&0x7f80001f==0x7100001f {
        let a=e.src(n,true,sf,rax);let imm=(bits(p.cmp,21,10)<<if bit(p.cmp,22)!=0 {12}else {0})as i32;
        if sf {e.u.a.cmp(a,imm).unwrap();}else {e.u.a.cmp(r32_of(a),imm).unwrap();}
    }else {
        let a=e.src(n,false,sf,rax);let b=e.src(bits(p.cmp,20,16),false,sf,rcx);
        if sf {e.u.a.cmp(a,b).unwrap();}else {e.u.a.cmp(r32_of(a),r32_of(b)).unwrap();}
    }
    let cc=x86_cc(FuseKind::Sub,p.cond).unwrap();e.jcc(if p.taken {cc.inverse()}else {cc},miss);
    // Preserve native depth protection. Saturation uses the ORIGINAL slow call,
    // not an unbounded new stack convention or an unchecked inline shortcut.
    e.u.a.mov(rcx,qword_ptr(r15+off::HOST_SP)).unwrap();e.u.a.sub(rcx,NATIVE_RAS_LIMIT*16).unwrap();
    e.u.a.cmp(rsp,rcx).unwrap();e.u.a.jbe(full).unwrap();
    if !jit_off("tbimask") {e.u.a.mov(r11,0x00ff_ffff_ffff_ffffu64).unwrap();}
    #[cfg(test)] e.u.a.add(qword_ptr(r15+off::AOT_LOOP_REGIONS),0x100000i32).unwrap();
    let analysis=std::mem::take(&mut e.u.funedge_analysis);
    let recipe=(e.u.recleaflazy&&r.packed_flags().is_some()&&p.lazy.contains_key(&next)).then_some(Recipe{cmp:p.cmp});
    e.u.recleaf_context=Some(Context{branch_pc:p.branch_pc,leaf:p.leaf,recipe,next,continuation});
    e.u.region_labels=p.starts.iter().map(|&pc|(pc,e.u.a.create_label())).collect();
    let read=|pc|p.words.get(&pc).copied();
    for &pc in &p.starts {
        let mut label=e.u.region_labels[&pc];e.u.a.set_label(&mut label).unwrap();reset(e.u,r,pc,&read);
        let start=e.u.a.instructions().len();
        if let Some(frame)=fun_stack::plan(e.u,pc,&read) {fun_stack::emit(e.u,pc,frame);}
        else {emit_block_body(e.u,pc,&read,None,false,start);}
        // All original cloned source operations were admitted native. Calls in
        // this interval are forbidden, including conditional helpers/cold paths.
        assert!(!e.u.a.instructions()[start..].iter().any(|i|i.mnemonic()==iced_x86::Mnemonic::Call));
    }
    e.u.recleaf_context=None;e.u.funedge_analysis=analysis;e.u.region_labels=original_labels;
    reset(e.u,r,e.pc,&|_|None);e.u.look=look;e.u.tbi_mask_live=mask;
    e.u.a.set_label(&mut full).unwrap();
    #[cfg(test)] e.u.a.add(qword_ptr(r15+off::AOT_LOOP_REGIONS),0x100i32).unwrap();
    e.u.a.jmp(miss).unwrap(); // distinct labels even without test-only counter
    e.u.a.set_label(&mut miss).unwrap();e.u.recleaf_emissions+=1;
    if recipe.is_some() {e.u.recleaf_lazy_emissions+=1;if std::env::var_os("ATTESOR_RECLEAFLAZY_LOG").is_some(){eprintln!("recleaflazy-site\t{:x}",e.pc);}}
    if std::env::var_os("ATTESOR_RECLEAF_LOG").is_some() {eprintln!("recleaf-site\t{:x}\tleaf={:x}\twords={}",e.pc,p.leaf,p.words.len());}
    e.host_call(next,|e|e.goto(p.header));
}
pub(super) fn ret(e:&mut E<'_>,ctx:Context) {
    // RAX is the original RET operand, RAW, matching current interpreter/RET.
    // Never RET natively: an outer frame may have the SAME guest return PC.
    let mut mismatch=e.new_label();let mut stale=e.new_label();e.mov_imm(rcx,ctx.next);
    e.u.a.cmp(rax,rcx).unwrap();e.u.a.jne(mismatch).unwrap();
    e.u.a.mov(rcx,qword_ptr(r15+off::EPOCH_PTR)).unwrap();e.u.a.mov(rcx,qword_ptr(rcx)).unwrap();
    e.u.a.cmp(rcx,qword_ptr(r15+off::ENTRY_EPOCH)).unwrap();e.u.a.jne(stale).unwrap();
    // Exact original continuation, same assignment; don't accidentally serialize
    // then jump to an internal label as if it were a public reload entry.
    if !e.u.tbi_mask_live&&!jit_off("tbimask") {e.u.a.mov(r11,0x00ff_ffff_ffff_ffffu64).unwrap();}
    e.u.a.jmp(ctx.continuation).unwrap();
    e.u.a.set_label(&mut stale).unwrap();if let Some(recipe)=ctx.recipe {repair(e,recipe);}e.st_pc_and_exit(ctx.next);
    e.u.a.set_label(&mut mismatch).unwrap();
    // Each cold entry independently sees the operand representation. Preserve
    // RAW target while repair/flags_sub clobbers RAX; don't normalize tags.
    e.u.a.mov(rdx,rax).unwrap();if let Some(recipe)=ctx.recipe {repair(e,recipe);}
    e.u.a.mov(qword_ptr(r15+off::PC),rdx).unwrap();e.u.a.mov(qword_ptr(r15+off::EXIT_REASON),EXIT_CONTINUE as i32).unwrap();
    e.u.a.mov(qword_ptr(r15+off::EXIT_DATA),0i32).unwrap();let exit=e.dispatcher_exit();e.u.a.jmp(exit).unwrap();
}

// This slot is NOT a guest GPR: during the proven private interval it carries
// source CMP's operand, not packed NZCV. No recipe crosses an observer/public
// writeback without repair. Successful continuation must kill it first.
pub(super) fn snapshot(e:&mut E<'_>,recipe:Recipe) {
    let sf=bit(recipe.cmp,31)!=0;let dst=e.u.region.unwrap().packed_flags().unwrap();
    let src=e.src(bits(recipe.cmp,9,5),true,sf,rax);e.copy(dst,src,sf);e.u.fuse=None;
    #[cfg(test)] e.u.a.add(qword_ptr(r15+off::AOT_LOOP_REGIONS),0x200000i32).unwrap();
}
fn repair(e:&mut E<'_>,recipe:Recipe) {
    let value=e.u.region.unwrap().packed_flags().unwrap();let sf=bit(recipe.cmp,31)!=0;
    let imm=(bits(recipe.cmp,21,10)<<if bit(recipe.cmp,22)!=0 {12}else {0})as i32;
    if sf {e.u.a.cmp(value,imm).unwrap();}else {e.u.a.cmp(r32_of(value),imm).unwrap();}
    assert!(!e.u.dead_flags);e.flags_sub();
}
