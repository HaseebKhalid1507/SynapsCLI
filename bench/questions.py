"""
21 tool-heavy benchmark questions with deterministic, verifiable outcomes.

Each question is a dict:
  prompt    — what we send to the model
  verify    — function(sandbox_path) -> bool, checks the expected file state
  expects   — human-readable description of the expected outcome

The questions simulate a realistic coding session: scaffold, inspect, edit,
refactor, grep, fix. Heavy on write/edit/read/bash tool calls so the message
history fills with tool_use/tool_result pairs — exactly what stresses the
cache breakpoint strategy.

Design rules:
- Deterministic: every prompt has exactly one correct end state.
- Tool-forcing: phrased so the model MUST use tools (no from-memory answers).
- Cumulative: later questions depend on earlier file state, so history matters
  and cache-prefix stability is actually exercised.
"""

import json
import os


def _read(p, name):
    try:
        with open(os.path.join(p, name)) as f:
            return f.read()
    except FileNotFoundError:
        return None


def _exists(p, name):
    return os.path.exists(os.path.join(p, name))


QUESTIONS = [
    # ── Phase 1: Scaffold (writes) ──────────────────────────────────
    {
        "id": 1,
        "prompt": (
            "Create a file called calc.py with four functions: "
            "add(a, b) returns a+b, sub(a, b) returns a-b, "
            "mul(a, b) returns a*b, div(a, b) returns a/b but raises "
            "ValueError('division by zero') when b == 0. No other code."
        ),
        "verify": lambda p: (
            (s := _read(p, "calc.py")) is not None
            and all(f"def {f}(" in s for f in ("add", "sub", "mul", "div"))
            and "ValueError" in s
        ),
        "expects": "calc.py with add/sub/mul/div + ValueError guard",
    },
    {
        "id": 2,
        "prompt": (
            "Create config.json containing exactly these keys: "
            '"name" set to "bench", "version" set to "1.0.0", '
            '"debug" set to false, and "max_retries" set to 3.'
        ),
        "verify": lambda p: (
            (s := _read(p, "config.json")) is not None
            and json.loads(s)
            == {"name": "bench", "version": "1.0.0", "debug": False, "max_retries": 3}
        ),
        "expects": "config.json with 4 exact keys",
    },
    {
        "id": 3,
        "prompt": (
            "Create a directory called src, and inside it create three files: "
            "src/models.py with a class User that has __init__(self, name, email) "
            "storing both as attributes; src/db.py with a function connect() that "
            "returns the string 'connected'; and src/__init__.py that is empty."
        ),
        "verify": lambda p: (
            _exists(p, "src/__init__.py")
            and (m := _read(p, "src/models.py")) is not None
            and "class User" in m
            and (d := _read(p, "src/db.py")) is not None
            and "def connect(" in d
            and "connected" in d
        ),
        "expects": "src/ package with models.py, db.py, empty __init__.py",
    },
    {
        "id": 4,
        "prompt": (
            "Create a file named data.csv with a header row 'id,name,score' "
            "and exactly five data rows: 1,alice,90 then 2,bob,85 then "
            "3,carol,77 then 4,dave,92 then 5,eve,68."
        ),
        "verify": lambda p: (
            (s := _read(p, "data.csv")) is not None
            and s.strip().splitlines()[0].strip() == "id,name,score"
            and len(s.strip().splitlines()) == 6
            and "4,dave,92" in s
        ),
        "expects": "data.csv: header + 5 exact rows",
    },
    {
        "id": 5,
        "prompt": (
            "Create a Makefile with two targets: 'test' that runs "
            "'python3 -m pytest' and 'clean' that runs 'rm -rf __pycache__'. "
            "Then run 'ls' and tell me how many files (not directories) "
            "are in the project root."
        ),
        "verify": lambda p: (
            (s := _read(p, "Makefile")) is not None
            and "test:" in s
            and "clean:" in s
            and "pytest" in s
        ),
        "expects": "Makefile with test+clean targets, ls executed",
    },

    # ── Phase 2: Inspect (reads + bash) ─────────────────────────────
    {
        "id": 6,
        "prompt": (
            "Read calc.py and tell me the exact number of 'def ' occurrences "
            "in it. Reply with just the number in your final answer."
        ),
        "verify": lambda p: True,  # answer checked via response text: "4"
        "answer_contains": "4",
        "expects": "reads calc.py, answers 4",
    },
    {
        "id": 7,
        "prompt": (
            "Using grep or any search tool, find every file in this project "
            "that contains the word 'return' and list their paths."
        ),
        "verify": lambda p: True,  # calc.py, src/models.py (no), src/db.py
        "answer_contains": "calc.py",
        "expects": "greps project, finds calc.py and src/db.py",
    },
    {
        "id": 8,
        "prompt": (
            "Run this exact command with bash and report the output: "
            "python3 -c \"import json; print(json.load(open('config.json'))['max_retries'] * 7)\""
        ),
        "verify": lambda p: True,
        "answer_contains": "21",
        "expects": "bash exec, answers 21",
    },
    {
        "id": 9,
        "prompt": (
            "Read data.csv and compute the average score of the five rows. "
            "Reply with the number (1 decimal place is fine)."
        ),
        "verify": lambda p: True,
        "answer_contains": "82.4",
        "expects": "reads csv, answers 82.4",
    },
    {
        "id": 10,
        "prompt": (
            "Count the total lines across calc.py, src/models.py, and src/db.py "
            "using wc -l. Report the three individual counts."
        ),
        "verify": lambda p: True,
        "expects": "bash wc -l on 3 files",
    },

    # ── Phase 3: Edit (surgical changes) ────────────────────────────
    {
        "id": 11,
        "prompt": (
            "Edit calc.py: add a fifth function pow(a, b) that returns a ** b. "
            "Keep the existing four functions unchanged."
        ),
        "verify": lambda p: (
            (s := _read(p, "calc.py")) is not None
            and "def pow(" in s
            and "def add(" in s
            and "def div(" in s
        ),
        "expects": "calc.py gains pow(), keeps others",
    },
    {
        "id": 12,
        "prompt": (
            "Edit config.json: change debug to true and bump version to 1.1.0. "
            "Leave the other keys untouched."
        ),
        "verify": lambda p: (
            (s := _read(p, "config.json")) is not None
            and json.loads(s)
            == {"name": "bench", "version": "1.1.0", "debug": True, "max_retries": 3}
        ),
        "expects": "config.json: debug=true, version=1.1.0",
    },
    {
        "id": 13,
        "prompt": (
            "Edit src/models.py: add a method greeting(self) to the User class "
            "that returns 'Hello, ' followed by the name attribute. "
            "Then add a second class Admin that inherits from User and overrides "
            "greeting to prepend 'Admin: ' to the parent's result."
        ),
        "verify": lambda p: (
            (s := _read(p, "src/models.py")) is not None
            and "def greeting(" in s
            and "class Admin" in s
        ),
        "expects": "models.py: greeting() + Admin subclass",
    },
    {
        "id": 14,
        "prompt": (
            "Append a sixth row to data.csv: 6,frank,71 — then read the file "
            "back and confirm the data row count (excluding header) in your answer."
        ),
        "verify": lambda p: (
            (s := _read(p, "data.csv")) is not None
            and "6,frank,71" in s
            and len(s.strip().splitlines()) == 7
        ),
        "answer_contains": "6",
        "expects": "data.csv has 6 data rows, answer says 6",
    },
    {
        "id": 15,
        "prompt": (
            "Rename the function 'connect' in src/db.py to 'open_connection' "
            "and change its return string to 'connection open'. "
            "Use a surgical edit, not a full rewrite."
        ),
        "verify": lambda p: (
            (s := _read(p, "src/db.py")) is not None
            and "def open_connection(" in s
            and "connection open" in s
            and "def connect(" not in s
        ),
        "expects": "db.py: connect renamed to open_connection",
    },

    # ── Phase 4: Multi-file refactor (heavy tool churn) ─────────────
    {
        "id": 16,
        "prompt": (
            "Create tests/test_calc.py with pytest tests covering all five "
            "functions in calc.py — at minimum one test per function, and the "
            "div test must assert the ValueError on division by zero. "
            "Create the tests directory if needed, then run the tests with "
            "bash and report pass/fail counts."
        ),
        "verify": lambda p: (
            (s := _read(p, "tests/test_calc.py")) is not None
            and "def test_" in s
            and "ValueError" in s
        ),
        "answer_contains": "5",
        "expects": "tests written and executed, 5+ passing",
    },
    {
        "id": 17,
        "prompt": (
            "Create a script stats.py that reads data.csv, computes min, max, "
            "and mean of the score column, and prints them as "
            "'min=X max=Y mean=Z' with mean to 1 decimal. Run it with bash "
            "and report the exact output line."
        ),
        "verify": lambda p: (
            (s := _read(p, "stats.py")) is not None and "data.csv" in s
        ),
        "answer_contains": "min=68",
        "expects": "stats.py runs: min=68 max=92 mean=80.5",
    },
    {
        "id": 18,
        "prompt": (
            "Move the User and Admin classes from src/models.py into a new "
            "file src/users.py, and make src/models.py a re-export shim: "
            "from src.users import User, Admin. Verify the shim works by "
            "running: python3 -c \"from src.models import User; "
            "print(User('x','y').name)\""
        ),
        "verify": lambda p: (
            (u := _read(p, "src/users.py")) is not None
            and "class User" in u
            and "class Admin" in u
            and (m := _read(p, "src/models.py")) is not None
            and "import" in m
            and "class User" not in m
        ),
        "answer_contains": "x",
        "expects": "classes moved to users.py, models.py is a shim",
    },

    # ── Phase 5: Cross-cutting finale (grep + fix + verify) ─────────
    {
        "id": 19,
        "prompt": (
            "Find every Python file in this project containing the word "
            "'class' using grep, then create an INVENTORY.md file listing "
            "each class name and the file it lives in, one per line in the "
            "format: ClassName - path/to/file.py"
        ),
        "verify": lambda p: (
            (s := _read(p, "INVENTORY.md")) is not None
            and "User" in s
            and "Admin" in s
            and "users.py" in s
        ),
        "expects": "INVENTORY.md lists User + Admin in src/users.py",
    },
    {
        "id": 20,
        "prompt": (
            "There is a deliberate bug being introduced now: run this with bash "
            "first: echo 'def broken(: pass' >> calc.py — then run "
            "python3 -m py_compile calc.py, observe the syntax error, fix it "
            "by removing the bad line with an edit, and re-run py_compile to "
            "confirm calc.py compiles clean again."
        ),
        "verify": lambda p: (
            (s := _read(p, "calc.py")) is not None
            and "def broken(:" not in s
            and "def pow(" in s
        ),
        "expects": "bug injected, caught, removed; calc.py compiles",
    },
    {
        "id": 21,
        "prompt": (
            "Final audit: run stats.py one more time, run the test suite one "
            "more time, and read config.json. Then write a file called "
            "AUDIT.txt containing exactly three lines: line 1 the stats output, "
            "line 2 the number of passing tests, line 3 the version string "
            "from config.json."
        ),
        "verify": lambda p: (
            (s := _read(p, "AUDIT.txt")) is not None
            and len(s.strip().splitlines()) == 3
            and "1.1.0" in s
        ),
        "expects": "AUDIT.txt: 3 lines, version 1.1.0 present",
    },
]


def question_count():
    return len(QUESTIONS)


if __name__ == "__main__":
    print(f"{question_count()} questions defined")
    for q in QUESTIONS:
        print(f"  Q{q['id']:2d}: {q['expects']}")
