// Ghidra headless survey: metadata and at most two short analysis excerpts.
// Original Apple binaries, full decompilations and Ghidra projects are NOT exported.
// @category RosettaResearch
import ghidra.app.script.GhidraScript;
import ghidra.app.decompiler.*;
import ghidra.program.model.listing.*;
import ghidra.program.model.pcode.*;
import ghidra.program.model.symbol.*;
import java.io.*;
import java.util.*;

public class Survey extends GhidraScript {
    public void run() throws Exception {
        String[] args=getScriptArgs();
        if(args.length!=1) throw new IllegalArgumentException("output file required");
        TreeMap<String,Function> chosen=new TreeMap<>();
        ArrayList<String> references=new ArrayList<>();
        DataIterator data=currentProgram.getListing().getDefinedData(true);
        while(data.hasNext() && !monitor.isCancelled()) {
            Data d=data.next(); Object value=d.getValue();
            if(!(value instanceof String)) continue;
            String s=(String)value;
            if(!s.toLowerCase().matches("(?s).*(translat|return.address|register.alloc|print_ir|liveout|livein).*")) continue;
            ReferenceIterator refs=currentProgram.getReferenceManager().getReferencesTo(d.getAddress());
            while(refs.hasNext()) {
                Function f=getFunctionContaining(refs.next().getFromAddress());
                if(f!=null) {
                    chosen.put(f.getEntryPoint().toString(),f);
                    if(references.size()<60) references.add(f.getEntryPoint()+" "+s.replace('\n',' ').substring(0,Math.min(s.length(),140)));
                }
            }
        }
        FunctionIterator funcs=currentProgram.getFunctionManager().getFunctions(true);
        int functions=0;
        while(funcs.hasNext()) {
            Function f=funcs.next(); functions++;
            if(f.getName().toLowerCase().matches(".*(translat|resolve|return|flags).*")) chosen.put(f.getEntryPoint().toString(),f);
        }
        if(chosen.isEmpty()) {
            funcs=currentProgram.getFunctionManager().getFunctions(true);
            while(funcs.hasNext() && chosen.size()<8) {
                Function f=funcs.next(); if(!f.isExternal() && f.getBody().getNumAddresses()>48) chosen.put(f.getEntryPoint().toString(),f);
            }
        }
        DecompInterface decomp=new DecompInterface(); decomp.openProgram(currentProgram);
        try(PrintWriter out=new PrintWriter(args[0],"UTF-8")) {
            out.println("Program="+currentProgram.getName()); out.println("Language="+currentProgram.getLanguageID());
            out.println("Functions="+functions); out.println("Candidates="+chosen.size());
            out.println("Evidence is static; names and string xrefs do not prove function semantics.");
            for(String ref:references) out.println("string-xref "+ref);
            int count=0,excerpts=0;
            for(Function f:chosen.values()) {
                if(count++>=24 || monitor.isCancelled()) break;
                DecompileResults r=decomp.decompileFunction(f,12,monitor);
                out.println("function "+f.getEntryPoint()+" name="+f.getName()+" bytes="+f.getBody().getNumAddresses()+" decompiled="+r.decompileCompleted());
                if(!r.decompileCompleted()) {out.println("error="+r.getErrorMessage());continue;}
                Map<String,Integer> ops=new TreeMap<>();
                Iterator<PcodeOpAST> it=r.getHighFunction().getPcodeOps();
                while(it.hasNext()) {String op=it.next().getMnemonic();ops.put(op,ops.getOrDefault(op,0)+1);}
                out.println("pcode="+ops);
                if(excerpts++<2) {
                    String c=r.getDecompiledFunction().getC();
                    out.println("bounded-excerpt (not a full recovered source):\n"+c.substring(0,Math.min(c.length(),900)));
                }
            }
        } finally {decomp.dispose();}
    }
}
