/*
 * Test parameter mapping for INSERT INTO metadata_items
 *
 * This test helps debug why title and guid end up NULL
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// Simulate the placeholder translation
int main() {
    // The Plex INSERT statement uses named params like :U1, :U2, etc.
    // According to the log, we have 49 parameters named U1-U49

    // Looking at metadata_items schema:
    // Position 0 (param U1): library_section_id
    // Position 1 (param U2): parent_id
    // Position 2 (param U3): metadata_type
    // Position 3 (param U4): guid              <-- THIS SHOULD BE BOUND
    // Position 4 (param U5): edition_title
    // Position 5 (param U6): slug
    // Position 6 (param U7): hash
    // Position 7 (param U8): media_item_count
    // Position 8 (param U9): title             <-- THIS SHOULD BE BOUND
    // Position 9 (param U10): title_sort
    // ... etc

    printf("Expected parameter mapping for INSERT INTO metadata_items:\n");
    printf("  U1  -> library_section_id (position 0)\n");
    printf("  U2  -> parent_id (position 1)\n");
    printf("  U3  -> metadata_type (position 2)\n");
    printf("  U4  -> guid (position 3)  <<< CRITICAL\n");
    printf("  U5  -> edition_title (position 4)\n");
    printf("  U6  -> slug (position 5)\n");
    printf("  U7  -> hash (position 6)\n");
    printf("  U8  -> media_item_count (position 7)\n");
    printf("  U9  -> title (position 8)  <<< CRITICAL\n");
    printf("  U10 -> title_sort (position 9)\n");
    printf("\n");

    printf("If parameter mapping is correct:\n");
    printf("  When Plex binds :U4, it should map to pg_idx=3 (guid column)\n");
    printf("  When Plex binds :U9, it should map to pg_idx=8 (title column)\n");
    printf("\n");

    printf("The BUG is likely in pg_map_param_index():\n");
    printf("  - It receives sqlite_idx (1-based index from SQLite)\n");
    printf("  - It needs to map named parameter :U4 to pg_idx=3 (0-based)\n");
    printf("  - The param_names array should have: param_names[3] = \"U4\"\n");
    printf("  - If param_names[3] != \"U4\", the mapping is wrong!\n");
    printf("\n");

    printf("To debug:\n");
    printf("  1. Check if param_names array is correctly populated during PREPARE\n");
    printf("  2. Check if sqlite3_bind_parameter_name() returns :U4 for the guid bind\n");
    printf("  3. Check if pg_map_param_index() finds \"U4\" in param_names[]\n");
    printf("  4. Verify that the correct pg_idx is returned\n");

    return 0;
}
