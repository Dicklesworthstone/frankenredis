#!/usr/bin/env bash
# redis-cli compatibility smoke test for FrankenRedis
# Starts a FrankenRedis server, runs redis-cli commands, verifies outputs.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
REDIS_CLI="$PROJECT_ROOT/legacy_redis_code/redis/src/redis-cli"
FRANKENREDIS="$PROJECT_ROOT/target/release/frankenredis"

# Fall back to debug binary if release doesn't exist
if [ ! -x "$FRANKENREDIS" ]; then
    FRANKENREDIS="$PROJECT_ROOT/target/debug/frankenredis"
fi

if [ ! -x "$FRANKENREDIS" ]; then
    echo "SKIP: frankenredis binary not found (run cargo build first)"
    exit 0
fi

if [ ! -x "$REDIS_CLI" ]; then
    echo "SKIP: redis-cli not found at $REDIS_CLI"
    exit 0
fi

# Pick a random port
PORT=$((20000 + RANDOM % 10000))
PASS=0
FAIL=0
TOTAL=0

cleanup() {
    if [ -n "${SERVER_PID:-}" ]; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# Start FrankenRedis
"$FRANKENREDIS" --bind 127.0.0.1 --port "$PORT" --mode strict &
SERVER_PID=$!

# Wait for server to be ready
for i in $(seq 1 50); do
    if "$REDIS_CLI" -p "$PORT" PING >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

check() {
    local desc="$1"
    local expected="$2"
    shift 2
    TOTAL=$((TOTAL + 1))
    local actual
    actual=$("$REDIS_CLI" -p "$PORT" "$@" 2>&1) || true
    if [ "$actual" = "$expected" ]; then
        PASS=$((PASS + 1))
        echo "  PASS: $desc"
    else
        FAIL=$((FAIL + 1))
        echo "  FAIL: $desc"
        echo "    expected: $expected"
        echo "    actual:   $actual"
    fi
}

check_contains() {
    local desc="$1"
    local needle="$2"
    shift 2
    TOTAL=$((TOTAL + 1))
    local actual
    actual=$("$REDIS_CLI" -p "$PORT" "$@" 2>&1) || true
    if echo "$actual" | grep -q "$needle"; then
        PASS=$((PASS + 1))
        echo "  PASS: $desc"
    else
        FAIL=$((FAIL + 1))
        echo "  FAIL: $desc (does not contain '$needle')"
        echo "    actual: $actual"
    fi
}

echo "redis-cli smoke test against FrankenRedis on port $PORT"
echo "---"

# Basic connectivity
check "PING" "PONG" PING

# String operations
check "SET foo bar" "OK" SET foo bar
check "GET foo" "bar" GET foo
check "APPEND foo baz" "6" APPEND foo baz
check "GET foo after APPEND" "barbaz" GET foo

# Key operations
check "EXISTS foo" "1" EXISTS foo
check "DEL foo" "1" DEL foo
check "EXISTS foo after DEL" "0" EXISTS foo

# Numeric operations
check "SET counter 0" "OK" SET counter 0
check "INCR counter" "1" INCR counter
check "INCRBY counter 9" "10" INCRBY counter 9

# List operations
check "RPUSH mylist a" "1" RPUSH mylist a
check "RPUSH mylist b" "2" RPUSH mylist b
check "LLEN mylist" "2" LLEN mylist
check "LPOP mylist" "a" LPOP mylist

# Hash operations
check "HSET myhash field val" "1" HSET myhash field val
check "HGET myhash field" "val" HGET myhash field

# Set operations
check "SADD myset member1" "1" SADD myset member1
check "SISMEMBER myset member1" "1" SISMEMBER myset member1
check "SCARD myset" "1" SCARD myset

# Server info
check_contains "INFO server has redis_version" "redis_version" INFO server

# DBSIZE
check "DBSIZE" "4" DBSIZE

# Lua EVAL
check "EVAL return 42" "42" EVAL "return 42" 0
check "EVAL redis.call" "10" EVAL "return redis.call('GET','counter')" 0

# CONFIG
check_contains "CONFIG GET maxmemory" "maxmemory" CONFIG GET maxmemory

echo "---"
echo "Results: $PASS/$TOTAL passed, $FAIL failed"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
