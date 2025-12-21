#!/bin/bash

# Script to analyze PostgreSQL fallback queries and suggest improvements

FALLBACK_LOG="/tmp/plex_pg_fallbacks.log"

if [ ! -f "$FALLBACK_LOG" ]; then
    echo "No fallback log found at $FALLBACK_LOG"
    echo "Run Plex with the shim first to generate fallback data"
    exit 1
fi

echo "=== PostgreSQL Fallback Analysis ==="
echo ""

# Count total fallbacks
TOTAL=$(grep -c "^\[" "$FALLBACK_LOG")
echo "Total fallbacks: $TOTAL"
echo ""

# Count by error type
echo "=== Fallbacks by Error Type ==="
grep "ERROR:" "$FALLBACK_LOG" | sed 's/ERROR: //' | sed 's/LINE.*//' | sort | uniq -c | sort -rn
echo ""

# Count by context
echo "=== Fallbacks by Context ==="
grep "^\[" "$FALLBACK_LOG" | sed 's/\[.*\] //' | sort | uniq -c | sort -rn
echo ""

# Show most common failing queries (first 50 chars)
echo "=== Most Common Failing Queries (top 10) ==="
grep "^ORIGINAL:" "$FALLBACK_LOG" | sed 's/ORIGINAL: //' | cut -c1-100 | sort | uniq -c | sort -rn | head -10
echo ""

# Show unique error patterns
echo "=== Unique Error Patterns ==="
grep "ERROR:" "$FALLBACK_LOG" | sed 's/ERROR: ERROR:  //' | sed 's/LINE.*//' | sort -u
echo ""

# Suggestions
echo "=== Improvement Suggestions ==="
echo ""

# Check for specific patterns
if grep -q "operator does not exist: integer = json" "$FALLBACK_LOG"; then
    echo "❌ JSON operator issue detected"
    echo "   → Fix: Add type casting in SQL translator for JSON comparisons"
    echo ""
fi

if grep -q "must appear in the GROUP BY clause" "$FALLBACK_LOG"; then
    echo "❌ GROUP BY strict mode issue detected"
    echo "   → Fix: Add all selected columns to GROUP BY clause in translator"
    echo ""
fi

if grep -q "syntax error" "$FALLBACK_LOG"; then
    echo "❌ Syntax errors detected"
    echo "   → Review translated SQL for syntax issues"
    echo "   → Check FALLBACK log for specific queries"
    echo ""
fi

if grep -q "no unique or exclusion constraint" "$FALLBACK_LOG"; then
    echo "❌ ON CONFLICT constraint issue detected"
    echo "   → Fix: Check UPSERT conversion logic"
    echo ""
fi

echo ""
echo "=== Top 5 Queries to Fix ==="
echo "(Queries that fail most frequently)"
echo ""

# Show top 5 most frequent failing queries with their errors
grep -B1 "^ORIGINAL:" "$FALLBACK_LOG" | grep -A1 "^\[" | grep -v "^--$" | \
    paste - - | sed 's/\[.*\] //' | cut -c1-150 | sort | uniq -c | sort -rn | head -5 | \
    awk '{$1="["$1"x]"; print}'

echo ""
echo "Full log available at: $FALLBACK_LOG"
