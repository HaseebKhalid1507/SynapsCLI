"""A daemon client that leaves its parent's session and process tree first.

The process forks; the child calls setsid and forks again; the grandchild
waits until it has been reparented (its parent exits), then opens one
connection per frame given on the command line: it sends the Hello line,
reads one line, sends the frame, reads one more line. What came back, and
the grandchild's session, parent and NoNewPrivs flag, are written as JSON
to the out file. The first process returns once the middle one has exited.

Used by tests/daemon_sandboxed_peer.rs.

argv: [--no-new-privs] socket path, out file, Hello line, frame lines...
With --no-new-privs the first process sets that flag on itself first (as
every process in a Synaps sandbox has it), and nothing else.
"""
import ctypes
import json
import os
import socket
import sys
import time


def no_new_privs():
    with open("/proc/self/status") as f:
        for line in f:
            if line.startswith("NoNewPrivs:"):
                return line.split()[1]
    return None


def talk(path, hello, frame):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(10)
    s.connect(path)
    f = s.makefile("rwb")
    f.write(hello.encode() + b"\n")
    f.flush()
    first = f.readline().decode()
    try:
        f.write(frame.encode() + b"\n")
        f.flush()
        after = f.readline().decode()
    except OSError:
        after = ""
    s.close()
    return {"first": first, "after": after}


PR_SET_NO_NEW_PRIVS = 38


def main():
    args = sys.argv[1:]
    if args and args[0] == "--no-new-privs":
        args = args[1:]
        libc = ctypes.CDLL(None, use_errno=True)
        zero, one = ctypes.c_ulong(0), ctypes.c_ulong(1)
        if libc.prctl(PR_SET_NO_NEW_PRIVS, one, zero, zero, zero) != 0:
            sys.exit("prctl failed: errno %d" % ctypes.get_errno())
    path, out, hello, frames = args[0], args[1], args[2], args[3:]
    top = os.getpid()
    top_sid = os.getsid(0)
    if os.fork() != 0:
        os.wait()
        return
    os.setsid()
    middle = os.getpid()
    if os.fork() != 0:
        os._exit(0)
    deadline = time.time() + 10
    while os.getppid() == middle and time.time() < deadline:
        time.sleep(0.01)
    record = {
        "top": top,
        "top_sid": top_sid,
        "middle": middle,
        "pid": os.getpid(),
        "ppid": os.getppid(),
        "sid": os.getsid(0),
        "no_new_privs": no_new_privs(),
        "replies": [talk(path, hello, frame) for frame in frames],
    }
    tmp = out + ".tmp"
    with open(tmp, "w") as f:
        json.dump(record, f)
    os.rename(tmp, out)
    os._exit(0)


main()
