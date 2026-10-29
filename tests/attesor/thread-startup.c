#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
static void *work(void *p) {return p;}
int main(int argc,char **argv){int rounds=argc>1?atoi(argv[1]):100;for(int r=0;r<rounds;r++){pthread_t t[4];for(int i=0;i<4;i++)if(pthread_create(t+i,0,work,(void*)(long)i))return 2;for(int i=0;i<4;i++){void *v=0;int rc=pthread_join(t[i],&v);if(rc||v!=(void*)(long)i){fprintf(stderr,"round%d thread%d join=%d value=%p want=%p\n",r,i,rc,v,(void*)(long)i);return 3;}}}printf("thread-startup PASS %d\n",rounds);return 0;}
