/*
 * Benchmark raw libpq performance to establish baseline
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <libpq-fe.h>

#define ITERATIONS 10000

static inline double now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e9 + ts.tv_nsec;
}

int main(void) {
    const char *host = getenv("PLEX_PG_HOST") ?: "/tmp";
    char conninfo[256];
    snprintf(conninfo, sizeof(conninfo), "host=%s dbname=plex user=plex", host);
    
    PGconn *conn = PQconnectdb(conninfo);
    if (PQstatus(conn) != CONNECTION_OK) {
        fprintf(stderr, "Connection failed: %s\n", PQerrorMessage(conn));
        return 1;
    }
    
    PQexec(conn, "SET search_path TO plex, public");
    
    printf("Raw libpq benchmark (%d iterations)\n", ITERATIONS);
    printf("====================================\n\n");
    
    double start, total;
    PGresult *res;
    
    // Warmup
    for (int i = 0; i < 1000; i++) {
        res = PQexec(conn, "SELECT id, title FROM metadata_items WHERE id = 1");
        PQclear(res);
    }
    
    // Test 1: PQexec (simple query)
    start = now_ns();
    for (int i = 0; i < ITERATIONS; i++) {
        res = PQexec(conn, "SELECT id, title FROM metadata_items WHERE id = 1");
        PQclear(res);
    }
    total = now_ns() - start;
    printf("1. PQexec (simple):        %6.1f ns/op\n", total / ITERATIONS);
    
    // Test 2: PQexecParams (with params)
    start = now_ns();
    for (int i = 0; i < ITERATIONS; i++) {
        const char *params[1] = {"1"};
        res = PQexecParams(conn, "SELECT id, title FROM metadata_items WHERE id = $1",
                          1, NULL, params, NULL, NULL, 0);
        PQclear(res);
    }
    total = now_ns() - start;
    printf("2. PQexecParams:           %6.1f ns/op\n", total / ITERATIONS);
    
    // Test 3: PQprepare + PQexecPrepared (first call prepares, rest execute)
    res = PQprepare(conn, "stmt1", "SELECT id, title FROM metadata_items WHERE id = $1", 0, NULL);
    PQclear(res);
    
    start = now_ns();
    for (int i = 0; i < ITERATIONS; i++) {
        const char *params[1] = {"1"};
        res = PQexecPrepared(conn, "stmt1", 1, params, NULL, NULL, 0);
        PQclear(res);
    }
    total = now_ns() - start;
    printf("3. PQexecPrepared:         %6.1f ns/op\n", total / ITERATIONS);
    
    // Test 4: Same but varying param
    char param_buf[16];
    start = now_ns();
    for (int i = 0; i < ITERATIONS; i++) {
        snprintf(param_buf, sizeof(param_buf), "%d", i % 1000 + 1);
        const char *params[1] = {param_buf};
        res = PQexecPrepared(conn, "stmt1", 1, params, NULL, NULL, 0);
        PQclear(res);
    }
    total = now_ns() - start;
    printf("4. PQexecPrepared (vary):  %6.1f ns/op\n", total / ITERATIONS);
    
    PQfinish(conn);
    return 0;
}
