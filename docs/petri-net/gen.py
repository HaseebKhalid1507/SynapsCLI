"""Regenerate the Petri-net figures for docs/petri-net-model.md.

    python3 docs/petri-net/gen.py        # needs matplotlib

Writes the SVGs next to this file. Text stays text (svg.fonttype = none) and
every figure has a white background, so they read in GitHub's dark theme too.
"""
import math
import pathlib

import matplotlib

matplotlib.use("Agg")
matplotlib.rcParams["svg.fonttype"] = "none"
matplotlib.rcParams["font.family"] = "DejaVu Sans"
matplotlib.rcParams["svg.hashsalt"] = "synaps-petri-net"  # stable ids: reruns are byte-identical
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.lines import Line2D  # noqa: E402
from matplotlib.patches import Circle, FancyArrowPatch, Rectangle  # noqa: E402

OUT = pathlib.Path(__file__).resolve().parent
R = 0.32             # place radius
TW, TH = 0.10, 0.62  # transition bar width / height
ENV = "#e8f0ff"      # environment place fill
RED, BLUE, GREEN, PURPLE = "#b00000", "#0040a0", "#0a6a0a", "#7a4ab0"


class Net:
    def __init__(self, w, h):
        self.fig, self.ax = plt.subplots(figsize=(w, h))
        self.ax.set_aspect("equal")
        self.ax.axis("off")
        self.nodes = {}

    def place(self, name, x, y, tokens=0, label=None, lpos="below", env=False):
        c = Circle((x, y), R, fill=env, fc=ENV if env else "none", ec="k", lw=1.6, zorder=3,
                   ls=(0, (3, 2)) if env else "-")
        self.ax.add_patch(c)
        self.nodes[name] = (c, x, y)
        lab = name if label is None else label
        dy = -R - 0.2 if lpos == "below" else R + 0.2
        self.ax.text(x, y + dy, lab, ha="center", va="center", fontsize=9, fontweight="bold")
        if isinstance(tokens, str):
            self.ax.text(x, y, tokens, ha="center", va="center", fontsize=9)
        elif tokens == 1:
            self.ax.add_patch(Circle((x, y), 0.07, color="k", zorder=4))
        elif tokens > 1:
            self.ax.text(x, y, str(tokens), ha="center", va="center", fontsize=9)

    def trans(self, name, x, y, label=None, note=None, horiz=False, lpos="above", color="k"):
        w, h = (TH, TW) if horiz else (TW, TH)
        r = Rectangle((x - w / 2, y - h / 2), w, h, color=color, zorder=3)
        self.ax.add_patch(r)
        self.nodes[name] = (r, x, y)
        lab = name if label is None else label
        off = h / 2 + 0.17
        s = 1 if lpos == "above" else -1
        if lab:
            self.ax.text(x, y + s * off, lab, ha="center", va="bottom" if s > 0 else "top",
                         fontsize=8.5, style="italic")
        if note:
            self.ax.text(x, y - s * off, note, ha="center", va="top" if s > 0 else "bottom",
                         fontsize=7.5, color=RED)

    def arc(self, a, b, w=None, rad=0.0, woff=(0, 0.13), ls="-"):
        pa, xa, ya = self.nodes[a]
        pb, xb, yb = self.nodes[b]
        self.ax.add_patch(FancyArrowPatch((xa, ya), (xb, yb), patchA=pa, patchB=pb, arrowstyle="-|>",
                                          mutation_scale=11, lw=1.2, ls=ls,
                                          connectionstyle=f"arc3,rad={rad}", zorder=2))
        if w is not None:
            mx, my = (xa + xb) / 2, (ya + yb) / 2
            if rad:
                mx += 0.5 * rad * (yb - ya)
                my -= 0.5 * rad * (xb - xa)
            self.ax.text(mx + woff[0], my + woff[1], str(w), ha="center", va="center", fontsize=8.5,
                         color=BLUE, fontweight="bold",
                         bbox=dict(boxstyle="round,pad=0.08", fc="white", ec="none"))

    def _edge(self, name, toward):
        patch, x, y = self.nodes[name]
        dx, dy = toward[0] - x, toward[1] - y
        if isinstance(patch, Rectangle):
            hw, hh = patch.get_width() / 2, patch.get_height() / 2
            t = min(hw / abs(dx) if dx else 1e9, hh / abs(dy) if dy else 1e9)
            return (x + dx * t, y + dy * t)
        n = math.hypot(dx, dy) or 1
        return (x + dx / n * R, y + dy / n * R)

    def route(self, a, b, pts, ls="-"):
        """Orthogonal polyline arc a -> pts... -> b."""
        verts = [self._edge(a, pts[0])] + list(pts) + [self._edge(b, pts[-1])]
        for (x1, y1), (x2, y2) in zip(verts[:-2], verts[1:-1]):
            self.ax.add_line(Line2D([x1, x2], [y1, y2], color="k", lw=1.2, ls=ls, zorder=2))
        self.ax.add_patch(FancyArrowPatch(verts[-2], verts[-1], arrowstyle="-|>", mutation_scale=11,
                                          lw=1.2, ls=ls, zorder=2))

    def text(self, x, y, s, **kw):
        kw.setdefault("fontsize", 8)
        kw.setdefault("ha", "left")
        kw.setdefault("va", "center")
        self.ax.text(x, y, s, **kw)

    def save(self, name, xlim, ylim):
        self.ax.set_xlim(*xlim)
        self.ax.set_ylim(*ylim)
        self.fig.savefig(OUT / name, bbox_inches="tight", pad_inches=0.06, facecolor="white",
                         metadata={"Date": None})
        plt.close(self.fig)
        # Text stays text: give browsers without DejaVu Sans a similar-width fallback.
        svg = (OUT / name).read_text()
        (OUT / name).write_text(svg.replace("font-family: 'DejaVu Sans'",
                                            "font-family: 'DejaVu Sans', Verdana, sans-serif"))


# ── 1. session lifecycle in the daemon ───────────────────────────────────────
n = Net(7.6, 5.4)
n.place("Live", 0, 0, tokens=1, label="")
n.text(-0.36, -0.42, "Live", fontsize=9, fontweight="bold", ha="right")
n.trans("park", 1.6, 0, note="⟨60 s⟩ [can_park]", lpos="above")
n.place("Parking", 3.2, 0, lpos="below")
n.trans("park_ok", 4.8, 0, note="[saved]", lpos="above")
n.place("Parked", 6.4, 0, lpos="below")
n.trans("park_fail", 3.2, 1.15, horiz=True, label="")
n.text(3.5, 1.15, "park_fail  [save failed]", fontsize=7.5, style="italic", color=RED)
n.trans("unpark", 3.2, 2.45, horiz=True, label="")
n.text(3.2, 2.12, "unpark", ha="center", fontsize=8.5, style="italic")
n.place("Wake", 1.4, 3.5, env=True, label="Wake: attach · send · pin", lpos="above")
n.place("Ended", 3.2, -3.7, lpos="below")
n.trans("idle_end", 1.6, -1.75, label="", note="")
n.text(1.75, -1.75, "idle_end ⟨5 s⟩\n[0 turns, 0 clients]", fontsize=7.5, color=RED)
n.trans("close", 0, -2.3, label="")
n.text(0.15, -2.3, "close\n[nothing in flight\n ∨ force]", fontsize=7.5, color=RED)
n.place("Close", -1.5, -3.7, env=True, label="/close · daemon close", lpos="below")
n.trans("evict", 6.4, -1.75, label="", note="")
n.text(6.55, -1.75, "evict ⟨1 h⟩", fontsize=7.5, color=RED)
n.arc("Live", "park"); n.arc("park", "Parking"); n.arc("Parking", "park_ok"); n.arc("park_ok", "Parked")
n.arc("Parking", "park_fail"); n.route("park_fail", "Live", [(0, 1.15)])
n.route("Parked", "unpark", [(6.4, 2.45)]); n.arc("Wake", "unpark")
n.route("unpark", "Live", [(-0.9, 2.45), (-0.9, 0)])
n.arc("Live", "idle_end"); n.arc("idle_end", "Ended")
n.arc("Live", "close"); n.route("Close", "close", [(-1.5, -2.3)]); n.route("close", "Ended", [(0, -3.7)])
n.arc("Parked", "evict"); n.route("evict", "Ended", [(6.4, -3.7)])
n.save("session.svg", (-2.6, 7.4), (-4.4, 4.0))

# ── 2. input ownership: one token ────────────────────────────────────────────
n = Net(6.4, 4.2)
n.place("Free", 0, 0, tokens=1, label="")
n.text(-0.42, 0, "Free", fontsize=9, fontweight="bold", ha="right")
n.trans("mirror_A", 1.6, 1.3, label="attach_A (Mirror)", lpos="above")
n.trans("mirror_B", 1.6, -1.3, label="attach_B (Mirror)", lpos="below")
n.place("OwnerA", 3.2, 1.3, label="")
n.place("OwnerB", 3.2, -1.3, label="")
n.text(3.45, 1.78, "A owns input", fontsize=9, fontweight="bold")
n.text(3.45, -1.78, "B owns input", fontsize=9, fontweight="bold")
n.trans("take_B", 4.2, 0, label="")
n.trans("take_A", 6.0, 0, label="")
n.text(4.34, 0, "B takes over", fontsize=7.5, style="italic")
n.text(6.14, 0, "A takes over", fontsize=7.5, style="italic")
n.trans("leave_A", 1.6, 2.6, horiz=True, label="")
n.text(1.6, 2.85, "A detaches, no other writer", ha="center", fontsize=7.5, style="italic")
n.trans("leave_B", 1.6, -2.6, horiz=True, label="")
n.text(1.6, -2.85, "B detaches, no other writer", ha="center", fontsize=7.5, style="italic")
n.arc("Free", "mirror_A"); n.arc("mirror_A", "OwnerA"); n.arc("Free", "mirror_B"); n.arc("mirror_B", "OwnerB")
n.arc("OwnerA", "take_B"); n.arc("take_B", "OwnerB"); n.arc("OwnerB", "take_A"); n.arc("take_A", "OwnerA")
n.route("OwnerA", "leave_A", [(3.2, 2.6)]); n.route("leave_A", "Free", [(0, 2.6)])
n.route("OwnerB", "leave_B", [(3.2, -2.6)]); n.route("leave_B", "Free", [(0, -2.6)])
n.text(5.2, 2.3, "Observe clients have\nno arc to these places:\nthey never hold input.", fontsize=7.5, color=BLUE)
n.save("input.svg", (-1.3, 7.8), (-3.2, 3.2))

# ── 3. one turn ──────────────────────────────────────────────────────────────
n = Net(9.6, 5.8)
n.place("Idle", 0, 0, tokens=1, lpos="above")
n.place("Trigger", 0, 1.7, env=True, label="Trigger", lpos="above")
n.trans("start", 1.4, 0, label="start")
n.place("Ready", 2.8, 0, lpos="above")
n.place("Rounds", 4.2, 1.7, tokens="k", label="Rounds (bounded roles)", lpos="above")
n.trans("call_model", 4.2, 0, label="call_model", lpos="below")
n.text(4.32, 0.5, "prio 2", fontsize=7.5, color=RED)
n.place("Streaming", 5.6, 0, lpos="above")
n.trans("reply_text", 7.0, 1.0, label="reply_text\n[no tool_use]", lpos="above")
n.trans("reply_tools", 7.0, -1.0, label="reply_tools\n[has tool_use]", lpos="below")
n.place("Answered", 8.4, 1.0)
n.place("ToolBatch", 8.4, -1.0, lpos="above")
n.trans("finish", 9.8, 1.0, note="prio 1", lpos="below")
n.place("Steer", 8.4, 2.6, env=True, lpos="above")
n.trans("steer", 9.8, 2.6, note="prio 2", lpos="above")
n.trans("run_tools", 9.8, -1.0, label="run_tools (fig. 4)", lpos="below", color=PURPLE)
n.trans("out_of_budget", 1.4, -1.25, horiz=True, label="")
n.text(1.4, -1.55, "out_of_budget  prio 1", ha="center", fontsize=7.5, style="italic", color=RED)
n.place("Esc", 7.2, -2.9, env=True, lpos="below")
n.trans("cancel", 5.6, -2.9, horiz=True, label="")
n.text(5.6, -3.25, "cancel  prio 3", ha="center", fontsize=7.5, style="italic", color=RED)
n.arc("Idle", "start"); n.arc("Trigger", "start"); n.arc("start", "Ready")
n.arc("Ready", "call_model"); n.arc("Rounds", "call_model", ls="--"); n.arc("call_model", "Streaming")
n.arc("Streaming", "reply_text"); n.arc("Streaming", "reply_tools")
n.arc("reply_text", "Answered"); n.arc("reply_tools", "ToolBatch")
n.arc("Answered", "finish"); n.arc("Answered", "steer"); n.arc("Steer", "steer")
n.arc("ToolBatch", "run_tools")
n.arc("Ready", "out_of_budget"); n.arc("out_of_budget", "Idle")
n.arc("Streaming", "cancel"); n.arc("Esc", "cancel")
n.route("steer", "Ready", [(10.6, 2.6), (10.6, 3.55), (2.8, 3.55)])
n.route("run_tools", "Ready", [(10.6, -1.0), (10.6, -2.2), (2.8, -2.2)])
n.route("finish", "Idle", [(11.1, 1.0), (11.1, -3.8), (-0.9, -3.8), (-0.9, 0)])
n.route("cancel", "Idle", [(0, -2.9)])
n.save("turn.svg", (-1.3, 11.5), (-4.1, 4.1))

# ── 4. one tool batch: fork, lanes, join ─────────────────────────────────────
n = Net(8.0, 5.0)
n.place("ToolBatch", 0, 0, tokens=1, lpos="above")
n.trans("dispatch", 1.5, 0, lpos="below")
n.text(1.5, -0.95, "(gate authorizes\nall calls first)", ha="center", fontsize=7, color=RED)
rows = [("read", 3.0, "ReadOnly"), ("grep", 1.0, "ReadOnly"),
        ("bash", -1.0, "NonIdempotent"), ("write", -3.0, "NonIdempotent")]
for name, y, eff in rows:
    top = name in ("read", "grep")
    n.place("P_" + name, 3.2, y, label=f"{name}?", lpos="above" if top else "below")
    n.trans("run_" + name, 4.9, y, label="run_" + name, lpos="below" if name == "write" else "above")
    n.place("R_" + name, 6.6, y, label=f"{name} result", lpos="above" if top else "below")
    n.text(4.9, y + (-0.85 if name == "write" else 0.85), eff, ha="center", fontsize=7,
           color=GREEN if eff == "ReadOnly" else RED)
    n.arc("dispatch", "P_" + name); n.arc("P_" + name, "run_" + name); n.arc("run_" + name, "R_" + name)
n.place("Next", 4.9, -2.0, label="")
n.text(5.3, -2.0, "Next (serial lane)", fontsize=7, color=RED)
n.arc("run_bash", "Next"); n.arc("Next", "run_write")
n.trans("collect", 8.3, 0, lpos="below")
n.text(8.55, 1.0, "join: every tool_use\nneeds its tool_result", fontsize=7, color=RED)
for name, _, _ in rows:
    n.arc("R_" + name, "collect")
n.place("Ready", 9.8, 0, label="Ready (fig. 3)")
n.arc("collect", "Ready")
n.save("batch.svg", (-0.7, 10.7), (-4.1, 4.0))

# ── 5. one call through before_tool_call and the confirm prompt ──────────────
n = Net(8.6, 5.2)
n.place("Call", 0, 0, tokens=1, lpos="below")
n.trans("hook_continue", 1.5, 1.8, label="continue / modify", lpos="above")
n.trans("hook_confirm", 1.5, 0, label="confirm", lpos="below")
n.trans("hook_block", 1.5, -1.8, label="")
n.text(1.35, -2.25, "block", fontsize=8.5, style="italic", ha="right")
n.place("Asking", 3.0, 0, lpos="below")
n.trans("auto_ok", 4.5, 0.9, label="", note="")
n.text(4.38, 1.4, "auto_ok  prio 2\n[auto_approve ∨ allow-all]", fontsize=7.2, color=RED, ha="right")
n.trans("ask", 4.5, -0.9, label="", note="")
n.text(4.5, -0.42, "ask  prio 1", ha="center", fontsize=7.5, color=RED)
n.place("Run", 6.0, 1.8, label="Run", lpos="above")
n.place("Prompt", 6.0, -0.9, label="", lpos="below")
n.text(5.6, -1.5, "Prompt (on clients)", fontsize=7.5, fontweight="bold", ha="right")
n.trans("answer_yes", 7.5, 0.45, label="", note="")
n.text(7.65, -0.05, "y / yes / Allow all", fontsize=7.5, style="italic")
n.place("Yes", 8.7, 0.45, env=True, label="", lpos="below")
n.trans("answer_no", 7.5, -2.1, label="", note="")
n.text(7.65, -1.65, "n / Esc", fontsize=7.5, style="italic")
n.place("No", 7.5, -3.1, env=True, label="", lpos="below")
n.trans("abandon", 6.0, -2.4, label="", note="")
n.text(5.85, -2.45, "abandon ⟨1 h⟩\n[0 clients]", fontsize=7.5, color=RED, ha="right")
n.trans("run_tool", 9.2, 1.8, label="run_tool", lpos="above")
n.place("Result", 10.6, 0, label="")
n.text(10.95, 0.42, "tool_result", fontsize=9, fontweight="bold")
n.arc("Call", "hook_continue"); n.arc("Call", "hook_confirm"); n.arc("Call", "hook_block")
n.arc("hook_confirm", "Asking"); n.arc("hook_continue", "Run")
n.arc("Asking", "auto_ok"); n.arc("Asking", "ask"); n.arc("auto_ok", "Run"); n.arc("ask", "Prompt")
n.arc("Prompt", "answer_yes"); n.arc("Yes", "answer_yes"); n.arc("answer_yes", "Run")
n.arc("Prompt", "abandon"); n.arc("Prompt", "answer_no"); n.arc("No", "answer_no")
n.arc("Run", "run_tool"); n.arc("run_tool", "Result")
n.route("answer_no", "Result", [(10.6, -2.1)])
n.route("abandon", "Result", [(6.0, -3.9), (10.9, -3.9), (10.9, -0.3)])
n.route("hook_block", "Result", [(1.5, -4.4), (11.3, -4.4), (11.3, 0)])
n.text(8.2, -3.65, "denied", fontsize=7.5, color=RED)
n.save("confirm.svg", (-0.6, 12.3), (-4.8, 2.7))

# ── 6. tool-call ledger ──────────────────────────────────────────────────────
n = Net(9.0, 2.9)
xs = {"Planned": 0, "authorize": 1.35, "Authorized": 2.7, "start": 4.05, "Started": 5.4,
      "commit": 6.75, "Committed": 8.1, "record": 9.45, "ResultRecorded": 10.8}
for k, x in xs.items():
    if k[0].isupper():
        n.place(k, x, 0, tokens=1 if k == "Planned" else 0, lpos="above")
    else:
        n.trans(k, x, 0, lpos="below")
keys = list(xs)
for a, b in zip(keys[:-1], keys[1:]):
    n.arc(a, b)
n.place("Esc", 4.05, -1.6, env=True, lpos="below")
n.trans("cancel_early", 2.7, -1.6, horiz=True, label="")
n.trans("cancel_late", 5.4, -1.6, horiz=True, label="")
n.text(2.7, -1.95, "cancel_early", ha="center", fontsize=7.5, style="italic")
n.text(5.4, -1.95, "cancel_late", ha="center", fontsize=7.5, style="italic")
n.place("CanceledBeforeCommit", 1.0, -2.75, label="CanceledBeforeCommit\n(safe to retry)")
n.place("UnknownAfterSideEffect", 7.1, -2.75, label="UnknownAfterSideEffect\n(never auto-rerun)")
n.arc("Authorized", "cancel_early"); n.arc("Esc", "cancel_early"); n.arc("cancel_early", "CanceledBeforeCommit")
n.arc("Started", "cancel_late"); n.arc("Esc", "cancel_late"); n.arc("cancel_late", "UnknownAfterSideEffect")
n.save("ledger.svg", (-0.6, 11.4), (-3.75, 0.9))

# ── 7. the prompt cache entry ────────────────────────────────────────────────
n = Net(5.2, 3.6)
n.place("Ready", -0.8, 0, lpos="below")
n.trans("hit", 2.0, 1.7, horiz=True, label="")
n.text(2.0, 1.95, "hit  prio 2 (read price)", ha="center", fontsize=7.5, color=RED)
n.trans("miss", 2.0, -1.7, horiz=True, label="")
n.text(2.0, -2.0, "miss  prio 1 (write price)", ha="center", fontsize=7.5, color=RED)
n.place("Cache", 2.0, 0, tokens=1, label="", lpos="below")
n.text(2.0, -0.55, "Cache", ha="center", fontsize=9, fontweight="bold")
n.place("Streaming", 4.8, 0, lpos="below")
n.trans("expire", 3.1, 0, label="", note="")
n.text(3.1, 0.48, "expire\n⟨TTL⟩", ha="center", fontsize=7.2, color=RED)
n.trans("prefix_change", 0.9, 0, label="", note="")
n.text(0.9, 0.48, "prefix\nchange", ha="center", fontsize=7.2, color=RED)
n.arc("Ready", "hit"); n.arc("hit", "Streaming"); n.arc("Ready", "miss"); n.arc("miss", "Streaming")
n.arc("Cache", "hit", rad=-0.35); n.arc("hit", "Cache", rad=-0.35); n.arc("miss", "Cache")
n.arc("Cache", "expire"); n.arc("Cache", "prefix_change")
n.save("cache.svg", (-1.4, 5.4), (-2.3, 2.3))

# ── 8. keep-warm pings ───────────────────────────────────────────────────────
n = Net(5.0, 3.6)
n.place("Idle", 0, 0, tokens=1, lpos="below")
n.trans("ping", 2.0, 0, label="", note="")
n.text(2.15, 0.55, "ping ⟨TTL − margin⟩", fontsize=7.5, color=RED)
n.place("Rent", 2.0, 1.7, tokens="k", lpos="above")
n.place("Cache", 2.0, -1.7, tokens=1, lpos="below")
n.trans("expire", 3.8, -1.7, label="", note="")
n.text(3.95, -1.7, "expire ⟨TTL⟩", fontsize=7.5, color=RED)
n.arc("Idle", "ping", rad=0.3); n.arc("ping", "Idle", rad=0.3)
n.arc("Cache", "ping", rad=0.3); n.arc("ping", "Cache", rad=0.3)
n.arc("Rent", "ping"); n.arc("Cache", "expire")
n.save("keepwarm.svg", (-0.6, 5.4), (-2.5, 2.4))
print("wrote", sorted(p.name for p in OUT.glob("*.svg")))
