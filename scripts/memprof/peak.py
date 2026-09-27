import subprocess, resource, sys, time
t=time.time(); p=subprocess.run(sys.argv[1:],capture_output=True,text=True); dt=time.time()-t
ru=resource.getrusage(resource.RUSAGE_CHILDREN)
print(f"cmd={sys.argv[1:]} out={p.stdout.strip()!r} rc={p.returncode} wall={dt*1000:.0f}ms maxrss={ru.ru_maxrss}kB ({ru.ru_maxrss/1024:.1f}MB)")
