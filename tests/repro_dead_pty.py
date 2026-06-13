import os, pty, time, signal, subprocess, struct, fcntl, termios, sys
BIN = os.path.expanduser("~/Projects/agent-runtime/target/debug/synaps")
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
proc = subprocess.Popen([BIN], stdin=slave, stdout=slave, stderr=slave,
    preexec_fn=os.setsid, env={**os.environ, "TERM":"xterm-256color"})
os.close(slave)
pid=proc.pid
print(f"[repro] pid={pid}, booting 3s...")
time.sleep(3)

def cpu(pid):
    def j():
        with open(f"/proc/{pid}/stat") as f: p=f.read().split()
        return int(p[13])+int(p[14])
    try: a=j(); time.sleep(0.5); b=j()
    except FileNotFoundError: return None
    return (b-a)/os.sysconf("SC_CLK_TCK")/0.5*100

print(f"[repro] booted: alive={proc.poll() is None}, cpu={cpu(pid)}%")
print("[repro] >>> closing PTY master (terminal window closes) <<<")
os.close(master)

for t in range(8):
    time.sleep(0.5)
    if proc.poll() is not None:
        print(f"[repro] ✅ EXITED {0.5*(t+1):.1f}s after PTY close, rc={proc.returncode}")
        sys.exit(0)
    print(f"[repro] +{0.5*(t+1):.1f}s ALIVE cpu={cpu(pid)}%")

print("[repro] 🔴 survived 4s — sending SIGTERM")
os.kill(pid, signal.SIGTERM)
for t in range(4):
    time.sleep(0.5)
    if proc.poll() is not None:
        print(f"[repro] SIGTERM worked after {0.5*(t+1):.1f}s rc={proc.returncode}"); sys.exit(1)
    print(f"[repro] post-SIGTERM +{0.5*(t+1):.1f}s ALIVE cpu={cpu(pid)}%")
print("[repro] 🔴🔴 SIGTERM IGNORED — only SIGKILL works (matches bug report)")
os.kill(pid, signal.SIGKILL); proc.wait(); sys.exit(2)
