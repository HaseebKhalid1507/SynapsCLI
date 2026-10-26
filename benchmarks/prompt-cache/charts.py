#!/usr/bin/env python3
"""Charts for the prompt-cache report (README.md). Reads stats.json in this directory.

    python3 charts.py    # needs matplotlib; writes charts/*.png
"""
import json, os, datetime as dt
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import matplotlib.dates as mdates

W = os.path.dirname(os.path.abspath(__file__))
OUT = f"{W}/charts"
os.makedirs(OUT, exist_ok=True)
s = json.load(open(f"{W}/stats.json"))

BG, FG, GRID = "#0f1117", "#e6e6e6", "#2a2e3a"
C_MAIN, C_WORK, C_UNAT, C_BAD, C_GOOD, C_ACC = "#7aa2f7", "#bb9af7", "#565f89", "#f7768e", "#9ece6a", "#e0af68"
plt.rcParams.update({"figure.facecolor": BG, "axes.facecolor": BG, "axes.edgecolor": GRID, "axes.labelcolor": FG,
                     "xtick.color": FG, "ytick.color": FG, "text.color": FG, "grid.color": GRID,
                     "font.size": 11, "axes.titleweight": "bold", "axes.titlesize": 14, "legend.frameon": False})
D = lambda x: dt.datetime.fromisoformat(x)
TAGS = [("0.8.0", "2026-08-04"), ("0.9.0", "2026-08-20"), ("0.9.1", "2026-09-07"), ("0.10.0", "2026-09-27"),
        ("0.10.2", "2026-10-12")]
BUG = (D("2026-05-15"), D("2026-08-04"))


def mark(ax, top, label_y=None):
    ax.axvspan(*BUG, color=C_BAD, alpha=0.10, lw=0)
    for n, d in TAGS:
        ax.axvline(D(d), color=C_ACC if n == "0.8.0" else GRID, lw=1.6 if n == "0.8.0" else 1, ls="-" if n == "0.8.0" else "--")
        ax.text(D(d), label_y if label_y is not None else top, f" v{n}", color=C_ACC if n == "0.8.0" else "#9aa5ce",
                fontsize=9, va="top", rotation=90)


def fmt_x(ax):
    ax.xaxis.set_major_locator(mdates.MonthLocator())
    ax.xaxis.set_major_formatter(mdates.DateFormatter("%b"))
    ax.grid(True, axis="y", lw=0.6)


# 1. weekly hit rate --------------------------------------------------------------------------
wk = s["by_week"]
x = [D(w) + dt.timedelta(days=3) for w in wk]
fig, ax = plt.subplots(figsize=(12, 5.2))
ax.fill_between(x, [v["p10_hit"] for v in wk.values()], [v["median_hit"] for v in wk.values()],
                color=C_MAIN, alpha=0.15, lw=0, label="p10–median per request")
ax.plot(x, [v["hit_rate"] for v in wk.values()], color=C_MAIN, lw=2.6, marker="o", ms=4, label="weekly hit rate (token-weighted)")
ax.set_ylim(0, 102); mark(ax, 101, label_y=38)
ax.text(D("2026-06-24"), 40, "system-prompt injection bug\n(per-turn context re-wrote the\nwhole conversation every turn)",
        color=C_BAD, ha="center", fontsize=10)
ax.set_title("Synaps prompt-cache hit rate, weekly")
ax.set_ylabel("% of prompt tokens served from cache")
fmt_x(ax); ax.legend(loc="lower right")
fig.tight_layout(); fig.savefig(f"{OUT}/1-weekly-hit-rate.png", dpi=160); plt.close(fig)

# 2. cumulative cache reads -------------------------------------------------------------------
days = sorted(s["daily"])
cum = {"main": [], "worker": [], "unattributed": []}
run = {k: 0 for k in cum}
for d in days:
    for k in cum:
        run[k] += s["daily"][d].get(f"rd_{k}", 0); cum[k].append(run[k] / 1e9)
xd = [D(d) for d in days]
fig, ax = plt.subplots(figsize=(12, 5.2))
ax.stackplot(xd, cum["unattributed"], cum["main"], cum["worker"], colors=[C_UNAT, C_MAIN, C_WORK],
             labels=["before per-session telemetry (unsplit)", "main sessions", "subagents"], alpha=0.9)
total = [a + b + c for a, b, c in zip(cum["unattributed"], cum["main"], cum["worker"])]
mark(ax, max(total) * 1.02, label_y=max(total) * 0.99)
i080 = next(i for i, d in enumerate(days) if d >= "2026-08-04")
ax.text(D("2026-05-05"), max(total) * 0.60, f"{total[-1]:.1f}B tokens read from cache", color=FG, fontsize=13, fontweight="bold")
ax.text(D("2026-05-05"), max(total) * 0.54, f"{total[-1] - total[i080]:.1f}B of them since v0.8.0 (in 82 days)", color=C_ACC, fontsize=11)
ax.set_title("Cumulative prompt-cache reads (Anthropic, both machines)")
ax.set_ylabel("billion tokens"); fmt_x(ax); ax.legend(loc="upper left")
fig.tight_layout(); fig.savefig(f"{OUT}/2-cumulative-cache-reads.png", dpi=160); plt.close(fig)

# 3. write share + big writes per 1k ----------------------------------------------------------
fig, ax = plt.subplots(figsize=(12, 4.6))
ax.bar(x, [v["write_share"] for v in wk.values()], width=5.5, color=[C_BAD if BUG[0] - dt.timedelta(days=4) <= xx <= BUG[1] else C_GOOD for xx in x], alpha=0.85)
ax.set_ylabel("% of prompt tokens re-written"); mark(ax, 34, label_y=20)
ax2 = ax.twinx()
ax2.plot(x, [v["big_writes_per_1k"] for v in wk.values()], color=C_ACC, lw=2, marker=".", label="cache rebuilds ≥20k tokens per 1,000 requests")
ax2.set_ylabel("rebuilds per 1k requests", color=C_ACC); ax2.tick_params(axis="y", colors=C_ACC)
ax.set_title("Cache writes: what each week paid to (re)build the cache")
fmt_x(ax); ax2.legend(loc="upper right")
fig.tight_layout(); fig.savefig(f"{OUT}/3-cache-writes.png", dpi=160); plt.close(fig)

# 4. monthly API-equivalent cost --------------------------------------------------------------
mo = s["by_month"]
labels = [dt.date.fromisoformat(m + "-01").strftime("%b") + {"2026-04": "\n(from 12th)", "2026-10": "\n(to 25th)"}.get(m, "") for m in mo]
act = [v["cost_total"] / 1e3 for v in mo.values()]
nc = [v["cost_total_nocache"] / 1e3 for v in mo.values()]
fig, ax = plt.subplots(figsize=(12, 4.8))
xi = range(len(labels))
ax.bar([i - 0.2 for i in xi], nc, width=0.4, color=C_UNAT, label="same traffic with no prompt cache")
ax.bar([i + 0.2 for i in xi], act, width=0.4, color=C_MAIN, label="with Synaps' prompt cache")
for i, (a, n) in enumerate(zip(act, nc)):
    ax.text(i + 0.2, a, f"-{(1 - a / n) * 100:.0f}%", ha="center", va="bottom", fontsize=10, color=C_GOOD)
ax.set_xticks(list(xi), labels); ax.set_ylabel("API list-price equivalent, $k")
ax.set_title("What the cache saved, by month (list-price equivalent, not money spent)")
ax.grid(True, axis="y", lw=0.6); ax.legend(loc="upper left")
fig.tight_layout(); fig.savefig(f"{OUT}/4-monthly-savings.png", dpi=160); plt.close(fig)
print("wrote", sorted(os.listdir(OUT)))
