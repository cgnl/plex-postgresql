/*
 * PostgreSQL Shim - Logging Module
 * Thread-safe logging with configurable levels
 */

#include "pg_logging.h"
#include "pg_types.h"
#include <stdio.h>
#include <stdlib.h>
#include <stdarg.h>
#include <string.h>
#include <strings.h>
#include <time.h>
#include <pthread.h>

// ============================================================================
// Static State
// ============================================================================

static FILE *log_file = NULL;
static pthread_mutex_t log_mutex = PTHREAD_MUTEX_INITIALIZER;
static int current_log_level = PG_LOG_INFO;
static volatile int logging_initialized = 0;
static pthread_once_t logging_init_once = PTHREAD_ONCE_INIT;

// ============================================================================
// Throttling State (prevents disk space exhaustion from query explosions)
// ============================================================================

#define THROTTLE_WINDOW_SEC 1           // Time window for counting
#define THROTTLE_THRESHOLD 1000         // Queries per window before throttling
#define THROTTLE_SAMPLE_RATE 1000       // Log 1 in N queries when throttled
#define THROTTLE_SUMMARY_INTERVAL 10    // Log summary every N seconds when throttled

static atomic_long query_count = 0;           // Queries in current window
static atomic_long query_count_total = 0;     // Total queries since throttle started
static atomic_long suppressed_count = 0;      // Suppressed log entries
static time_t window_start = 0;               // Start of current counting window
static time_t last_summary = 0;               // Last time we logged a summary
static atomic_int throttle_active = 0;        // 1 = currently throttling

// ============================================================================
// Throttle Check (returns 1 if message should be logged, 0 if suppressed)
// ============================================================================

static int should_log_message(void) {
    time_t now = time(NULL);

    // Reset window if expired
    if (now != window_start) {
        long prev_count = atomic_exchange(&query_count, 1);
        window_start = now;

        // Check if we should exit throttle mode (low query rate)
        if (prev_count < THROTTLE_THRESHOLD / 10 && atomic_load(&throttle_active)) {
            atomic_store(&throttle_active, 0);
            long total = atomic_exchange(&query_count_total, 0);
            long suppressed = atomic_exchange(&suppressed_count, 0);
            // Log exit from throttle mode (will be logged since throttle is now off)
            if (log_file) {
                pthread_mutex_lock(&log_mutex);
                fprintf(log_file, "[%04d-%02d-%02d %02d:%02d:%02d] [INFO] THROTTLE OFF: %ld queries, %ld suppressed\n",
                        (int)(1900 + (now / 31536000) % 200), (int)((now / 2592000) % 12 + 1), (int)((now / 86400) % 31 + 1),
                        (int)((now % 86400) / 3600), (int)((now % 3600) / 60), (int)(now % 60),
                        total, suppressed);
                fflush(log_file);
                pthread_mutex_unlock(&log_mutex);
            }
        }
        return 1; // Always log first message in new window
    }

    long count = atomic_fetch_add(&query_count, 1) + 1;

    // Enter throttle mode if threshold exceeded
    if (count >= THROTTLE_THRESHOLD && !atomic_load(&throttle_active)) {
        atomic_store(&throttle_active, 1);
        atomic_store(&query_count_total, count);
        last_summary = now;
        if (log_file) {
            pthread_mutex_lock(&log_mutex);
            fprintf(log_file, "[THROTTLE] Query explosion detected: %ld queries/sec, sampling 1:%d\n",
                    count, THROTTLE_SAMPLE_RATE);
            fflush(log_file);
            pthread_mutex_unlock(&log_mutex);
        }
    }

    // If throttling, sample and log summaries
    if (atomic_load(&throttle_active)) {
        atomic_fetch_add(&query_count_total, 1);

        // Log periodic summary
        if (now - last_summary >= THROTTLE_SUMMARY_INTERVAL) {
            last_summary = now;
            long total = atomic_load(&query_count_total);
            long suppressed = atomic_load(&suppressed_count);
            if (log_file) {
                pthread_mutex_lock(&log_mutex);
                fprintf(log_file, "[THROTTLE] Status: %ld queries total, %ld suppressed, rate ~%ld/sec\n",
                        total, suppressed, count);
                fflush(log_file);
                pthread_mutex_unlock(&log_mutex);
            }
            return 1;
        }

        // Sample: only log every Nth message
        if (count % THROTTLE_SAMPLE_RATE != 0) {
            atomic_fetch_add(&suppressed_count, 1);
            return 0;
        }
    }

    return 1;
}

// ============================================================================
// Initialization
// ============================================================================

static void do_logging_init(void) {
    // Log Level from environment
    const char *level_env = getenv(ENV_PG_LOG_LEVEL);
    if (level_env) {
        if (strcasecmp(level_env, "DEBUG") == 0) current_log_level = PG_LOG_DEBUG;
        else if (strcasecmp(level_env, "ERROR") == 0) current_log_level = PG_LOG_ERROR;
        else current_log_level = PG_LOG_INFO;
    }

    // Log File from environment
    const char *file_env = getenv(ENV_PG_LOG_FILE);
    if (file_env) {
        if (strcasecmp(file_env, "stdout") == 0) {
            log_file = stdout;
        } else if (strcasecmp(file_env, "stderr") == 0) {
            log_file = stderr;
        } else {
            log_file = fopen(file_env, "a");
        }
    } else {
        log_file = fopen(LOG_FILE, "a");
    }

    if (!log_file) {
        log_file = stderr;
        fprintf(stderr, "[PG_SHIM] Failed to open log file, falling back to stderr\n");
    }

    // Unbuffered for file output
    if (log_file != stdout && log_file != stderr) {
        setbuf(log_file, NULL);
    }

    logging_initialized = 1;
}

void pg_logging_init(void) {
    pthread_once(&logging_init_once, do_logging_init);
    // Log after init is complete (can't call from do_logging_init due to recursion)
    static volatile int first_log_done = 0;
    if (!first_log_done) {
        first_log_done = 1;
        pg_log_message_internal(PG_LOG_INFO, "Logging initialized. Level: %d", current_log_level);
    }
}

void pg_logging_cleanup(void) {
    if (log_file && log_file != stdout && log_file != stderr) {
        fclose(log_file);
        log_file = NULL;
    }
    logging_initialized = 0;
}

// ============================================================================
// Core Logging (minimized mutex hold time)
// ============================================================================

void pg_log_message_internal(int level, const char *fmt, ...) {
    if (!logging_initialized) pg_logging_init();
    if (level > current_log_level) return;
    if (!log_file) return;

    // Throttle check (skip if query explosion detected)
    // Always log ERROR messages, regardless of throttle
    if (level != PG_LOG_ERROR && !should_log_message()) return;

    // HEAP allocation to prevent stack overflow (Plex uses ~388KB of stack)
    // Previously this 4KB buffer on stack caused crashes when combined with Plex's deep recursion
    #define LOG_BUFFER_SIZE 4096
    char *buffer = malloc(LOG_BUFFER_SIZE);
    if (!buffer) return;

    int offset = 0;

    // Timestamp
    time_t now = time(NULL);
    struct tm tm_buf;
    struct tm *tm = localtime_r(&now, &tm_buf);  // Thread-safe version
    offset += snprintf(buffer + offset, LOG_BUFFER_SIZE - offset,
                       "[%04d-%02d-%02d %02d:%02d:%02d] ",
                       tm->tm_year + 1900, tm->tm_mon + 1, tm->tm_mday,
                       tm->tm_hour, tm->tm_min, tm->tm_sec);

    // Level tag
    const char *tag;
    switch (level) {
        case PG_LOG_ERROR: tag = "[ERROR] "; break;
        case PG_LOG_INFO:  tag = "[INFO] "; break;
        case PG_LOG_DEBUG: tag = "[DEBUG] "; break;
        default: tag = "[???] "; break;
    }
    offset += snprintf(buffer + offset, LOG_BUFFER_SIZE - offset, "%s", tag);

    // Message
    va_list args;
    va_start(args, fmt);
    offset += vsnprintf(buffer + offset, LOG_BUFFER_SIZE - offset, fmt, args);
    va_end(args);

    // Newline
    if (offset < LOG_BUFFER_SIZE - 1) {
        buffer[offset++] = '\n';
        buffer[offset] = '\0';
    }

    // Brief mutex hold for atomic write
    pthread_mutex_lock(&log_mutex);
    fputs(buffer, log_file);
    fflush(log_file);
    pthread_mutex_unlock(&log_mutex);

    free(buffer);
}

// ============================================================================
// SQL Fallback Logging
// ============================================================================

void log_sql_fallback(const char *original_sql, const char *translated_sql,
                      const char *error_msg, const char *context) {
    // Log to main log
    pg_log_message_internal(PG_LOG_INFO, "=== SQL FALLBACK TO SQLITE ===");
    pg_log_message_internal(PG_LOG_INFO, "Context: %s", context ? context : "(null)");
    pg_log_message_internal(PG_LOG_INFO, "Original SQL: %.500s", original_sql ? original_sql : "(null)");
    if (translated_sql) {
        pg_log_message_internal(PG_LOG_INFO, "Translated SQL: %.500s", translated_sql);
    }
    pg_log_message_internal(PG_LOG_INFO, "PostgreSQL Error: %s", error_msg ? error_msg : "(null)");
    pg_log_message_internal(PG_LOG_INFO, "=== END FALLBACK ===");

    // Also log to separate fallback analysis file
    FILE *fallback_log = fopen(FALLBACK_LOG_FILE, "a");
    if (fallback_log) {
        time_t now = time(NULL);
        char timestamp[64];
        strftime(timestamp, sizeof(timestamp), "%Y-%m-%d %H:%M:%S", localtime(&now));

        fprintf(fallback_log, "\n[%s] %s\n", timestamp, context ? context : "(null)");
        fprintf(fallback_log, "ORIGINAL: %s\n", original_sql ? original_sql : "(null)");
        if (translated_sql) {
            fprintf(fallback_log, "TRANSLATED: %s\n", translated_sql);
        }
        fprintf(fallback_log, "ERROR: %s\n", error_msg ? error_msg : "(null)");
        fprintf(fallback_log, "---\n");
        fclose(fallback_log);
    }
}

// ============================================================================
// Error Classification
// ============================================================================

int is_known_translation_limitation(const char *error_msg) {
    if (!error_msg) return 0;

    // Known translation limitations (logged for improvement)
    if (strstr(error_msg, "operator does not exist: integer = json")) return 1;
    if (strstr(error_msg, "must appear in the GROUP BY clause")) return 1;
    if (strstr(error_msg, "syntax error")) return 1;
    if (strstr(error_msg, "no unique or exclusion constraint matching the ON CONFLICT")) return 1;

    return 0;
}
