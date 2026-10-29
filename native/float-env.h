// Host bridge uses default IEEE rounding/denormal policy. Non-default guest
// FPCR modes must stay in the guest library rather than silently ignoring them.
static int attesor_default_fpcr(void) {
    unsigned long long value;
    __asm__ volatile("mrs %0, fpcr" : "=r"(value));
    return value==0;
}
