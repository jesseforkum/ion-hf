"""Drives `ionhf --loopback` through its host ports for screenshots: an
ARQ link to LOOP with text flowing both ways, or a broadcast group.

python drive.py arq|file|bcast [--port 8400] [--secs 120]
"""
import socket, sys, time, threading

mode = sys.argv[1]
port = int(sys.argv[sys.argv.index("--port") + 1]) if "--port" in sys.argv else 8400
secs = float(sys.argv[sys.argv.index("--secs") + 1]) if "--secs" in sys.argv else 120.0
me = "N0JLF"

def connect(p):
    for _ in range(60):
        try:
            return socket.create_connection(("127.0.0.1", p), timeout=2)
        except OSError:
            time.sleep(0.5)
    raise SystemExit(f"no port {p}")

cmd = connect(port)
cmd.settimeout(None)
def say(line):
    cmd.sendall((line + "\r").encode())

def reader(sock, tag):
    buf = b""
    while True:
        try:
            d = sock.recv(4096)
        except OSError:
            return
        if not d:
            return
        buf += d
        while b"\r" in buf:
            line, buf = buf.split(b"\r", 1)
            print(tag, line.decode(errors="replace"), flush=True)

threading.Thread(target=reader, args=(cmd, "cmd"), daemon=True).start()
say(f"MYCALL {me}")
end = time.time() + secs
if mode == "arq":
    data = connect(port + 1)
    threading.Thread(target=lambda: [data.recv(65536) for _ in iter(int, 1)], daemon=True).start()
    say(f"CONNECT {me} LOOP")
    n = 0
    while time.time() < end:
        text = "".join(f"line {n + i:05d} the quick brown fox jumps over the lazy dog 0123456789\n" for i in range(20))
        data.sendall(text.encode())
        n += 20
        time.sleep(2.0)
elif mode == "file":
    # One big transfer, no line ends (LOOP echoes whole lines only): the
    # stream gear's case.
    data = connect(port + 1)
    threading.Thread(target=lambda: [data.recv(65536) for _ in iter(int, 1)], daemon=True).start()
    say(f"CONNECT {me} LOOP")
    # One line: LOOP sends it back whole, so the stream runs both ways.
    text = "".join(f"block {i:05d} the quick brown fox jumps over the lazy dog 0123456789 " for i in range(150))
    data.sendall((text + chr(10)).encode())
    while time.time() < end:
        time.sleep(1.0)
elif mode == "bcast":
    say(f"BCAST OPEN NET40 FROM {me}")
    time.sleep(1.0)
    kiss = connect(port - 200)
    n = 0
    while time.time() < end:
        frame = f"{me}: bulletin {n:04d} net check-in, all stations QRV on 7.074".encode()
        kiss.sendall(b"\xc0" + bytes([0x10]) + frame.replace(b"\xdb", b"\xdb\xdd").replace(b"\xc0", b"\xdb\xdc") + b"\xc0")
        n += 1
        time.sleep(6.0)
