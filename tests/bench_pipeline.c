/*
 * Benchmark libpq pipelining (PostgreSQL 14+)
 * Pipelining sends multiple queries without waiting for results
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <libpq-fe.h>

#define ITERATIONS 10000
#define BATCH_SIZE 10

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
    
    // Check if pipelining is supported
    int server_version = PQserverVersion(conn);
    printf("PostgreSQL server version: %d.%d\n", server_version / 10000, (server_version / 100) % 100);
    
    if (server_version < 140000) {
        printf("Pipelining requires PostgreSQL 14+\n");
        PQfinish(conn);
        return 1;
    }
    
    PQexec(conn, "SET search_path TO plex, public");
    
    // Prepare statement
    PGresult *res = PQprepare(conn, "q1", "SELECT id, title FROM metadata_items WHERE id = $1", 0, NULL);
    if (PQresultStatus(res) != PGRES_COMMAND_OK) {
        fprintf(stderr, "Prepare failed: %s\n", PQerrorMessage(conn));
        PQfinish(conn);
        return 1;
    }
    PQclear(res);
    
    printf("\nPipelining benchmark (%d iterations, batch=%d)\n", ITERATIONS, BATCH_SIZE);
    printf("================================================\n\n");
    
    double start, total;
    char param_buf[16];
    
    // Warmup
    for (int i = 0; i < 100; i++) {
        const char *params[1] = {"1"};
        res = PQexecPrepared(conn, "q1", 1, params, NULL, NULL, 0);
        PQclear(res);
    }
    
    // Test 1: Sequential (baseline)
    start = now_ns();
    for (int i = 0; i < ITERATIONS; i++) {
        snprintf(param_buf, sizeof(param_buf), "%d", i % 1000 + 1);
        const char *params[1] = {param_buf};
        res = PQexecPrepared(conn, "q1", 1, params, NULL, NULL, 0);
        PQclear(res);
    }
    total = now_ns() - start;
    printf("1. Sequential:             %7.1f ns/op (%d ops/sec)\n", 
           total / ITERATIONS, (int)(ITERATIONS * 1e9 / total));
    
    // Test 2: Pipelined (batch of BATCH_SIZE)
    start = now_ns();
    for (int batch = 0; batch < ITERATIONS / BATCH_SIZE; batch++) {
        // Enter pipeline mode
        if (PQenterPipelineMode(conn) != 1) {
            fprintf(stderr, "Failed to enter pipeline mode\n");
            break;
        }
        
        // Send all queries in batch
        for (int i = 0; i < BATCH_SIZE; i++) {
            snprintf(param_buf, sizeof(param_buf), "%d", (batch * BATCH_SIZE + i) % 1000 + 1);
            const char *params[1] = {param_buf};
            if (PQsendQueryPrepared(conn, "q1", 1, params, NULL, NULL, 0) != 1) {
                fprintf(stderr, "Send failed: %s\n", PQerrorMessage(conn));
            }
        }
        
        // Mark end of pipeline
        PQpipelineSync(conn);
        
        // Collect all results
        for (int i = 0; i < BATCH_SIZE; i++) {
            res = PQgetResult(conn);
            if (res) PQclear(res);
            // Get the NULL that follows each result
            res = PQgetResult(conn);
            if (res) PQclear(res);
        }
        
        // Get the pipeline sync result
        res = PQgetResult(conn);
        if (res) PQclear(res);
        
        // Exit pipeline mode
        PQexitPipelineMode(conn);
    }
    total = now_ns() - start;
    printf("2. Pipelined (batch=%d):   %7.1f ns/op (%d ops/sec)\n", 
           BATCH_SIZE, total / ITERATIONS, (int)(ITERATIONS * 1e9 / total));
    
    // Test 3: Larger batch
    #define BATCH_SIZE_LARGE 50
    start = now_ns();
    for (int batch = 0; batch < ITERATIONS / BATCH_SIZE_LARGE; batch++) {
        PQenterPipelineMode(conn);
        
        for (int i = 0; i < BATCH_SIZE_LARGE; i++) {
            snprintf(param_buf, sizeof(param_buf), "%d", (batch * BATCH_SIZE_LARGE + i) % 1000 + 1);
            const char *params[1] = {param_buf};
            PQsendQueryPrepared(conn, "q1", 1, params, NULL, NULL, 0);
        }
        
        PQpipelineSync(conn);
        
        for (int i = 0; i < BATCH_SIZE_LARGE; i++) {
            res = PQgetResult(conn);
            if (res) PQclear(res);
            res = PQgetResult(conn);
            if (res) PQclear(res);
        }
        
        res = PQgetResult(conn);
        if (res) PQclear(res);
        
        PQexitPipelineMode(conn);
    }
    total = now_ns() - start;
    printf("3. Pipelined (batch=%d):  %7.1f ns/op (%d ops/sec)\n", 
           BATCH_SIZE_LARGE, total / ITERATIONS, (int)(ITERATIONS * 1e9 / total));
    
    PQfinish(conn);
    return 0;
}
