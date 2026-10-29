// Native middleman used to verify ARM guest -> x86 helper -> ARM guest.
#include <unistd.h>
#include <stdio.h>
int main(int argc,char **argv) { if(argc<2)return 2;execv(argv[1],argv+1);perror("execv");return 127; }
