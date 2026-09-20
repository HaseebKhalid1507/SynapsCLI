#!/usr/bin/env python3
# symbol-audit.py — after each phase of a rebuild-not-merge: every fn/const/type the
# upstream branch defines that our copy of the same file lacks. Test-vs-prod is decided
# by the caller (see PROCEDURE.md). Dropped symbols found 3 lost hunks in #112.
# usage: UP=<branch> python3 scripts/merge/symbol-audit.py
import subprocess, re, sys, collections
import os
UP=os.environ.get("UP","origin/feat/context-continuation")
def files(ref):
    return set(subprocess.run(["git","ls-tree","-r","--name-only",ref],capture_output=True,text=True).stdout.split())
def show(ref,f):
    r=subprocess.run(["git","show",f"{ref}:{f}"],capture_output=True,text=True); return r.stdout if r.returncode==0 else None
FN=re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)',re.M)
CONST=re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const|static)\s+([A-Z_][A-Z0-9_]*)',re.M)
TYPE=re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum|trait|type)\s+([A-Za-z_][A-Za-z0-9_]*)',re.M)
def syms(src):
    return set("fn:"+x for x in FN.findall(src))|set("const:"+x for x in CONST.findall(src))|set("type:"+x for x in TYPE.findall(src))
up=files(UP); ours=files("HEAD")
roots=("crates/agent-core/src","crates/agent-engine/src","src/")
skip=("crates/agent-tui/",)
missing_files=[]; missing_syms=collections.OrderedDict()
for f in sorted(up):
    if not f.endswith(".rs") or not f.startswith(roots): continue
    if f not in ours: missing_files.append(f); continue
    u=show(UP,f); o=show("HEAD",f)
    d=syms(u)-syms(o)
    if d: missing_syms[f]=sorted(d)
print("== .rs files upstream has that we don't (engine/core/src):"); [print("  ",f) for f in missing_files]
print("\n== symbols (fn/const/type) upstream defines that our copy of the same file lacks:")
tot=0
for f,d in missing_syms.items():
    tot+=len(d); print(f"  {f} ({len(d)})"); [print("     ",x) for x in d]
print("\nTOTAL missing symbols:",tot)
