import sys,re,collections
pid=sys.argv[1]
groups=collections.defaultdict(lambda:[0,0,0,0])  # rss,pss,private,count
cur=None
for line in open(f"/proc/{pid}/smaps"):
    m=re.match(r'^([0-9a-f]+)-([0-9a-f]+) (\S+) \S+ \S+ \S+\s*(.*)$',line)
    if m:
        name=m.group(4).strip(); perms=m.group(3)
        if name=='' : key='[anon]'
        elif name.startswith('[heap]'): key='[heap]'
        elif name.startswith('[stack'): key='[stack]'
        elif name.startswith('['): key=name
        elif 'synaps' in name: key=f'binary:synaps ({perms})'
        elif '.so' in name: key='shared libs (.so)'
        else: key='file:'+name
        cur=key; groups[key][3]+=1
    elif cur:
        k,v=line.split(':',1)[0],line.split()[1] if len(line.split())>1 else 0
        if k=='Rss': groups[cur][0]+=int(v)
        elif k=='Pss': groups[cur][1]+=int(v)
        elif k in('Private_Clean','Private_Dirty'): groups[cur][2]+=int(v)
tot=[sum(g[i] for g in groups.values()) for i in range(3)]
print(f"{'mapping':40s} {'maps':>5s} {'RSS_kB':>8s} {'PSS_kB':>8s} {'USS_kB':>8s}")
for k,g in sorted(groups.items(),key=lambda x:-x[1][1]):
    if g[0]==0: continue
    print(f"{k[:40]:40s} {g[3]:5d} {g[0]:8d} {g[1]:8d} {g[2]:8d}")
print(f"{'TOTAL':40s} {'':5s} {tot[0]:8d} {tot[1]:8d} {tot[2]:8d}")
