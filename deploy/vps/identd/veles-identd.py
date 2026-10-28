#!/usr/bin/python3 -I
"""veles-identd — an RFC 1413 ident responder that answers for ONE user.

Why it exists (2026-10-02): the owner's IRC client runs on the VPS, and
so do veles' sandboxes, so both reach IRC from the same address. An IRC
server shows an ident it could not verify as `~name`, which any client
may claim; a sandbox could connect as the owner's nick and `~hastur` and
match the owner's hostmask. With this answering, the owner's connections
carry a VERIFIED ident (`hastur`, no `~`) and everyone else's stay `~`.

What it does, and nothing more: systemd hands it one accepted connection
(veles-identd.socket, Accept=yes) on stdin/stdout. It reads one query
line (`<port on this host> , <port on the querier>`), finds the
ESTABLISHED TCP connection with those ports whose far end is the querier
itself, and replies `USERID : UNIX : <name>` only when that connection's
owner is in VELES_IDENTD_USERS (`unixuser=reply,…`); anything else is
`ERROR : NO-USER`. No config file, no per-user files, no forwarding or
NAT lookups, no spoofing by users: the features other identds were
broken through. Input is one line of at most 64 bytes, read for at most
10 seconds; systemd bounds connections, memory and run time on top.

    veles-identd.py --self-test     # the pure part, on canned /proc rows
"""
import os
import pwd
import re
import signal
import socket
import sys

MAX_LINE = 64
TIMEOUT_SECS = 10
QUERY = re.compile(rb"^\s*(\d{1,5})\s*,\s*(\d{1,5})\s*$")
ESTABLISHED = "01"


def users_from(spec):
    """`wrath=hastur,other=x` -> {"wrath": "hastur", ...}; bad pairs dropped."""
    out = {}
    for pair in (spec or "").split(","):
        user, _, reply = pair.partition("=")
        user, reply = user.strip(), reply.strip()
        if user and re.fullmatch(r"[A-Za-z0-9._-]{1,32}", reply or ""):
            out[user] = reply
    return out


def hex_addr(field):
    """A /proc/net/tcp{,6} address `HEX:PORT` -> (ip text, port)."""
    addr, port = field.split(":")
    raw = bytes.fromhex(addr)
    # Each 32-bit word is in host (little-endian) order.
    words = b"".join(raw[i:i + 4][::-1] for i in range(0, len(raw), 4))
    family = socket.AF_INET if len(words) == 4 else socket.AF_INET6
    ip = socket.inet_ntop(family, words)
    if ip.startswith("::ffff:") and "." in ip:
        ip = ip[len("::ffff:"):]
    return ip, int(port, 16)


def plain_ip(ip):
    """The querier's address without an IPv4-mapped prefix."""
    return ip[len("::ffff:"):] if ip.startswith("::ffff:") and "." in ip else ip


def find_uid(tables, local_port, remote_port, remote_ip):
    """The owner uid of the established connection local:local_port ->
    remote_ip:remote_port, from the text of /proc/net/tcp and tcp6."""
    for text in tables:
        for row in text.splitlines()[1:]:
            f = row.split()
            if len(f) < 8 or f[3] != ESTABLISHED:
                continue
            try:
                (_, lport), (rip, rport) = hex_addr(f[1]), hex_addr(f[2])
            except ValueError:
                continue
            if lport == local_port and rport == remote_port and rip == remote_ip:
                return int(f[7])
    return None


def answer(line, querier_ip, tables, users, name_of):
    """The reply line (no CRLF) to one query line (bytes)."""
    m = QUERY.match(line.rstrip(b"\r\n"))
    if not m:
        return "0 , 0 : ERROR : INVALID-PORT"
    lp, rp = int(m.group(1)), int(m.group(2))
    head = f"{lp} , {rp}"
    if not (0 < lp < 65536 and 0 < rp < 65536):
        return f"{head} : ERROR : INVALID-PORT"
    uid = find_uid(tables, lp, rp, plain_ip(querier_ip))
    reply = users.get(name_of(uid)) if uid is not None else None
    if reply is None:
        return f"{head} : ERROR : NO-USER"
    return f"{head} : USERID : UNIX : {reply}"


def name_of_uid(uid):
    try:
        return pwd.getpwuid(uid).pw_name if uid is not None else None
    except KeyError:
        return None


def read_line(conn):
    buf = b""
    while len(buf) < MAX_LINE and b"\n" not in buf:
        chunk = conn.recv(MAX_LINE - len(buf))
        if not chunk:
            break
        buf += chunk
    return buf.split(b"\n", 1)[0]


def serve():
    signal.alarm(TIMEOUT_SECS)
    conn = socket.socket(fileno=sys.stdin.fileno())
    querier = conn.getpeername()[0]
    line = read_line(conn)
    tables = []
    for name in ("tcp", "tcp6"):
        try:
            with open(f"/proc/net/{name}") as f:
                tables.append(f.read())
        except OSError:
            pass
    reply = answer(line, querier, tables, users_from(os.environ.get("VELES_IDENTD_USERS")),
                   name_of_uid)
    conn.sendall(reply.encode() + b"\r\n")
    conn.close()


def proc_addr(ip, port):
    """The inverse of hex_addr, for the self-test's rows."""
    family = socket.AF_INET6 if ":" in ip else socket.AF_INET
    raw = socket.inet_pton(family, ip)
    words = b"".join(raw[i:i + 4][::-1] for i in range(0, len(raw), 4))
    return f"{words.hex().upper()}:{port:04X}"


def self_test():
    # The kernel's own spelling (little-endian words, as on x86 and
    # arm64): 127.0.0.1:80 and [::1]:80. Decoding these is the claim
    # that matters; proc_addr only builds the rows below.
    assert hex_addr("0100007F:0050") == ("127.0.0.1", 80)
    assert hex_addr("00000000000000000000000001000000:0050") == ("::1", 80)
    assert hex_addr("0000000000000000FFFF00000100007F:0050") == ("127.0.0.1", 80)

    def row(local, remote, state, uid):
        return f"   0: {proc_addr(*local)} {proc_addr(*remote)} {state} " \
               f"00000000:00000000 00:00000000 00000000 {uid:>5}        0 1\n"
    head = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"
    server4, server6 = "167.99.234.58", "2607:5300:201:3000::4757"
    tcp = head + row(("203.0.113.10", 42206), (server4, 6697), "01", 1002) \
        + row(("203.0.113.10", 42207), (server4, 6697), "01", 992) \
        + row(("0.0.0.0", 113), ("0.0.0.0", 0), "0A", 1002)
    tcp6 = head + row(("2001:41d0:404:100::1f41", 59356), (server6, 6697), "01", 1002)
    names = {1002: "wrath", 992: "veles-sbx"}
    users = users_from("wrath=hastur, bad=no spaces allowed,=x")
    assert users == {"wrath": "hastur"}, users

    def ok(line, ip):
        return answer(line, ip, [tcp, tcp6], users, names.get)
    cases = [
        # the owner's connection, asked by its own far end (v4, v4-mapped, v6)
        (b"42206 , 6697\r\n", server4, "42206 , 6697 : USERID : UNIX : hastur"),
        (b"42206,6697", "::ffff:" + server4, "42206 , 6697 : USERID : UNIX : hastur"),
        (b"59356 , 6697\r\n", server6, "59356 , 6697 : USERID : UNIX : hastur"),
        # a sandbox's connection: same server, not an owner
        (b"42207 , 6697\r\n", server4, "42207 , 6697 : ERROR : NO-USER"),
        # a third party asking about the owner's connection
        (b"42206 , 6697\r\n", "203.0.113.9", "42206 , 6697 : ERROR : NO-USER"),
        # a listening socket is not a connection
        (b"113 , 0\r\n", server4, "113 , 0 : ERROR : INVALID-PORT"),
        (b"99999 , 6697", server4, "99999 , 6697 : ERROR : INVALID-PORT"),
        (b"GET / HTTP/1.0", server4, "0 , 0 : ERROR : INVALID-PORT"),
        (b"", server4, "0 , 0 : ERROR : INVALID-PORT"),
    ]
    for line, ip, want in cases:
        got = ok(line, ip)
        assert got == want, (line, ip, got, want)
    print(f"veles-identd self-test: {len(cases) + 3} checks ok")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    else:
        serve()
