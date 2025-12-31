// Test if drop_icu_root_indexes could cause crashes
#include <stdio.h>
#include <sqlite3.h>

int main() {
    sqlite3 *db = NULL;
    char *errmsg = NULL;
    
    // Open a test database
    int rc = sqlite3_open(":memory:", &db);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "Failed to open database\n");
        return 1;
    }
    
    // Try dropping a non-existent index (like our code does)
    const char *sql = "DROP INDEX IF EXISTS index_title_sort_icu";
    rc = sqlite3_exec(db, sql, NULL, NULL, &errmsg);
    
    printf("DROP INDEX result: %d\n", rc);
    if (errmsg) {
        printf("Error: %s\n", errmsg);
        sqlite3_free(errmsg);
    }
    
    sqlite3_close(db);
    printf("Test completed successfully\n");
    return 0;
}
