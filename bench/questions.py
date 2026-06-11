
QUESTIONS = [
    # ── Phase 1: Scaffold a FastAPI project (heavy writes) ──────────

    {
        "id": 1,
        "prompt": (
            "Create a FastAPI project with the following structure. "
            "First create app/__init__.py (empty). "
            "Then create app/models.py containing Pydantic models: "
            "UserCreate(name: str, email: str, age: int), "
            "UserResponse(id: int, name: str, email: str, age: int, created_at: str), "
            "ItemCreate(title: str, description: str, price: float, owner_id: int), "
            "ItemResponse(id: int, title: str, description: str, price: float, owner_id: int, created_at: str), "
            "ErrorResponse(detail: str, code: int), and "
            "PaginatedResponse(items: list, total: int, page: int, per_page: int). "
            "Include proper type hints and Field validators: name must be 2-50 chars, "
            "email must contain @, age must be 18-120, price must be > 0. "
            "After creating the file, read it back and count the total lines."
        ),
        "verify": lambda p: (
            (s := _read(p, "app/models.py")) is not None
            and all(c in s for c in ("UserCreate", "UserResponse", "ItemCreate",
                                      "ItemResponse", "ErrorResponse", "PaginatedResponse"))
            and "Field(" in s
            and _lines(p, "app/models.py") >= 35
        ),
        "expects": "app/models.py with 6 Pydantic models + validators (35+ lines)",
    },

    {
        "id": 2,
        "prompt": (
            "Create app/database.py that implements an in-memory database using "
            "Python dicts. It should have: a Database class with __init__ that creates "
            "empty dicts for users and items plus auto-increment counters; "
            "create_user(data) that stores and returns a user dict with id and "
            "created_at (ISO format); get_user(user_id) returning user or None; "
            "list_users(page, per_page) with pagination returning (users_list, total); "
            "create_item(data) similar to create_user; get_item(item_id); "
            "list_items(owner_id=None, min_price=None, max_price=None, page=1, per_page=10) "
            "with filtering and pagination; update_item(item_id, data) that merges fields; "
            "delete_item(item_id) returning bool. "
            "Add a module-level instance: db = Database(). "
            "After creating, read the file back and verify it has all 8 methods by "
            "running: python3 -c \"from app.database import db; print([m for m in dir(db) if not m.startswith('_')])\""
        ),
        "verify": lambda p: (
            (s := _read(p, "app/database.py")) is not None
            and all(m in s for m in ("create_user", "get_user", "list_users",
                                      "create_item", "get_item", "list_items",
                                      "update_item", "delete_item"))
            and "class Database" in s
            and _lines(p, "app/database.py") >= 50
        ),
        "expects": "app/database.py: Database class with 8 methods, 50+ lines",
    },

    {
        "id": 3,
        "prompt": (
            "Create app/routes.py with FastAPI router endpoints. Import the models "
            "and database. Create a router = APIRouter(). Implement these endpoints: "
            "POST /users -> create user, GET /users/{id} -> get user (404 if missing), "
            "GET /users -> list users with page/per_page query params (defaults 1, 10), "
            "POST /items -> create item, GET /items/{id} -> get item (404 if missing), "
            "GET /items -> list items with optional owner_id, min_price, max_price filters "
            "plus pagination, PUT /items/{id} -> update item (404 if missing), "
            "DELETE /items/{id} -> delete item (404 if missing). "
            "Use proper HTTP status codes (201 for creates, 204 for delete, 404 for not found). "
            "Use the ErrorResponse model for error responses. "
            "After creating, read it back, then grep for 'def ' to count the endpoint functions."
        ),
        "verify": lambda p: (
            (s := _read(p, "app/routes.py")) is not None
            and all(ep in s for ep in ("POST", "GET", "PUT", "DELETE",
                                        "/users", "/items"))
            and s.count("def ") >= 8
            and _lines(p, "app/routes.py") >= 60
        ),
        "expects": "app/routes.py: 8 endpoints, proper status codes, 60+ lines",
    },

    {
        "id": 4,
        "prompt": (
            "Create app/main.py that imports FastAPI, the router from routes, and "
            "sets up the app with title='Benchmark API', version='1.0.0', "
            "description='A benchmark FastAPI project'. Include the router, add "
            "a root endpoint GET / returning {status: 'ok', version: '1.0.0'}, "
            "add a GET /health endpoint returning {healthy: true, checks: {database: 'ok', "
            "uptime: <float seconds since start>}} (track start time with a module-level "
            "datetime.now()). Add CORS middleware allowing all origins. "
            "Then create a requirements.txt with fastapi, uvicorn, pydantic, pytest, httpx. "
            "After creating both files, run: python3 -c \"from app.main import app; "
            "print(app.title, app.version)\" to verify it imports cleanly."
        ),
        "verify": lambda p: (
            (s := _read(p, "app/main.py")) is not None
            and "FastAPI" in s and "1.0.0" in s and "CORS" in s.upper()
            and (r := _read(p, "requirements.txt")) is not None
            and "fastapi" in r and "pytest" in r
        ),
        "expects": "app/main.py (CORS, health, root) + requirements.txt",
    },

    {
        "id": 5,
        "prompt": (
            "Create a comprehensive seed script at scripts/seed.py that: "
            "1) imports the database, 2) creates 10 users with realistic names/emails "
            "(alice@example.com through jane@example.com, ages 22-55), "
            "3) creates 20 items across those users with varied titles, descriptions "
            "(at least 30 chars each), and prices ranging from $9.99 to $299.99, "
            "4) prints a summary: 'Seeded X users and Y items', "
            "5) prints the most expensive item's title and price, "
            "6) prints the user with the most items. "
            "After creating it, run it with bash and report the full output. "
            "Then list all files in the project recursively with 'find . -type f | sort'."
        ),
        "verify": lambda p: (
            (s := _read(p, "scripts/seed.py")) is not None
            and "create_user" in s and "create_item" in s
            and _lines(p, "scripts/seed.py") >= 40
        ),
        "expects": "scripts/seed.py: 10 users, 20 items, runs and reports stats",
    },

    # ── Phase 2: Deep inspection chains (read → compute → read → answer) ──

    {
        "id": 6,
        "prompt": (
            "I need a full code review of the database module. Read app/database.py, "
            "then read app/models.py for context on the data shapes. "
            "Then run: python3 -c \"from app.database import db; "
            "db.create_user({'name':'test','email':'t@t.com','age':25}); "
            "db.create_user({'name':'test2','email':'t2@t.com','age':30}); "
            "print('users:', db.list_users(1,10)); "
            "db.create_item({'title':'Widget','description':'A test widget for benchmarking','price':19.99,'owner_id':1}); "
            "print('items:', db.list_items())\" "
            "Report the exact output, then list all issues you see in database.py "
            "(missing validation, edge cases, etc). Write your findings to REVIEW.md."
        ),
        "verify": lambda p: (
            (s := _read(p, "REVIEW.md")) is not None
            and len(s) >= 200
        ),
        "expects": "REVIEW.md with code review findings (200+ chars)",
    },

    {
        "id": 7,
        "prompt": (
            "Run the seed script (python3 scripts/seed.py), then write a query "
            "script at scripts/query.py that imports the database, runs the seed, "
            "and then: 1) finds all items priced over $100 and prints count + titles, "
            "2) groups items by owner_id and prints owner_id: count for each, "
            "3) computes the average price across all items (1 decimal), "
            "4) finds the owner with highest total item value and prints their user info. "
            "Run scripts/query.py and report the full output."
        ),
        "verify": lambda p: (
            (s := _read(p, "scripts/query.py")) is not None
            and "list_items" in s
        ),
        "expects": "scripts/query.py with 4 queries, executed with output",
    },

    {
        "id": 8,
        "prompt": (
            "Read every Python file in the app/ directory. Count: "
            "1) total lines across all files, 2) total number of function definitions "
            "(lines containing 'def '), 3) total number of class definitions, "
            "4) every import statement. Run this bash one-liner to cross-check: "
            "find app -name '*.py' -exec cat {} + | wc -l && "
            "grep -r 'def ' app/ | wc -l && grep -r 'class ' app/ | wc -l. "
            "Then create a file PROJECT_STATS.md with a markdown table of these metrics."
        ),
        "verify": lambda p: (
            (s := _read(p, "PROJECT_STATS.md")) is not None
            and "|" in s
            and "def" in s.lower()
        ),
        "expects": "PROJECT_STATS.md with metrics table from reading all app/ files",
    },

    {
        "id": 9,
        "prompt": (
            "Read app/routes.py, then read app/database.py, then read app/models.py. "
            "Trace the full request flow for POST /items: which route handler, which "
            "model validates input, which database method stores it. "
            "Then do the same for GET /items with filters. "
            "Create a file ARCHITECTURE.md documenting both flows as step-by-step "
            "numbered lists. Include the exact function signatures involved at each step. "
            "After writing it, read it back to verify it's correct."
        ),
        "verify": lambda p: (
            (s := _read(p, "ARCHITECTURE.md")) is not None
            and "POST" in s and "GET" in s
            and "create_item" in s
            and len(s) >= 400
        ),
        "expects": "ARCHITECTURE.md with traced request flows (400+ chars)",
    },

    {
        "id": 10,
        "prompt": (
            "Write a comprehensive bash script at scripts/analyze.sh that: "
            "1) prints '=== Project Structure ===' then runs find . -type f | sort, "
            "2) prints '=== Line Counts ===' then wc -l on every .py file, "
            "3) prints '=== TODO/FIXME ===' then greps for TODO or FIXME across all files, "
            "4) prints '=== Import Graph ===' then for each .py file in app/, "
            "   extracts import lines and prints 'file: imports module', "
            "5) prints '=== Validation Rules ===' then greps for Field( in models.py "
            "   and prints each match. "
            "Make it executable, run it with bash, capture the full output. "
            "The output will be long — that's intentional."
        ),
        "verify": lambda p: (
            (s := _read(p, "scripts/analyze.sh")) is not None
            and "find" in s and "wc" in s and "grep" in s
        ),
        "expects": "scripts/analyze.sh runs 5 analysis sections with output",
    },

    # ── Phase 3: Multi-file refactors (read → plan → edit → verify chains) ──

    {
        "id": 11,
        "prompt": (
            "Add comprehensive input validation to app/database.py. "
            "First read app/models.py to understand the validation rules, "
            "then read app/database.py. Add validation in create_user: "
            "raise ValueError if name is empty or >50 chars, if email doesn't "
            "contain @, if age is not 18-120. Add validation in create_item: "
            "raise ValueError if title is empty, if price <= 0, if owner_id "
            "doesn't exist in the users dict. Use surgical edits, not rewrites. "
            "After editing, run: python3 -c \"from app.database import db; "
            "try: db.create_user({'name':'','email':'bad','age':10}); "
            "except ValueError as e: print('caught:', e)\" "
            "to verify the validation works."
        ),
        "verify": lambda p: (
            (s := _read(p, "app/database.py")) is not None
            and "ValueError" in s
            and s.count("raise ValueError") >= 3
        ),
        "expects": "database.py: 3+ ValueError raises in create methods",
    },

    {
        "id": 12,
        "prompt": (
            "The routes need proper error handling. Read app/routes.py, then "
            "read app/database.py to see what exceptions it can raise. "
            "Edit app/routes.py to wrap every create/update endpoint in try/except "
            "that catches ValueError and returns a 422 with ErrorResponse. "
            "Also add input length logging: before each create, print the request "
            "body size to stderr. After editing, read the file back and count "
            "the number of try/except blocks to confirm. Then run: "
            "python3 -c \"from app.routes import router; print(len(router.routes), 'routes registered')\""
        ),
        "verify": lambda p: (
            (s := _read(p, "app/routes.py")) is not None
            and s.count("except") >= 3
            and "422" in s
        ),
        "expects": "routes.py: 3+ try/except blocks with 422 responses",
    },

    {
        "id": 13,
        "prompt": (
            "Extract the validation logic from database.py into a new module "
            "app/validators.py. Read database.py first to identify all validation "
            "code. Create validators.py with functions: validate_user(data) and "
            "validate_item(data, existing_user_ids) that raise ValueError with "
            "descriptive messages. Then edit database.py to import and use these "
            "validators instead of inline checks. After refactoring, run the "
            "validation test again: python3 -c \"from app.database import db; "
            "try: db.create_user({'name':'','email':'bad','age':10}); "
            "except ValueError as e: print('still works:', e)\" "
            "Then read both files back to verify the refactor is clean."
        ),
        "verify": lambda p: (
            (v := _read(p, "app/validators.py")) is not None
            and "validate_user" in v and "validate_item" in v
            and (d := _read(p, "app/database.py")) is not None
            and "from app.validators import" in d.replace("from .validators import", "from app.validators import")
        ),
        "expects": "validators.py extracted, database.py imports it",
    },

    {
        "id": 14,
        "prompt": (
            "Add a logging system. Create app/logger.py with a configure_logging() "
            "function that sets up Python's logging module with format "
            "'%(asctime)s [%(levelname)s] %(name)s: %(message)s'. "
            "Then edit app/database.py to add logging: log every create/update/delete "
            "operation at INFO level with the entity type and id, and log validation "
            "failures at WARNING level. Read database.py to plan the edits first, "
            "then make the edits, then run the seed script and capture the log output. "
            "After that, read database.py back to verify all log calls are present."
        ),
        "verify": lambda p: (
            (l := _read(p, "app/logger.py")) is not None
            and "configure_logging" in l
            and (d := _read(p, "app/database.py")) is not None
            and d.count("log") >= 5
        ),
        "expects": "logger.py created, database.py has 5+ log calls",
    },

    {
        "id": 15,
        "prompt": (
            "Add search functionality. Read app/database.py and app/routes.py first. "
            "Then edit database.py to add a search_items(query, fields=None) method "
            "that does case-insensitive substring matching across title and description "
            "(or specified fields). Also add search_users(query) matching name and email. "
            "Then edit routes.py to add GET /search?q=<query>&type=users|items|all "
            "that returns combined results with the type labeled. "
            "After all edits, run: python3 -c \"from app.database import db; "
            "db.create_user({'name':'Alice Test','email':'alice@test.com','age':25}); "
            "db.create_item({'title':'Test Widget','description':'A widget for testing purposes','price':9.99,'owner_id':1}); "
            "print(db.search_items('widget')); print(db.search_users('alice'))\" "
            "to verify it works."
        ),
        "verify": lambda p: (
            (d := _read(p, "app/database.py")) is not None
            and "search_items" in d and "search_users" in d
            and (r := _read(p, "app/routes.py")) is not None
            and "/search" in r
        ),
        "expects": "search methods in database.py, /search endpoint in routes.py",
    },

    # ── Phase 4: Complex cross-cutting tasks ────────────────────────

    {
        "id": 16,
        "prompt": (
            "Create a full test suite at tests/test_database.py. "
            "Read app/database.py and app/validators.py first to understand "
            "what to test. Write tests covering: "
            "- create_user happy path + all validation errors (empty name, bad email, bad age) "
            "- get_user found + not found "
            "- list_users pagination (create 15 users, verify page 1 has 10, page 2 has 5) "
            "- create_item happy path + validation errors (bad price, missing owner) "
            "- list_items filtering (by owner_id, by price range) "
            "- update_item happy path + not found "
            "- delete_item happy path + not found "
            "- search_items and search_users "
            "Each test must have a descriptive name. Use pytest fixtures for a fresh "
            "Database instance. After creating, run: python3 -m pytest tests/test_database.py -v "
            "and report the full output."
        ),
        "verify": lambda p: (
            (s := _read(p, "tests/test_database.py")) is not None
            and s.count("def test_") >= 12
            and "fixture" in s.lower() or "@pytest" in s
        ),
        "expects": "tests/test_database.py: 12+ tests, run with pytest output",
    },

    {
        "id": 17,
        "prompt": (
            "Create tests/test_routes.py that tests the API endpoints using "
            "httpx and FastAPI's TestClient. Read app/routes.py and app/main.py first. "
            "Write tests for: POST /users (valid + invalid), GET /users/{id} (found + 404), "
            "POST /items (valid + invalid + nonexistent owner), GET /items with filters, "
            "PUT /items/{id} (valid + 404), DELETE /items/{id} (valid + 404), "
            "GET /search with various queries, GET / and GET /health. "
            "Use a fixture that creates the TestClient. After creating, "
            "run pytest tests/test_routes.py -v and report full output. "
            "If any tests fail, read the error, fix the test or the code, and re-run."
        ),
        "verify": lambda p: (
            (s := _read(p, "tests/test_routes.py")) is not None
            and s.count("def test_") >= 10
            and "TestClient" in s
        ),
        "expects": "tests/test_routes.py: 10+ integration tests with TestClient",
    },

    {
        "id": 18,
        "prompt": (
            "Create a data migration script at scripts/migrate.py that: "
            "1) reads the current database state (run seed first), "
            "2) adds a 'tags' field (list of strings) to every item — derive tags "
            "   from the title/description using keyword extraction (split words, "
            "   lowercase, filter out words < 4 chars), "
            "3) adds an 'is_premium' boolean to every user (True if they own any item > $100), "
            "4) creates a migration log file at data/migration_log.json recording: "
            "   timestamp, items_updated count, users_updated count, sample of first "
            "   3 items with their new tags, "
            "5) prints a summary of all changes. "
            "Run the script and verify the migration log was created by reading it back."
        ),
        "verify": lambda p: (
            (s := _read(p, "scripts/migrate.py")) is not None
            and "tags" in s and "is_premium" in s
        ),
        "expects": "scripts/migrate.py: adds tags+is_premium, creates migration log",
    },

    {
        "id": 19,
        "prompt": (
            "Perform a full security audit. Read every file in app/ one by one. "
            "Then create SECURITY_AUDIT.md documenting: "
            "1) Input validation gaps (any endpoint that doesn't validate, any "
            "   field that could be exploited — SQL injection doesn't apply since "
            "   we use dicts, but look for XSS in stored fields, missing length limits), "
            "2) Authentication gaps (there's no auth at all — document what's needed), "
            "3) Rate limiting gaps (none exists — recommend approach), "
            "4) Data exposure risks (are any internal fields leaking?), "
            "5) Specific code fixes with file:line references. "
            "The audit should be at least 50 lines long. After writing it, "
            "read it back and verify it references all 4 app/ source files."
        ),
        "verify": lambda p: (
            (s := _read(p, "SECURITY_AUDIT.md")) is not None
            and _lines(p, "SECURITY_AUDIT.md") >= 40
            and all(f in s for f in ("models.py", "database.py", "routes.py"))
        ),
        "expects": "SECURITY_AUDIT.md: 40+ lines referencing all source files",
    },

    {
        "id": 20,
        "prompt": (
            "There are bugs being introduced now. Run these bash commands: "
            "echo '    def broken_method(self):' >> app/database.py && "
            "echo '        return self.nonexistent_attr' >> app/database.py && "
            "sed -i 's/def get_user/def getuser/' app/database.py && "
            "echo 'import nonexistent_module' >> app/routes.py. "
            "Now: 1) try to import the app and observe the errors, "
            "2) read the affected files to understand what broke, "
            "3) fix all three bugs (remove broken_method, restore get_user name, "
            "   remove bad import) using surgical edits, "
            "4) verify the fix by importing the app again, "
            "5) run the database tests to confirm nothing else broke."
        ),
        "verify": lambda p: (
            (d := _read(p, "app/database.py")) is not None
            and "def get_user(" in d
            and "nonexistent_attr" not in d
            and (r := _read(p, "app/routes.py")) is not None
            and "import nonexistent_module" not in r
        ),
        "expects": "3 injected bugs found and fixed, tests pass",
    },

    {
        "id": 21,
        "prompt": (
            "Final comprehensive audit. "
            "1) Read every .py file in app/ and count total lines, functions, classes. "
            "2) Run the full test suite: python3 -m pytest tests/ -v "
            "3) Run the analyze script: bash scripts/analyze.sh "
            "4) Read config if any exists. "
            "5) Create FINAL_REPORT.md with: "
            "   - Project structure (list all files with line counts), "
            "   - Test results summary (X passed, Y failed), "
            "   - Code metrics (functions, classes, total lines), "
            "   - Architecture summary (how the modules connect), "
            "   - Top 3 recommended improvements. "
            "The report should be thorough — at least 60 lines. "
            "After creating it, read it back to verify completeness."
        ),
        "verify": lambda p: (
            (s := _read(p, "FINAL_REPORT.md")) is not None
            and _lines(p, "FINAL_REPORT.md") >= 50
            and "test" in s.lower()
            and "routes" in s.lower()
        ),
        "expects": "FINAL_REPORT.md: 50+ lines, tests + metrics + recommendations",
    },
]


def question_count():
    return len(QUESTIONS)


if __name__ == "__main__":
    print(f"{question_count()} questions defined")
    for q in QUESTIONS:
        print(f"  Q{q['id']:2d}: {q['expects']}")
