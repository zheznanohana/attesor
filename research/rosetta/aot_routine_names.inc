/* ABI routine-name list from athei/x87sidecar
   499e0f7e60d2961d379df459dd96bc79ecfe59fd, RosettaAotApi.cpp.

MIT License

Copyright (c) 2025 Lifeisawful

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

*/
static const char* const reference_routine_names[]={
    "branch_slot",
    "indirect_jmp",
    "indirect_jmp_dyld_stub",
    "far_jmp",
    "indirect_call",
    "far_call",
    "return_stack_miss",
    "far_ret",
    "far_ret_single_step",
    "syscall_handler",
    "toggle_single_step",
    "cpuid",
    "wide_udiv_64",
    "wide_sdiv_64",
    "pcmpestri",
    "pcmpestrm",
    "pcmpistri",
    "pcmpistrm",
    "mov_segment",
    "mov_segment_and_reg",
    "get_cpu_number",
    "get_tls_base",
    "read_timer_nanoseconds",
    "read_mxcsr",
    "mxcsr_to_fpcr_fpsr",
    "f2xm1",
    "fabs",
    "fadd_f32",
    "fadd_f64",
    "fadd_ST",
    "fbld",
    "fbstp",
    "fchs",
    "fcmov",
    "fcom_f32",
    "fcom_f64",
    "fcom_ST",
    "fcomi",
    "fcos",
    "fdecstp",
    "fdiv_f32",
    "fdiv_f64",
    "fdiv_ST",
    "fdivr_f32",
    "fdivr_f64",
    "fdivr_ST",
    "fiadd",
    "ficom",
    "fidiv",
    "fidivr",
    "fild",
    "fimul",
    "fincstp",
    "fist_i16",
    "fist_i32",
    "fist_i64",
    "fistt_i16",
    "fistt_i32",
    "fistt_i64",
    "fisub",
    "fisubr",
    "fld_fp32",
    "fld_fp64",
    "fld_fp80",
    "fld_constant",
    "fld_STi",
    "fmul_f32",
    "fmul_f64",
    "fmul_ST",
    "fpatan",
    "fprem",
    "fprem1",
    "fptan",
    "frndint",
    "fscale",
    "fsin",
    "fsincos",
    "fsqrt",
    "fst_fp32",
    "fst_fp64",
    "fst_fp80",
    "fst_STi",
    "fsub_f32",
    "fsub_f64",
    "fsub_ST",
    "fsubr_f32",
    "fsubr_f64",
    "fsubr_ST",
    "fucom",
    "fucomi",
    "fxam",
    "fxch",
    "fxtract",
    "fyl2x",
    "fyl2xp1",
    "ffree",
    "load_segment_limit",
    "rdrand",
    "xrstor",
    "xsave",
};
