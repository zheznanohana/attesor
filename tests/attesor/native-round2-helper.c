// Fail if any ARM native-library shim leaks into a native middleman.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
int main(int argc,char **argv) {
    const char *inserts=getenv("DYLD_INSERT_LIBRARIES");
    if(inserts&&(strstr(inserts,"libattesornative")||strstr(inserts,"libattesorvimage")||strstr(inserts,"libattesorvdsp")||strstr(inserts,"libattesorcompression")||strstr(inserts,"libattesorcrypto")||strstr(inserts,"libattesorimageio")))return 5;
    if(argc<2)return 2;execv(argv[1],argv+1);perror("execv");return 127;
}
