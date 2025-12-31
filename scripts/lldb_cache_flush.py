#!/usr/bin/env python3
"""
LLDB script to find and flush Plex's SQLite cache.
This script attaches to the running Plex process and attempts to call
sqlite3_db_cacheflush on all open database handles.
"""

import lldb
import sys

def find_plex_pid():
    """Find the PID of the running Plex Media Server"""
    import subprocess
    result = subprocess.run(['pgrep', '-x', 'Plex Media Server'],
                          capture_output=True, text=True)
    if result.returncode != 0:
        print("Error: Plex Media Server is not running")
        return None
    return int(result.stdout.strip())

def main():
    # Initialize debugger
    debugger = lldb.SBDebugger.Create()
    debugger.SetAsync(False)

    # Find Plex PID
    pid = find_plex_pid()
    if not pid:
        return 1

    print(f"Found Plex Media Server with PID: {pid}")

    # Attach to process
    target = debugger.CreateTarget("")
    error = lldb.SBError()
    process = target.AttachToProcessWithID(debugger.GetListener(), pid, error)

    if not error.Success():
        print(f"Error attaching to process: {error}")
        return 1

    print(f"Attached to Plex Media Server (PID {pid})")

    # Get the main thread
    thread = process.GetThreadAtIndex(0)
    frame = thread.GetFrameAtIndex(0)

    # Find sqlite3_db_cacheflush function
    print("\nSearching for sqlite3_db_cacheflush...")

    # Try to find the function in libsqlite3
    for module in target.module_iter():
        if 'libsqlite3' in module.file.GetFilename():
            print(f"Found SQLite library: {module.file.GetFilename()}")

            # Look for sqlite3_db_cacheflush symbol
            symbols = module.FindSymbols('sqlite3_db_cacheflush')
            if symbols.GetSize() > 0:
                symbol = symbols.GetContextAtIndex(0).GetSymbol()
                addr = symbol.GetStartAddress().GetLoadAddress(target)
                print(f"Found sqlite3_db_cacheflush at address: 0x{addr:x}")

    # The challenge: we need to find open sqlite3* database handles
    # These would typically be stored in Plex's global state or singletons
    print("\nNote: To actually flush caches, we would need to:")
    print("1. Find open sqlite3* database handles (stored in Plex's memory)")
    print("2. Call sqlite3_db_cacheflush(db) for each handle")
    print("3. This requires reverse engineering Plex's internal data structures")

    # Detach
    process.Detach()
    lldb.SBDebugger.Destroy(debugger)

    return 0

if __name__ == '__main__':
    sys.exit(main())
