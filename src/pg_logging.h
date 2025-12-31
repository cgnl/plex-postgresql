/*
 * PostgreSQL Shim - Logging Module
 * Thread-safe logging with configurable levels
 */

#ifndef PG_LOGGING_H
#define PG_LOGGING_H

// Log levels
typedef enum {
    PG_LOG_ERROR = 0,
    PG_LOG_INFO = 1,
    PG_LOG_DEBUG = 2
} pg_log_level_t;

// Initialize logging (called automatically on first log)
void pg_logging_init(void);

// Cleanup logging
void pg_logging_cleanup(void);

// Core logging function (internal - use macros below)
void pg_log_message_internal(int level, const char *fmt, ...);

// Log SQL fallback for analysis
void log_sql_fallback(const char *original_sql, const char *translated_sql,
                      const char *error_msg, const char *context);

// Check if error is a known translation limitation
int is_known_translation_limitation(const char *error_msg);

// Convenience macros
#define LOG_ERROR(fmt, ...) pg_log_message_internal(PG_LOG_ERROR, fmt, ##__VA_ARGS__)
#define LOG_INFO(fmt, ...)  pg_log_message_internal(PG_LOG_INFO, fmt, ##__VA_ARGS__)
#define LOG_DEBUG(fmt, ...) pg_log_message_internal(PG_LOG_DEBUG, fmt, ##__VA_ARGS__)

#endif // PG_LOGGING_H
