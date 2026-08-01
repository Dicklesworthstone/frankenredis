"""Hammer one O(N) BITCOUNT key from N connections until killed."""
import socket, sys, threading

port = int(sys.argv[1])
nconn = int(sys.argv[2])
key = sys.argv[3].encode()


def enc(*a):
    out = b"*%d\r\n" % len(a)
    for x in a:
        b = x if isinstance(x, bytes) else str(x).encode()
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    return out


def worker():
    cmd = enc(b"BITCOUNT", key)
    while True:
        try:
            s = socket.create_connection(("127.0.0.1", port))
            s.settimeout(60)
            while True:
                s.sendall(cmd)
                if not s.recv(4096):
                    break
        except Exception:
            return


for _ in range(nconn):
    threading.Thread(target=worker, daemon=True).start()
threading.Event().wait()
