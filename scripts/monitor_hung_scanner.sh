#!/bin/bash
# Monitor for hung Plex Media Scanner processes and capture lldb backtrace

THRESHOLD=3  # seconds in UN state before capturing
LOG_DIR="/tmp/plex_hung_scanners"
mkdir -p "$LOG_DIR"

declare -A first_seen

while true; do
    # Find scanners in UN (uninterruptible sleep) state
    while read -r pid state cmd; do
        [[ -z "$pid" ]] && continue

        now=$(date +%s)

        if [[ "$state" == "UN" || "$state" == "U" || "$state" == "D" ]]; then
            if [[ -z "${first_seen[$pid]}" ]]; then
                first_seen[$pid]=$now
                echo "[$(date)] Scanner $pid entered UN state"
            else
                elapsed=$((now - first_seen[$pid]))
                if [[ $elapsed -ge $THRESHOLD ]]; then
                    timestamp=$(date +%Y%m%d_%H%M%S)
                    outfile="$LOG_DIR/scanner_${pid}_${timestamp}.txt"

                    echo "[$(date)] Scanner $pid hung for ${elapsed}s - capturing lldb backtrace..."

                    {
                        echo "=== Hung Scanner Debug: PID $pid ==="
                        echo "Time: $(date)"
                        echo "Command: $cmd"
                        echo ""
                        echo "=== LLDB Backtrace ==="
                        lldb -p "$pid" -o "bt all" -o "quit" 2>&1
                        echo ""
                        echo "=== /proc-style info ==="
                        ps -p "$pid" -o pid,state,wchan,command 2>/dev/null
                    } > "$outfile" 2>&1

                    echo "[$(date)] Saved to $outfile"

                    # Kill the hung process
                    kill -9 "$pid" 2>/dev/null
                    echo "[$(date)] Killed hung scanner $pid"

                    unset first_seen[$pid]
                fi
            fi
        else
            # Process no longer in UN state
            unset first_seen[$pid] 2>/dev/null
        fi
    done < <(ps aux | grep "Plex Media Scanner" | grep -v grep | awk '{print $2, $8, $0}')

    # Clean up entries for dead processes
    for pid in "${!first_seen[@]}"; do
        if ! kill -0 "$pid" 2>/dev/null; then
            unset first_seen[$pid]
        fi
    done

    sleep 1
done
