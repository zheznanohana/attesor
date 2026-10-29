#include "../../gpu/attesor_exec_arch.h"
#include <stdio.h>
int main(int argc, char **argv) { if (argc != 2) return 2; printf("%d\n", attesor_needs_translation(argv[1])); return 0; }
