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
C_MAIN, C_WORK, C_UNAT, C_GOOD, C_ACC = "#7aa2f7", "#bb9af7", "#565f89", "#9ece6a", "#e0af68"
plt.rcParams.update({"figure.facecolor": BG, "axes.facecolor": BG, "axes.edgecolor": GRID, "axes.labelcolor": FG,
                     "xtick.color": FG, "ytick.color": FG, "text.color": FG, "grid.color": GRID,
                     "font.size": 11, "axes.titleweight": "bold", "axes.titlesize": 14, "legend.frameon": False})
D = lambda x: dt.datetime.fromisoformat(x)
START, END = D("2026-08-04"), D("2026-10-25")
TAGS = [("0.9.0", "2026-08-20"), ("0.9.1", "2026-09-07"), ("0.10.0", "2026-09-27"), ("0.10.1", "2026-09-30"),
        ("0.10.2", "2026-10-12")]


def mark(ax, label_y, va="top"):
    """Release lines. `label_y` is a number, or a function of the tag's date."""
    for n, d in TAGS:
        ax.axvline(D(d), color=GRID, lw=1, ls="--")
        y = label_y(d) if callable(label_y) else label_y
        ax.text(D(d), y, f" v{n}", color="#9aa5ce", fontsize=9, va=va, rotation=90)


def fmt_x(ax):
    ax.set_xlim(START - dt.timedelta(days=3), END + dt.timedelta(days=3))
    ax.xaxis.set_major_locator(mdates.MonthLocator())
    ax.xaxis.set_minor_locator(mdates.WeekdayLocator(byweekday=mdates.MO))
    ax.xaxis.set_major_formatter(mdates.DateFormatter("%b"))
    ax.grid(True, axis="y", lw=0.6)


# 1. weekly hit rate --------------------------------------------------------------------------
wk = s["by_week"]
x = [max(D(w), START) + dt.timedelta(days=3) for w in wk]
fig, ax = plt.subplots(figsize=(12, 5.2))
ax.fill_between(x, [v["p10_hit"] for v in wk.values()], [v["median_hit"] for v in wk.values()],
                color=C_MAIN, alpha=0.18, lw=0, label="p10–median per request")
ax.plot(x, [v["hit_rate"] for v in wk.values()], color=C_MAIN, lw=2.6, marker="o", ms=4,
        label="weekly hit rate (token-weighted)")
ax.set_ylim(80, 100.5); mark(ax, 88.5)
ax.set_title(f"Synaps prompt-cache hit rate, weekly: {s['since_080']['hit_rate']:.1f}% since v0.8.0")
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
             labels=["not split (session telemetry off)", "main sessions", "subagents"], alpha=0.9)
total = [a + b + c for a, b, c in zip(cum["unattributed"], cum["main"], cum["worker"])]
# each release label sits just above the stack on its date
mark(ax, lambda d: total[max(i for i, x in enumerate(days) if x <= d)] + max(total) * 0.05, va="bottom")
ax.text(START + dt.timedelta(days=2), max(total) * 0.55, f"{total[-1]:.1f}B tokens read from cache",
        color=FG, fontsize=13, fontweight="bold")
ax.text(START + dt.timedelta(days=2), max(total) * 0.48, f"since v0.8.0, in {len(days)} active days",
        color=C_ACC, fontsize=11)
ax.set_title("Cumulative prompt-cache reads (Anthropic, both machines)")
ax.set_ylabel("billion tokens"); fmt_x(ax); ax.legend(loc="upper left")
fig.tight_layout(); fig.savefig(f"{OUT}/2-cumulative-cache-reads.png", dpi=160); plt.close(fig)

# 3. write share + big writes per 1k ----------------------------------------------------------
fig, ax = plt.subplots(figsize=(12, 4.6))
ax.bar(x, [v["write_share"] for v in wk.values()], width=5.5, color=C_GOOD, alpha=0.85,
       label="% of prompt tokens re-written")
ax.set_ylim(0, 6); ax.set_ylabel("% of prompt tokens re-written"); mark(ax, 5.0)
ax2 = ax.twinx()
ax2.plot(x, [v["big_writes_per_1k"] for v in wk.values()], color=C_ACC, lw=2, marker=".",
         label="cache rebuilds ≥20k tokens per 1,000 requests")
ax2.set_ylim(0, 30)
ax2.set_ylabel("rebuilds per 1k requests", color=C_ACC); ax2.tick_params(axis="y", colors=C_ACC)
ax.set_title("Cache writes: what each week paid to (re)build the cache")
fmt_x(ax)
h1, l1 = ax.get_legend_handles_labels(); h2, l2 = ax2.get_legend_handles_labels()
ax2.legend(h1 + h2, l1 + l2, loc="upper center", ncol=2)
fig.tight_layout(); fig.savefig(f"{OUT}/3-cache-writes.png", dpi=160); plt.close(fig)

# 4. monthly API-equivalent cost --------------------------------------------------------------
mo = s["by_month"]
labels = [dt.date.fromisoformat(m + "-01").strftime("%b") + {"2026-08": "\n(from 4th)", "2026-10": "\n(to 25th)"}.get(m, "")
          for m in mo]
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
