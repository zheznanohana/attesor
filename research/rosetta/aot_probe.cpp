// Independently authored bounded, in-process Rosetta AOT observation probe.
// API discovery: athei/x87sidecar 499e0f7e60d2961d379df459dd96bc79ecfe59fd,
// aotinvoke/src/RosettaAotApi.{h,cpp}. No hooks, target attachment or SIP changes.
// Function types follow the actual exported C++ mangling, including BY-VALUE
// vectors, rather than treating non-trivial value arguments as references.
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <dlfcn.h>
#include <stdexcept>
#include <string>
#include <vector>
#include <sys/mman.h>
#include <mach-o/loader.h>
#include <mach/vm_prot.h>

template<class T> struct Interval {T low,high;};
struct DataEntry {uint32_t offset;uint16_t length,kind;};
struct Module;
struct Translation;
template<class F> F symbol(void* h,const char* name) {
    auto p=dlsym(h,name);if(!p)throw std::runtime_error(std::string("missing export: ")+name);
    return reinterpret_cast<F>(p);
}
// Observe this library's own resolved, read-only ABI name table instead of
// guessing a version-specific count or copying a stale third-party list.
// Only string pointers within its __cstring section may be dereferenced.
std::vector<const char*> routine_names(void* exported_function) {
    Dl_info info={};
    if(!dladdr(exported_function,&info)||!info.dli_fbase)throw std::runtime_error("dladdr failed");
    auto h=static_cast<const mach_header_64*>(info.dli_fbase);
    if(h->magic!=MH_MAGIC_64||h->ncmds>1024)throw std::runtime_error("unexpected library header");
    uintptr_t slide=0,strings_low=0,strings_high=0;
    std::vector<const segment_command_64*> segments;
    auto cmd=reinterpret_cast<const load_command*>(h+1);
    for(unsigned i=0;i<h->ncmds;i++) {
        if(cmd->cmd==LC_SEGMENT_64) {
            auto s=reinterpret_cast<const segment_command_64*>(cmd);segments.push_back(s);
            if(!strcmp(s->segname,"__TEXT"))slide=reinterpret_cast<uintptr_t>(h)-s->vmaddr;
        }
        cmd=reinterpret_cast<const load_command*>(reinterpret_cast<const char*>(cmd)+cmd->cmdsize);
    }
    for(auto s:segments) {
        auto sections=reinterpret_cast<const section_64*>(s+1);
        for(unsigned j=0;j<s->nsects;j++)if(!strcmp(sections[j].sectname,"__cstring")) {
            strings_low=slide+sections[j].addr;strings_high=strings_low+sections[j].size;
        }
    }
    if(!strings_low)throw std::runtime_error("no string section");
    auto valid=[&](uintptr_t p) {
        if(p<strings_low||p>=strings_high)return false;
        unsigned n=0;
        for(;n<80&&p+n<strings_high;n++) {
            auto c=*reinterpret_cast<const unsigned char*>(p+n);
            if(c==0)return n>0;
            if(!((c>='a'&&c<='z')||(c>='A'&&c<='Z')||(c>='0'&&c<='9')||c=='_'))return false;
        }
        return false;
    };
    std::vector<const char*> found;
    for(auto s:segments) {
        if(!(s->initprot&VM_PROT_READ)||!strcmp(s->segname,"__LINKEDIT"))continue;
        const auto low=(slide+s->vmaddr+7)&~uintptr_t(7);
        const auto high=slide+s->vmaddr+s->filesize;
        for(auto at=low;at+24<=high;at+=8) {
            auto table=reinterpret_cast<const uintptr_t*>(at);
            if(!valid(table[0])||strcmp(reinterpret_cast<const char*>(table[0]),"branch_slot")
                ||!valid(table[1])||strcmp(reinterpret_cast<const char*>(table[1]),"indirect_jmp")
                ||!valid(table[2])||strcmp(reinterpret_cast<const char*>(table[2]),"indirect_jmp_dyld_stub"))continue;
            std::vector<const char*> candidate;
            for(unsigned j=0;j<128&&at+8*(j+1)<=high&&valid(table[j]);j++)candidate.push_back(reinterpret_cast<const char*>(table[j]));
            if(candidate.size()<32||candidate.size()>=128)continue;
            if(!found.empty()&&found!=candidate)throw std::runtime_error("ambiguous ABI name table");
            found=candidate;
        }
    }
    if(found.empty())throw std::runtime_error("no bounded ABI name table");
    return found;
}
int main(int argc,char** argv) try {
    if(argc!=4)throw std::runtime_error("usage: aot_probe arith|flags|fpchain|loop ir|translate output.bin");
    std::vector<uint8_t> bytes;
    const std::string name=argv[1];
    if(name=="arith")bytes={0x89,0xf8,0x83,0xc0,0x01,0xc3};
    else if(name=="flags")bytes={0x83,0xff,0,0x0f,0x95,0xc0,0x0f,0xb6,0xc0,0xc3};
    else if(name=="fpchain")bytes={0xf2,0x0f,0x59,0xc1,0xf2,0x0f,0x58,0xd0,0xc3};
    else if(name=="loop")bytes={0x89,0xf8,0x83,0xe8,1,0x75,0xfb,0xc3};
    else throw std::runtime_error("unknown bounded case");
    const std::string mode=argv[2];
    if(mode!="ir"&&mode!="translate")throw std::runtime_error("unknown mode");
    constexpr auto path="/Library/Apple/usr/lib/libRosettaAot.dylib";
    void* h=dlopen(path,RTLD_NOW|RTLD_LOCAL);
    if(!h)throw std::runtime_error(std::string("dlopen: ")+dlerror());
    auto version=symbol<uint64_t(*)()>(h,"_ZN7rosetta14librosetta_aot7versionEv")();
    printf("CASE %s VERSION 0x%llx INPUT ",name.c_str(),(unsigned long long)version);
    for(auto c:bytes)printf("%02x",c);puts("");fflush(stdout);
    using Create=Module*(*)(const uint8_t*,Interval<uint64_t>,Interval<uint64_t>,Interval<uint32_t>,
        Interval<uint64_t>,Interval<uint32_t>,unsigned,std::vector<uint32_t>,std::vector<DataEntry>);
    auto create=symbol<Create>(h,"_ZN7rosetta14librosetta_aot9ir_createEPKh8IntervalIyES4_S3_IjES4_S5_jNSt3__16vectorIjNS6_9allocatorIjEEEENS7_INS_5macho15DataInCodeEntryENS8_ISC_EEEE");
    auto print=symbol<void(*)(const Module*,int)>(h,"_ZN7rosetta14librosetta_aot12module_printEPKNS0_12ModuleResultEi");
    auto free_module=symbol<void(*)(const Module*)>(h,"_ZN7rosetta14librosetta_aot11module_freeEPKNS0_12ModuleResultE");
    auto input=static_cast<uint8_t*>(mmap(nullptr,16384,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANON,-1,0));
    if(input==MAP_FAILED)throw std::runtime_error("mmap failed");
    memcpy(input,bytes.data(),bytes.size());
    Module* module=create(input,{0,0},{0,0},{0,(uint32_t)bytes.size()},{0,UINT64_MAX},{0,0},0,{0},{});
    if(!module)throw std::runtime_error("null IR module");
    puts("IR_BEGIN");fflush(stdout);print(module,1);puts("IR_END");fflush(stdout);
    if(mode=="translate") {
        // Synthetic metadata makes this a CODE-SHAPE probe, NOT a runnable
        // Rosetta process ABI. Output is never mapped executable or executed.
        // No copyrighted runtime helper code or protected cache is copied.
        uint32_t context_offsets[]={0,8,16,24,32,64};
        symbol<void(*)(const uint32_t*)>(h,"_ZN7rosetta14librosetta_aot31register_thread_context_offsetsEPKNS_20ThreadContextOffsetsE")(context_offsets);
        auto register_offsets=symbol<void(*)(const uint32_t*,const char**,uint64_t)>(h,"_ZN7rosetta14librosetta_aot32register_runtime_routine_offsetsEPKjPPKcm");
        auto names=routine_names(reinterpret_cast<void*>(register_offsets));
        std::vector<uint32_t> offsets(names.size());
        for(unsigned i=0;i<offsets.size();i++)offsets[i]=i*16;
        printf("OBSERVED_ABI_ROUTINES %zu SYNTHETIC_OFFSETS=1\n",names.size());fflush(stdout);
        register_offsets(offsets.data(),names.data(),names.size());
        auto result=symbol<Translation*(*)(const Module*)>(h,"_ZN7rosetta14librosetta_aot9translateEPKNS0_12ModuleResultE")(module);
        if(!result)throw std::runtime_error("null translation");
        auto size=symbol<uint64_t(*)(const Translation*)>(h,"_ZN7rosetta14librosetta_aot19translator_get_sizeEPKNS0_17TranslationResultE")(result);
        auto data=symbol<const uint8_t*(*)(Translation*)>(h,"_ZN7rosetta14librosetta_aot19translator_get_dataEPNS0_17TranslationResultE")(result);
        if(!data||size==0||size>65536)throw std::runtime_error("invalid/beyond-budget generated bytes");
        FILE* out=fopen(argv[3],"wb");if(!out)throw std::runtime_error("output open failed");
        auto written=fwrite(data,1,size,out);fclose(out);
        if(written!=size)throw std::runtime_error("short output write");
        printf("GENERATED_BYTES %llu SYNTHETIC_CONTEXT=1 EXECUTED=0 FIXUPS_APPLIED=0\n",(unsigned long long)size);
        symbol<void(*)(const Translation*)>(h,"_ZN7rosetta14librosetta_aot15translator_freeEPKNS0_17TranslationResultE")(result);
    }
    free_module(module);munmap(input,16384);dlclose(h);puts("PROBE_OK");return 0;
} catch(const std::exception& e) {fprintf(stderr,"PROBE_FAILED %s\n",e.what());return 1;}
