#!/usr/bin/env python3
"""Differential gate: WAIT / WAITAOF against Redis 7.2.4.

Besides standalone argument/AOF checks, a scripted replica acknowledges exact
positions in each server's real replication stream. Interleaved TCP clients then
exercise the issuing client's write offset, frozen blocking targets, ACK versus
FACK, transaction/script effects, blocked writes, and CLIENT UNBLOCK continuation.
Redis 7.2.4 server.c updates c->woff after propagation; replication.c uses that
offset for both the initial check and the blocked WAIT/WAITAOF target.

Run against dedicated disposable primaries with no existing replicas. This gate
flushes data, toggles appendonly, and leaves appendonly disabled. Positive WAIT
requests use finite timeouts except when explicitly released by CLIENT UNBLOCK;
WAIT with timeout 0 would otherwise wait indefinitely when its count is unmet.

Usage: wait_waitaof_differ.py <oracle_port> <fr_port>
       Exit 0 = byte-exact and expected semantics, 1 = divergence, 2 = setup error.
"""
from contextlib import ExitStack
import select
import sys
import time

from _respread import assert_ok, cmd, conn, encode_command, frame_len, read_frame


def expect(reply, wanted, label):
    if reply != wanted:
        raise AssertionError(f"{label}: expected {wanted!r}, got {reply!r}")


def info_fields(sock, section):
    raw = cmd(sock, "INFO", section)
    if not raw.startswith(b"$"):
        raise AssertionError(f"INFO {section}: expected bulk reply, got {raw!r}")
    return dict(line.split(b":", 1) for line in raw.split(b"\r\n") if b":" in line)


def read_replies(sock, count, deadline=4):
    """Read every pipeline reply, even when TCP splits or coalesces them."""
    expires = time.monotonic() + deadline
    old_timeout = sock.gettimeout()
    buf = b""
    try:
        for _ in range(count):
            while (size := frame_len(buf)) is None:
                remaining = expires - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("incomplete WAIT pipeline reply")
                sock.settimeout(remaining)
                chunk = sock.recv(1 << 16)
                if not chunk:
                    raise OSError("server closed the WAIT pipeline connection")
                buf += chunk
            yield buf[:size]
            buf = buf[size:]
        if buf:
            raise AssertionError(f"unsolicited pipeline reply: {buf!r}")
    finally:
        sock.settimeout(old_timeout)


class ScriptedReplica:
    """Consume full sync, count real bytes, and deliberately withhold ACK/FACK."""

    def __init__(self, port):
        self.sock = conn(port)
        self.reader = self.sock.makefile("rb")
        try:
            assert_ok(cmd(self.sock, "REPLCONF", "listening-port", "0"), "replica port")
            # No EOF capability: the RDB has a length, avoiding sentinel scans.
            assert_ok(cmd(self.sock, "REPLCONF", "capa", "psync2"), "replica capability")
            self.sock.sendall(encode_command(("PSYNC", "?", "-1")))
            header = self.nonempty_line()
            if not header.startswith(b"+FULLRESYNC "):
                raise AssertionError(f"replica full sync failed: {header!r}")
            self.offset = int(header.split()[2])
            preamble = self.nonempty_line()
            if not preamble.startswith(b"$") or preamble.startswith(b"$EOF:"):
                raise AssertionError(f"expected length-prefixed RDB, got {preamble!r}")
            snapshot = self.exact(int(preamble[1:-2]))
            if not snapshot.startswith(b"REDIS"):
                raise AssertionError("full sync did not contain an RDB snapshot")
            # Unlike a RESP bulk reply, the RDB transfer has no trailing CRLF.
            self.ack(self.offset)
        except Exception:
            self.close()
            raise

    def close(self):
        self.reader.close()
        self.sock.close()

    def line(self):
        line = self.reader.readline(1 << 20)
        if not line.endswith(b"\n"):
            raise AssertionError(f"incomplete replica line: {line[:100]!r}")
        return line

    def nonempty_line(self):
        line = self.line()
        while line in (b"\n", b"\r\n"):
            line = self.line()  # Full-sync keepalives are not stream bytes.
        return line

    def exact(self, size):
        if not 0 <= size <= 64 * 1024 * 1024:
            raise AssertionError(f"invalid replica transfer length: {size}")
        data = self.reader.read(size)
        if len(data) != size:
            raise AssertionError(f"short replica transfer: {len(data)} of {size}")
        return data

    def frame(self):
        header = self.line()
        if not header.startswith(b"*"):
            raise AssertionError(f"invalid replication command: {header!r}")
        argc = int(header[1:-2])
        if not 1 <= argc <= 1024:
            raise AssertionError(f"invalid replication argument count: {argc}")
        size, args = len(header), []
        for _ in range(argc):
            bulk = self.line()
            if not bulk.startswith(b"$"):
                raise AssertionError(f"invalid replication argument: {bulk!r}")
            data = self.exact(int(bulk[1:-2]) + 2)
            if not data.endswith(b"\r\n"):
                raise AssertionError("replication argument missing CRLF")
            size += len(bulk) + len(data)
            args.append(data[:-2])
        self.offset += size
        return args

    def until(self, *command):
        wanted = [str(a).encode("ascii") if not isinstance(a, bytes) else a for a in command]
        expires = time.monotonic() + 10
        for _ in range(256):
            if time.monotonic() >= expires:
                break
            if self.frame() == wanted:
                return self.offset
        raise AssertionError(f"replication stream did not contain {wanted!r}")

    def ack(self, offset, fsync=True):
        args = ["REPLCONF", "ACK", offset]
        if fsync:
            args += ["FACK", offset]
        self.sock.sendall(encode_command(args))  # ACK deliberately has no reply.


def client_offset_cases(port):
    """Return real TCP observations and independent expected replies for one primary."""
    results, wanted = {}, {}
    with ExitStack() as stack:
        control, a, b, unwritten = [stack.enter_context(conn(port)) for _ in range(4)]
        assert_ok(cmd(control, "CONFIG", "SET", "appendonly", "no"), "AOF off")
        assert_ok(cmd(control, "FLUSHALL"), "offset fixture FLUSHALL")
        info = info_fields(control, "replication")
        if info.get(b"role") != b"master" or info.get(b"connected_slaves") != b"0":
            raise AssertionError("offset cases require a primary with no existing replicas")
        replica = ScriptedReplica(port)
        stack.callback(replica.close)

        def record(label, sock, command, expected):
            results[label] = cmd(sock, *command)
            wanted[label] = expected

        def wait_pair(label, sock, count, replicas=1):
            record(label + "_wait", sock, ("WAIT", replicas, 100), b":%d\r\n" % count)
            record(label + "_waitaof", sock, ("WAITAOF", 0, replicas, 100),
                   b"*2\r\n:0\r\n:%d\r\n" % count)

        def write(sock, key, value):
            assert_ok(cmd(sock, "SET", key, value), f"SET {key}")
            return replica.until("SET", key, value)

        def acknowledge(offset):
            replica.ack(offset)
            # Seeing the new ACK in INFO is a causal barrier: both ACK and FACK
            # from that same command have been processed before initial checks.
            expires = time.monotonic() + 4
            while time.monotonic() < expires:
                info = info_fields(control, "replication")
                for key, value in info.items():
                    if key.startswith(b"slave"):
                        fields = dict(part.split(b"=", 1) for part in value.split(b","))
                        if int(fields.get(b"offset", b"-1")) >= offset:
                            return
                time.sleep(0.005)
            raise AssertionError(f"replica ACK {offset} was not observed")

        # A fresh client has offset zero, even after another client's write.
        write(b, "offset:background", "unacknowledged")
        wait_pair("never_written", unwritten, 1)

        offset_a = write(a, "offset:a", "not-an-integer")
        offset_b = write(b, "offset:b", "later")
        if offset_b <= offset_a:
            raise AssertionError("interleaved writer fixture did not advance the stream")
        acknowledge(offset_a)
        wait_pair("a_before_later_b", a, 1)
        wait_pair("b_still_unacknowledged", b, 0)
        wait_pair("timeout_counts_own_target", a, 1, replicas=2)

        expect(cmd(a, "GET", "offset:a"), b"$14\r\nnot-an-integer\r\n", "GET fixture")
        wait_pair("read_retains_history", a, 1)
        expect(cmd(a, "SET", "offset:a", "ignored", "NX"), b"$-1\r\n", "SET NX no-op")
        wait_pair("noop_retains_history", a, 1)
        expect(cmd(a, "INCR", "offset:a"), b"-ERR value is not an integer or out of range\r\n",
               "INCR fixture error")
        wait_pair("error_retains_history", a, 1)
        assert_ok(cmd(a, "MULTI"), "read-only MULTI")
        expect(cmd(a, "GET", "offset:a"), b"+QUEUED\r\n", "queued GET")
        expect(cmd(a, "EXEC"), b"*1\r\n$14\r\nnot-an-integer\r\n", "read-only EXEC")
        wait_pair("readonly_exec_retains_history", a, 1)

        # Receiving bytes and fsyncing bytes are separate acknowledgements.
        ack_only = write(a, "offset:ack-only", "received")
        replica.ack(ack_only, fsync=False)
        record("ack_without_fack_wait", a, ("WAIT", 1, 100), b":1\r\n")
        record("ack_without_fack_waitaof", a, ("WAITAOF", 0, 1, 100), b"*2\r\n:0\r\n:0\r\n")
        replica.ack(ack_only)
        wait_pair("fack_catches_up", a, 1)

        for label, command, expected in (
            ("blocked_wait", ("WAIT", 1, 2000), b":1\r\n"),
            ("blocked_waitaof", ("WAITAOF", 0, 1, 2000), b"*2\r\n:0\r\n:1\r\n"),
        ):
            target = write(a, "offset:" + label, "a")
            write(b, "offset:" + label + ":before", "b")
            a.sendall(encode_command(command))
            # GETACK proves the blocking command was dispatched. A subsequent B
            # write must not move A's frozen target during a polling retry.
            replica.until("REPLCONF", "GETACK", "*")
            if select.select([a], [], [], 0)[0]:
                raise AssertionError(f"{label} replied before its target was acknowledged")
            write(b, "offset:" + label + ":after", "b")
            replica.ack(target)
            results[label] = read_frame(a, deadline=4)
            wanted[label] = expected

        # A write transaction is complete only after the replicated EXEC marker.
        assert_ok(cmd(a, "MULTI"), "write MULTI")
        expect(cmd(a, "SET", "offset:tx", "1"), b"+QUEUED\r\n", "queued SET")
        expect(cmd(a, "INCR", "offset:tx"), b"+QUEUED\r\n", "queued INCR")
        expect(cmd(a, "EXEC"), b"*2\r\n+OK\r\n:2\r\n", "write EXEC")
        tx_offset = replica.until("EXEC")
        acknowledge(tx_offset - 1)
        wait_pair("exec_marker_not_acknowledged", a, 0)
        write(b, "offset:tx:later", "b")
        acknowledge(tx_offset)
        wait_pair("exec_own_final_offset", a, 1)

        script = "redis.call('SET',KEYS[1],'1'); return redis.call('INCR',KEYS[1])"
        expect(cmd(a, "EVAL", script, 1, "offset:lua"), b":2\r\n", "write script")
        script_offset = replica.until("EXEC")
        acknowledge(script_offset - 1)
        wait_pair("script_marker_not_acknowledged", a, 0)
        write(b, "offset:lua:later", "b")
        acknowledge(script_offset)
        wait_pair("script_own_final_offset", a, 1)
        expect(cmd(a, "EVAL", "return redis.call('GET',KEYS[1])", 1, "offset:lua"),
               b"$1\r\n2\r\n", "read-only script")
        wait_pair("readonly_script_retains_history", a, 1)

        # Redis 7.2.4's unblockClientOnKey nests execution around the retried
        # BLPOP. Propagation runs after call() has checked c->woff, so this path
        # retains the previous offset. Pin the live oracle's behavior rather
        # than assuming that a delayed pop updates it like an immediate pop.
        raw_id = cmd(a, "CLIENT", "ID")
        if not raw_id.startswith(b":"):
            raise AssertionError(f"CLIENT ID failed: {raw_id!r}")
        client_id = int(raw_id[1:-2])
        a.sendall(encode_command(("BLPOP", "offset:list", 2)))
        expires = time.monotonic() + 4
        while time.monotonic() < expires:
            clients = cmd(control, "CLIENT", "LIST", "ID", client_id)
            if any(b"b" in part[6:] for part in clients.split() if part.startswith(b"flags=")):
                break
            time.sleep(0.005)
        else:
            raise AssertionError("BLPOP fixture did not block")
        expect(cmd(b, "RPUSH", "offset:list", "value"), b":1\r\n", "blocked pop seed")
        expect(read_frame(a), b"*2\r\n$11\r\noffset:list\r\n$5\r\nvalue\r\n", "BLPOP result")
        pop_offset = replica.until("LPOP", "offset:list")
        wait_pair("delayed_pop_retains_previous_offset", a, 1)
        write(b, "offset:pop:later", "b")
        acknowledge(pop_offset)
        wait_pair("delayed_pop_after_later_writer", a, 1)

        expect(cmd(b, "RPUSH", "offset:immediate", "value"), b":1\r\n", "immediate pop seed")
        expect(cmd(a, "BLPOP", "offset:immediate", 2),
               b"*2\r\n$16\r\noffset:immediate\r\n$5\r\nvalue\r\n", "immediate BLPOP")
        immediate_offset = replica.until("LPOP", "offset:immediate")
        wait_pair("immediate_pop_needs_new_ack", a, 0)
        write(b, "offset:immediate:later", "b")
        acknowledge(immediate_offset)
        wait_pair("immediate_pop_own_offset", a, 1)

        # TIMEOUT must return ACK counts for the frozen target and resume input.
        for label, command, expected in (
            ("unblock_wait", ("WAIT", 2, 0), b":1\r\n"),
            ("unblock_waitaof", ("WAITAOF", 0, 2, 0), b"*2\r\n:0\r\n:1\r\n"),
        ):
            target = write(a, "offset:" + label, "a")
            acknowledge(target)
            write(b, "offset:" + label + ":later", "b")
            a.sendall(encode_command(command) + encode_command(("PING",)))
            replica.until("REPLCONF", "GETACK", "*")
            expect(cmd(control, "CLIENT", "UNBLOCK", client_id, "TIMEOUT"), b":1\r\n", label)
            results[label] = b"".join(read_replies(a, 2))
            wanted[label] = expected + b"+PONG\r\n"
    return results, wanted


def main():
    op = int(sys.argv[1]) if len(sys.argv) > 1 else 16399
    fp = int(sys.argv[2]) if len(sys.argv) > 2 else 16400
    od, fr = conn(op), conn(fp)
    fails = []

    def chk(label, *c):
        ro, rf = cmd(od, *c), cmd(fr, *c)
        if ro != rf:
            fails.append(f"{label}: redis={ro!r} fr={rf!r}")

    for s in (od, fr):
        assert_ok(cmd(s, "CONFIG", "SET", "appendonly", "no"), "CONFIG SET appendonly no")
        assert_ok(cmd(s, "FLUSHALL"), "FLUSHALL")
        assert_ok(cmd(s, "SET", "k", "v"), "SET k v")

    # WAIT — standalone, 0 replicas
    chk("wait_0_0", "WAIT", "0", "0")
    chk("wait_0_100", "WAIT", "0", "100")
    chk("wait_1_50", "WAIT", "1", "50")
    chk("wait_notint_n", "WAIT", "x", "0")
    chk("wait_notint_t", "WAIT", "0", "x")
    chk("wait_neg_timeout", "WAIT", "0", "-1")
    chk("wait_arity", "WAIT", "0")
    # WAITAOF — appendonly OFF
    chk("waitaof_local_off_err", "WAITAOF", "1", "0", "50")
    chk("waitaof_0_0_0", "WAITAOF", "0", "0", "0")
    chk("waitaof_0_1_50", "WAITAOF", "0", "1", "50")
    chk("waitaof_notint", "WAITAOF", "x", "0", "0")
    chk("waitaof_neg_timeout", "WAITAOF", "0", "0", "-1")
    chk("waitaof_arity", "WAITAOF", "0", "0")
    # WAITAOF — appendonly ON
    for s in (od, fr):
        assert_ok(cmd(s, "CONFIG", "SET", "appendonly", "yes"), "CONFIG SET appendonly yes")
    time.sleep(0.3)
    chk("waitaof_local_on", "WAITAOF", "1", "0", "100")
    chk("waitaof_local2", "WAITAOF", "2", "0", "50")
    for s in (od, fr):
        assert_ok(cmd(s, "CONFIG", "SET", "appendonly", "no"), "CONFIG SET appendonly no")
        s.close()

    oracle_offsets, expected = client_offset_cases(op)
    fr_offsets, fr_expected = client_offset_cases(fp)
    expect(fr_expected, expected, "offset scenario definitions")
    for label, reply in expected.items():
        ro, rf = oracle_offsets[label], fr_offsets[label]
        # Independently check the expected result: matching broken fixtures must
        # not turn into a pass merely because both servers returned zero.
        if ro != reply or rf != reply:
            fails.append(f"{label}: expected={reply!r} redis={ro!r} fr={rf!r}")

    print("=" * 60)
    if fails:
        print(f"FAIL — {len(fails)} WAIT/WAITAOF divergence(s) vs redis 7.2.4:")
        for x in fails[:12]:
            print(f"  {x}")
        sys.exit(1)
    print(
        "PASS — WAIT/WAITAOF byte-exact vs redis 7.2.4 "
        f"({len(expected)} client-offset/ACK/FACK observations plus standalone "
        "timeout, arity, and appendonly checks)"
    )


if __name__ == "__main__":
    try:
        main()
    except (AssertionError, OSError, ValueError) as error:
        print(f"ERROR — WAIT/WAITAOF gate could not complete: {error}", file=sys.stderr)
        sys.exit(2)
