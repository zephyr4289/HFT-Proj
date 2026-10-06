#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <time.h>
#include <pthread.h>
#include <sched.h>
#include <immintrin.h>

#define BUFFER_SIZE (64 * 1024 * 1024) // 64 MB buffer
#define ITERATIONS 30

static inline uint64_t get_time_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC_RAW, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + ts.tv_nsec;
}

static void* worker_1t(void* arg) {
    uint8_t* buf = (uint8_t*)arg;
    uint64_t sum = 0;
    
    // Warmup
    for (size_t i = 0; i < BUFFER_SIZE; i += 64) {
        sum += buf[i];
    }
    
    uint64_t start = get_time_ns();
    for (int iter = 0; iter < ITERATIONS; iter++) {
        uint64_t* p = (uint64_t*)buf;
        size_t n = BUFFER_SIZE / sizeof(uint64_t);
        for (size_t i = 0; i < n; i += 8) {
            sum += p[i] + p[i+1] + p[i+2] + p[i+3] + p[i+4] + p[i+5] + p[i+6] + p[i+7];
        }
    }
    uint64_t elapsed_ns = get_time_ns() - start;
    
    // Prevent dead-code elimination
    if (sum == 0xdeadbeef) printf(" ");
    
    double bytes_read = (double)BUFFER_SIZE * ITERATIONS;
    double gb_s = (bytes_read / (double)elapsed_ns); // bytes/ns = GB/s
    
    double* res = (double*)malloc(sizeof(double));
    *res = gb_s;
    return res;
}

int main(int argc, char** argv) {
    uint8_t* buffer = (uint8_t*)aligned_alloc(64, BUFFER_SIZE);
    if (!buffer) return 1;
    memset(buffer, 0x5A, BUFFER_SIZE);

    // 1-Thread Test
    pthread_t th1;
    cpu_set_t cpuset;
    CPU_ZERO(&cpuset);
    CPU_SET(0, &cpuset);
    pthread_create(&th1, NULL, worker_1t, buffer);
    pthread_setaffinity_np(th1, sizeof(cpu_set_t), &cpuset);
    
    double* gb_s_1t = NULL;
    pthread_join(th1, (void**)&gb_s_1t);

    printf("BANDWIDTH_1T_GB_S=%.2f\n", gb_s_1t ? *gb_s_1t : 0.0);
    free(gb_s_1t);
    free(buffer);
    return 0;
}
