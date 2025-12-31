// Analyze if prepare_v3 routing to prepare_v2 is safe
#include <stdio.h>
#include <sqlite3.h>

// Test if ignoring prepFlags causes issues
int main() {
    sqlite3 *db = NULL;
    sqlite3_stmt *stmt = NULL;
    const char *tail = NULL;
    
    sqlite3_open(":memory:", &db);
    
    // Create a test table
    sqlite3_exec(db, "CREATE TABLE test (id INTEGER, name TEXT)", NULL, NULL, NULL);
    
    // Test prepare_v3 with PERSISTENT flag
    const char *sql = "INSERT INTO test VALUES (?, ?)";
    int rc = sqlite3_prepare_v3(db, sql, -1, SQLITE_PREPARE_PERSISTENT, &stmt, &tail);
    
    printf("prepare_v3 with PERSISTENT flag: %d\n", rc);
    if (rc == SQLITE_OK) {
        printf("Statement prepared successfully\n");
        sqlite3_finalize(stmt);
    }
    
    // Test prepare_v2 with same SQL  
    rc = sqlite3_prepare_v2(db, sql, -1, &stmt, &tail);
    printf("prepare_v2: %d\n", rc);
    if (rc == SQLITE_OK) {
        printf("Statement prepared successfully\n");
        sqlite3_finalize(stmt);
    }
    
    sqlite3_close(db);
    return 0;
}
