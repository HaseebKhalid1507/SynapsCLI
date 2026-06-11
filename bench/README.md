# Cache Benchmark Suite

21-turn tool-heavy conversation benchmark for measuring Anthropic prompt caching effectiveness.

## What it measures
- Per-turn: input tokens, output tokens, cache read, cache write, hit %, cost
- Session totals: aggregate cost, average hit rate, cache efficiency
- Latency: TTFT per turn (when streaming)

## How it works
Each run simulates a realistic coding session:
1. Scaffolds a project in a temp dir
2. Fires 21 sequential prompts that create, read, edit, grep, and refactor files
3. Every answer is verifiable — the benchmark checks file state after each turn
4. Outputs a JSONL log + summary table

## Usage
```bash
# Set auth
export ANTHROPIC_API_KEY=sk-ant-...

# Run with default caching (current strategy)
python3 bench/run.py

# Run with a specific strategy
python3 bench/run.py --strategy single-last    # 1 marker on last message
python3 bench/run.py --strategy sliding-4       # current: 2 markers, stride 4
python3 bench/run.py --strategy last-3          # OpenCode style: last 3 messages

# Compare runs
python3 bench/compare.py results/run-*.jsonl
```
