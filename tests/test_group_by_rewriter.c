/*
 * Test Cases for GROUP BY Query Rewriter
 * Tests the complete GROUP BY rewriter functionality
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>

// Declare the function we're testing
char* fix_group_by_strict_complete(const char *sql);

// Test helper
static int test_query(const char *name, const char *input, const char *expected) {
    printf("TEST: %s\n", name);
    printf("  Input: %.100s...\n", input);

    char *result = fix_group_by_strict_complete(input);
    if (!result) {
        printf("  FAIL: Returned NULL\n\n");
        return 0;
    }

    int match = (strstr(result, expected) != NULL);
    if (match) {
        printf("  PASS: Contains expected: %s\n\n", expected);
    } else {
        printf("  FAIL: Expected substring not found\n");
        printf("  Expected: %s\n", expected);
        printf("  Got: %.200s\n\n", result);
    }

    free(result);
    return match;
}

int main() {
    int passed = 0;
    int total = 0;

    printf("=== GROUP BY Rewriter Test Suite ===\n\n");

    // Test 1: Simple case - add missing column
    total++;
    if (test_query(
        "Simple missing column",
        "SELECT metadata_items.id, metadata_items.title FROM metadata_items GROUP BY metadata_items.id",
        "GROUP BY metadata_items.id,metadata_items.title"
    )) passed++;

    // Test 2: Multiple missing columns
    total++;
    if (test_query(
        "Multiple missing columns",
        "SELECT metadata_items.id, metadata_items.library_section_id, metadata_items.title FROM metadata_items GROUP BY metadata_items.id",
        "GROUP BY metadata_items.id,metadata_items.library_section_id,metadata_items.title"
    )) passed++;

    // Test 3: With aggregate function
    total++;
    if (test_query(
        "With COUNT aggregate",
        "SELECT metadata_items.id, metadata_items.title, COUNT(*) as cnt FROM metadata_items GROUP BY metadata_items.id",
        "GROUP BY metadata_items.id,metadata_items.title"
    )) passed++;

    // Test 4: GROUP BY with HAVING clause
    total++;
    if (test_query(
        "GROUP BY with HAVING",
        "SELECT metadata_items.id, metadata_items.title, COUNT(*) FROM metadata_items GROUP BY metadata_items.id HAVING COUNT(*) > 1",
        "GROUP BY metadata_items.id,metadata_items.title HAVING"
    )) passed++;

    // Test 5: GROUP BY with ORDER BY
    total++;
    if (test_query(
        "GROUP BY with ORDER BY",
        "SELECT metadata_items.id, metadata_items.title FROM metadata_items GROUP BY metadata_items.id ORDER BY metadata_items.title",
        "GROUP BY metadata_items.id,metadata_items.title ORDER BY"
    )) passed++;

    // Test 6: Quoted column names
    total++;
    if (test_query(
        "Quoted column names",
        "SELECT metadata_items.id, metadata_items.\"index\" FROM metadata_items GROUP BY metadata_items.id",
        "GROUP BY metadata_items.id,metadata_items.\"index\""
    )) passed++;

    // Test 7: Multiple aggregate functions
    total++;
    if (test_query(
        "Multiple aggregates",
        "SELECT metadata_items.guid, COUNT(DISTINCT views.id) as cnt, group_concat(views.account_id) as ids FROM metadata_items GROUP BY metadata_items.guid",
        "GROUP BY metadata_items.guid"
    )) passed++;

    // Test 8: Real Plex query - metadata_items with views
    total++;
    if (test_query(
        "Real Plex metadata_items query",
        "select metadata_items.id, metadata_items.library_section_id, metadata_items.title, count(distinct metadata_item_views.id) as globalViewCount from metadata_item_views left join metadata_items on metadata_items.guid=metadata_item_views.guid where metadata_items.metadata_type=$1 group by metadata_items.guid order by globalViewCount desc limit 6",
        "GROUP BY metadata_items.guid,metadata_items.id,metadata_items.library_section_id,metadata_items.title"
    )) passed++;

    // Test 9: No GROUP BY - should return unchanged
    total++;
    const char *no_groupby = "SELECT * FROM metadata_items WHERE id = 1";
    char *result = fix_group_by_strict_complete(no_groupby);
    if (result && strcmp(result, no_groupby) == 0) {
        printf("TEST: No GROUP BY\n  PASS: Returned unchanged\n\n");
        passed++;
    } else {
        printf("TEST: No GROUP BY\n  FAIL: Should return unchanged\n\n");
    }
    free(result);

    // Test 10: Already complete GROUP BY - should return unchanged
    total++;
    const char *complete_groupby = "SELECT metadata_items.id, metadata_items.title FROM metadata_items GROUP BY metadata_items.id, metadata_items.title";
    result = fix_group_by_strict_complete(complete_groupby);
    if (result && strstr(result, "GROUP BY metadata_items.id") != NULL) {
        printf("TEST: Complete GROUP BY\n  PASS: Preserved GROUP BY\n\n");
        passed++;
    } else {
        printf("TEST: Complete GROUP BY\n  FAIL\n\n");
    }
    free(result);

    // Test 11: CASE expression - should not add to GROUP BY
    total++;
    if (test_query(
        "CASE expression",
        "SELECT metadata_items.id, CASE WHEN metadata_items.rating > 5 THEN 'high' ELSE 'low' END as rating_cat FROM metadata_items GROUP BY metadata_items.id",
        "GROUP BY metadata_items.id"
    )) passed++;

    // Test 12: Aliased columns with AS
    total++;
    if (test_query(
        "Aliased columns",
        "SELECT metadata_items.id AS item_id, metadata_items.title AS item_title FROM metadata_items GROUP BY metadata_items.id",
        "GROUP BY metadata_items.id,metadata_items.title"
    )) passed++;

    // Test 13: Table aliases in join
    total++;
    if (test_query(
        "Table aliases in join",
        "SELECT m.id, m.title, COUNT(*) FROM metadata_items m JOIN media_items mi ON mi.metadata_item_id = m.id GROUP BY m.id",
        "GROUP BY m.id,m.title"
    )) passed++;

    // Test 14: Real Plex query - media_items with parents
    total++;
    if (test_query(
        "Complex Plex query with parents",
        "select media_items.id, metadata_items.id, metadata_items.title, parents.title, count(distinct views.id) as cnt from metadata_items left join media_items on media_items.metadata_item_id=metadata_items.id left join metadata_items as parents on parents.id=metadata_items.parent_id left join metadata_item_views as views on views.guid=metadata_items.guid group by metadata_items.guid",
        "GROUP BY metadata_items.guid,media_items.id,metadata_items.id,metadata_items.title,parents.title"
    )) passed++;

    // Test 15: GROUP_CONCAT aggregate
    total++;
    if (test_query(
        "GROUP_CONCAT aggregate",
        "SELECT metadata_items.id, metadata_items.title, group_concat(tags.tag, ',') as tags FROM metadata_items GROUP BY metadata_items.id",
        "GROUP BY metadata_items.id,metadata_items.title"
    )) passed++;

    // Test 16: Subquery in SELECT - should not add to GROUP BY
    total++;
    if (test_query(
        "Subquery in SELECT",
        "SELECT metadata_items.id, (SELECT COUNT(*) FROM media_items WHERE metadata_item_id = metadata_items.id) as media_count FROM metadata_items GROUP BY metadata_items.id",
        "GROUP BY metadata_items.id"
    )) passed++;

    // Summary
    printf("=== Test Results ===\n");
    printf("Passed: %d/%d\n", passed, total);
    printf("Failed: %d/%d\n", total - passed, total);

    if (passed == total) {
        printf("\nAll tests PASSED!\n");
        return 0;
    } else {
        printf("\nSome tests FAILED\n");
        return 1;
    }
}
