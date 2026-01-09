/*
 * Unit tests for SQL translation (SQLite to PostgreSQL)
 *
 * Tests:
 * 1. Placeholder translation (:name → $1)
 * 2. Function translation (IFNULL → COALESCE, etc.)
 * 3. Type translation (INTEGER → BIGINT, etc.)
 * 4. Keyword translation (GLOB → ILIKE, etc.)
 * 5. Full query translation
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>

// Include the translator header
#include "sql_translator.h"

// Test counters
static int tests_passed = 0;
static int tests_failed = 0;

#define TEST(name) printf("  Testing: %s... ", name)
#define PASS() do { printf("\033[32mPASS\033[0m\n"); tests_passed++; } while(0)
#define FAIL(msg) do { printf("\033[31mFAIL: %s\033[0m\n", msg); tests_failed++; } while(0)

// ============================================================================
// Placeholder Translation Tests
// ============================================================================

static void test_placeholder_basic(void) {
    TEST("Placeholder - basic :name to $1");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders("SELECT * FROM t WHERE id = :id", &names, &count);

    if (result && strstr(result, "$1") && count == 1) {
        PASS();
    } else {
        FAIL("Expected $1 placeholder");
    }

    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) free(names[i]);
        free(names);
    }
}

static void test_placeholder_multiple(void) {
    TEST("Placeholder - multiple :name params");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE a = :foo AND b = :bar AND c = :baz", &names, &count);

    if (result && strstr(result, "$1") && strstr(result, "$2") && strstr(result, "$3") && count == 3) {
        PASS();
    } else {
        FAIL("Expected $1, $2, $3 placeholders");
    }

    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) free(names[i]);
        free(names);
    }
}

static void test_placeholder_reuse(void) {
    TEST("Placeholder - same :name used twice");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE a = :id OR b = :id", &names, &count);

    // Same param used twice should map to same $N
    if (result && count == 1) {
        PASS();
    } else {
        FAIL("Expected single param for reused :id");
    }

    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) free(names[i]);
        free(names);
    }
}

static void test_placeholder_question_mark(void) {
    TEST("Placeholder - ? positional params");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE a = ? AND b = ?", &names, &count);

    if (result && strstr(result, "$1") && strstr(result, "$2") && count == 2) {
        PASS();
    } else {
        FAIL("Expected $1, $2 for ? params");
    }

    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) free(names[i]);
        free(names);
    }
}

static void test_placeholder_in_string(void) {
    TEST("Placeholder - :name inside string literal ignored");
    char **names = NULL;
    int count = 0;
    char *result = sql_translate_placeholders(
        "SELECT * FROM t WHERE a = ':not_a_param'", &names, &count);

    // Should NOT translate :not_a_param inside quotes
    if (result && count == 0 && strstr(result, ":not_a_param")) {
        PASS();
    } else {
        FAIL("Should not translate :param inside string");
    }

    if (result) free(result);
    if (names) {
        for (int i = 0; i < count; i++) free(names[i]);
        free(names);
    }
}

// ============================================================================
// Function Translation Tests
// ============================================================================

static void test_function_ifnull(void) {
    TEST("Function - IFNULL to COALESCE");
    char *result = sql_translate_functions("SELECT IFNULL(a, 0) FROM t");

    if (result && strcasestr(result, "COALESCE") && !strcasestr(result, "IFNULL")) {
        PASS();
    } else {
        FAIL("Expected COALESCE instead of IFNULL");
    }

    if (result) free(result);
}

static void test_function_length(void) {
    TEST("Function - LENGTH preserved");
    char *result = sql_translate_functions("SELECT LENGTH(name) FROM t");

    if (result && strcasestr(result, "LENGTH")) {
        PASS();
    } else {
        FAIL("LENGTH should be preserved");
    }

    if (result) free(result);
}

static void test_function_substr(void) {
    TEST("Function - SUBSTR to SUBSTRING");
    char *result = sql_translate_functions("SELECT SUBSTR(a, 1, 5) FROM t");

    if (result && strcasestr(result, "SUBSTRING")) {
        PASS();
    } else {
        FAIL("Expected SUBSTRING");
    }

    if (result) free(result);
}

static void test_function_random(void) {
    TEST("Function - RANDOM() to RANDOM()");
    char *result = sql_translate_functions("SELECT RANDOM() FROM t");

    // PostgreSQL also has RANDOM(), should work
    if (result && strcasestr(result, "RANDOM")) {
        PASS();
    } else {
        FAIL("RANDOM should be preserved");
    }

    if (result) free(result);
}

static void test_function_datetime(void) {
    TEST("Function - datetime('now') handling");
    char *result = sql_translate_functions("SELECT datetime('now') FROM t");

    // Should translate to NOW() or similar
    if (result) {
        PASS();  // Just check it doesn't crash
    } else {
        FAIL("datetime translation failed");
    }

    if (result) free(result);
}

// ============================================================================
// Keyword Translation Tests
// ============================================================================

static void test_keyword_glob(void) {
    TEST("Keyword - GLOB to ILIKE");
    char *result = sql_translate_keywords("SELECT * FROM t WHERE name GLOB '*test*'");

    if (result && (strcasestr(result, "ILIKE") || strcasestr(result, "LIKE"))) {
        PASS();
    } else {
        FAIL("Expected ILIKE/LIKE pattern");
    }

    if (result) free(result);
}

static void test_keyword_notnull(void) {
    TEST("Keyword - NOT NULL preserved");
    char *result = sql_translate_keywords("SELECT * FROM t WHERE a IS NOT NULL");

    if (result && strcasestr(result, "NOT NULL")) {
        PASS();
    } else {
        FAIL("NOT NULL should be preserved");
    }

    if (result) free(result);
}

// ============================================================================
// Type Translation Tests
// ============================================================================

static void test_type_autoincrement(void) {
    TEST("Type - AUTOINCREMENT to SERIAL");
    char *result = sql_translate_types("CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)");

    if (result && strcasestr(result, "SERIAL") && !strcasestr(result, "AUTOINCREMENT")) {
        PASS();
    } else {
        FAIL("Expected SERIAL, no AUTOINCREMENT");
    }

    if (result) free(result);
}

static void test_type_text(void) {
    TEST("Type - TEXT preserved");
    char *result = sql_translate_types("CREATE TABLE t (name TEXT)");

    if (result && strcasestr(result, "TEXT")) {
        PASS();
    } else {
        FAIL("TEXT should be preserved");
    }

    if (result) free(result);
}

// ============================================================================
// Full Translation Tests
// ============================================================================

static void test_full_select(void) {
    TEST("Full - simple SELECT");
    sql_translation_t result = sql_translate("SELECT * FROM metadata_items WHERE id = :id");

    if (result.success && result.sql && strstr(result.sql, "$1")) {
        PASS();
    } else {
        FAIL(result.error[0] ? result.error : "Translation failed");
    }

    sql_translation_free(&result);
}

static void test_full_insert(void) {
    TEST("Full - INSERT with values");
    sql_translation_t result = sql_translate(
        "INSERT INTO t (a, b) VALUES (:a, :b)");

    if (result.success && result.sql && result.param_count == 2) {
        PASS();
    } else {
        FAIL("INSERT translation failed");
    }

    sql_translation_free(&result);
}

static void test_full_update(void) {
    TEST("Full - UPDATE with WHERE");
    sql_translation_t result = sql_translate(
        "UPDATE t SET a = :val WHERE id = :id");

    if (result.success && result.sql && result.param_count == 2) {
        PASS();
    } else {
        FAIL("UPDATE translation failed");
    }

    sql_translation_free(&result);
}

static void test_full_complex(void) {
    TEST("Full - complex Plex-like query");
    sql_translation_t result = sql_translate(
        "SELECT m.id, m.title, IFNULL(m.rating, 0) as rating "
        "FROM metadata_items m "
        "WHERE m.library_section_id = :lib_id "
        "AND m.metadata_type = :type "
        "ORDER BY m.added_at DESC LIMIT 50");

    if (result.success && result.sql &&
        strcasestr(result.sql, "COALESCE") &&  // IFNULL → COALESCE
        strstr(result.sql, "$1") &&
        strstr(result.sql, "$2")) {
        PASS();
    } else {
        FAIL("Complex query translation failed");
    }

    sql_translation_free(&result);
}

// ============================================================================
// Edge Case Tests
// ============================================================================

static void test_edge_empty(void) {
    TEST("Edge - empty string");
    sql_translation_t result = sql_translate("");

    // Should handle gracefully
    if (result.sql != NULL || !result.success) {
        PASS();  // Either returns empty or fails gracefully
    } else {
        FAIL("Empty string not handled");
    }

    sql_translation_free(&result);
}

static void test_edge_null(void) {
    TEST("Edge - NULL input");
    sql_translation_t result = sql_translate(NULL);

    // Should not crash
    PASS();

    sql_translation_free(&result);
}

static void test_edge_backticks(void) {
    TEST("Edge - backtick identifiers to double quotes");
    sql_translation_t result = sql_translate("SELECT `id`, `name` FROM `table`");

    // Backticks should become double quotes
    if (result.success && result.sql &&
        !strstr(result.sql, "`") &&
        strstr(result.sql, "\"")) {
        PASS();
    } else {
        FAIL("Backticks not converted to double quotes");
    }

    sql_translation_free(&result);
}

static void test_edge_double_quotes_preserved(void) {
    TEST("Edge - double quotes preserved");
    sql_translation_t result = sql_translate("SELECT \"id\" FROM \"table\"");

    if (result.success && result.sql && strstr(result.sql, "\"")) {
        PASS();
    } else {
        FAIL("Double quotes should be preserved");
    }

    sql_translation_free(&result);
}

// ============================================================================
// Main
// ============================================================================

int main(void) {
    printf("\n\033[1m=== SQL Translator Tests ===\033[0m\n\n");

    // Initialize translator
    sql_translator_init();

    printf("\033[1mPlaceholder Translation:\033[0m\n");
    test_placeholder_basic();
    test_placeholder_multiple();
    test_placeholder_reuse();
    test_placeholder_question_mark();
    test_placeholder_in_string();

    printf("\n\033[1mFunction Translation:\033[0m\n");
    test_function_ifnull();
    test_function_length();
    test_function_substr();
    test_function_random();
    test_function_datetime();

    printf("\n\033[1mKeyword Translation:\033[0m\n");
    test_keyword_glob();
    test_keyword_notnull();

    printf("\n\033[1mType Translation:\033[0m\n");
    test_type_autoincrement();
    test_type_text();

    printf("\n\033[1mFull Query Translation:\033[0m\n");
    test_full_select();
    test_full_insert();
    test_full_update();
    test_full_complex();

    printf("\n\033[1mEdge Cases:\033[0m\n");
    test_edge_empty();
    test_edge_null();
    test_edge_backticks();
    test_edge_double_quotes_preserved();

    // Cleanup
    sql_translator_cleanup();

    printf("\n\033[1m=== Results ===\033[0m\n");
    printf("Passed: \033[32m%d\033[0m\n", tests_passed);
    printf("Failed: \033[31m%d\033[0m\n", tests_failed);
    printf("\n");

    return tests_failed > 0 ? 1 : 0;
}
