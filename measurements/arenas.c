/* Address space one thread reserves under glibc malloc.
 *
 *   cc -O2 -pthread measurements/arenas.c -o /tmp/arenas
 *   /tmp/arenas 32
 *
 * Each thread allocates 64 KiB and touches it. VmSize grows by the
 * default 8 MiB stack plus one 64 MiB arena (HEAP_MAX_SIZE on 64-bit).
 */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int nthreads;
static pthread_barrier_t bar;

static void rss(const char *tag) {
    FILE *f = fopen("/proc/self/status", "r");
    char line[256];
    long rss = -1, vms = -1;
    while (fgets(line, sizeof line, f)) {
        if (sscanf(line, "VmRSS: %ld", &rss) == 1) {
            continue;
        }
        if (sscanf(line, "VmSize: %ld", &vms) == 1) {
            continue;
        }
    }
    fclose(f);
    printf("%s threads=%d VmRSS_kB=%ld VmSize_kB=%ld\n", tag, nthreads, rss, vms);
}

static void *work(void *arg) {
    (void)arg;
    size_t n = 64 * 1024;
    char *p = malloc(n);
    if (p) {
        memset(p, 1, n);
    }
    pthread_barrier_wait(&bar);
    pthread_barrier_wait(&bar);
    return p;
}

int main(int argc, char **argv) {
    nthreads = argc > 1 ? atoi(argv[1]) : 1;
    pthread_barrier_init(&bar, NULL, (unsigned)nthreads + 1);
    rss("before");
    pthread_t *ts = calloc((size_t)nthreads, sizeof *ts);
    for (int i = 0; i < nthreads; i++) {
        pthread_create(&ts[i], NULL, work, NULL);
    }
    pthread_barrier_wait(&bar);
    rss("after");
    pthread_barrier_wait(&bar);
    for (int i = 0; i < nthreads; i++) {
        pthread_join(ts[i], NULL);
    }
    return 0;
}
