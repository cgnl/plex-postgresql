/*
 * Test INSERT OR REPLACE translation
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "include/sql_translator.h"

void test_translation(const char *input, const char *expected_pattern) {
    printf("\n=== TEST ===\n");
    printf("Input:    %s\n", input);

    sql_translation_t result = sql_translate(input);

    if (!result.success) {
        printf("ERROR:    %s\n", result.error);
        sql_translation_free(&result);
        return;
    }

    printf("Output:   %s\n", result.sql);

    if (expected_pattern && strstr(result.sql, expected_pattern)) {
        printf("PASS:     Found expected pattern: %s\n", expected_pattern);
    } else if (expected_pattern) {
        printf("FAIL:     Expected pattern not found: %s\n", expected_pattern);
    }

    sql_translation_free(&result);
}

int main(void) {
    sql_translator_init();

    printf("==========================================================\n");
    printf("INSERT OR REPLACE Translation Tests\n");
    printf("==========================================================\n");

    // Test 1: Simple INSERT OR REPLACE with column list
    test_translation(
        "INSERT OR REPLACE INTO tags (id, tag, tag_type) VALUES (1, 'Action', 0)",
        "ON CONFLICT (id) DO UPDATE SET"
    );

    // Test 2: metadata_items with multiple columns
    test_translation(
        "INSERT OR REPLACE INTO metadata_items (id, title, metadata_type, library_section_id) VALUES (1, 'Test Movie', 1, 1)",
        "ON CONFLICT (id) DO UPDATE SET"
    );

    // Test 3: preferences with UNIQUE constraint on name
    test_translation(
        "INSERT OR REPLACE INTO preferences (id, name, value) VALUES (1, 'theme', 'dark')",
        "ON CONFLICT (name) DO UPDATE SET"
    );

    // Test 4: statistics_media (should get ON CONFLICT)
    test_translation(
        "INSERT OR REPLACE INTO statistics_media (id, account_id, device_id) VALUES (100, 1, 1)",
        "ON CONFLICT (id) DO UPDATE SET"
    );

    // Test 5: metadata_item_settings (should use existing custom logic)
    test_translation(
        "INSERT OR REPLACE INTO metadata_item_settings (id, account_id, guid, rating) VALUES (1, 1, 'plex://movie/1', 5.0)",
        "INSERT INTO"  // Should be converted to INSERT, ON CONFLICT added later
    );

    // Test 6: Complex query with WHERE clause
    test_translation(
        "INSERT OR REPLACE INTO taggings (id, metadata_item_id, tag_id, created_at) VALUES (1, 100, 50, 1234567890)",
        "ON CONFLICT (id) DO UPDATE SET"
    );

    printf("\n==========================================================\n");
    printf("All tests completed\n");
    printf("==========================================================\n");

    sql_translator_cleanup();
    return 0;
}
