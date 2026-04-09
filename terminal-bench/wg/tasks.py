"""
Shared Terminal Bench task definitions with verify commands.

Each task has: id, title, instruction_file, verify_cmd, difficulty.
verify_cmd is a shell command that exits 0 on success.

Used by:
  - wg/adapter.py (Harbor adapter) — to inject --verify gates into wg tasks
  - tb_trial_runner.py — to create trial tasks with --verify
  - run_pilot_f_89.py — pilot F runner
"""

# 8 calibration tasks (easy, medium, hard)
CALIBRATION_TASKS = [
    {
        "id": "file-ops",
        "title": "File Operations: create project structure",
        "instruction_file": "tasks/condition-a-calibration/01-file-ops-easy.txt",
        "verify_cmd": (
            "test -f /tmp/project/src/main.py && "
            "test -f /tmp/project/src/utils.py && "
            "test -f /tmp/project/src/tests/test_utils.py && "
            "test -f /tmp/project/data/config.json && "
            "test -f /tmp/project/README.md && "
            "test -f /tmp/project/.gitignore && "
            "python3 -c \"import json; json.load(open('/tmp/project/data/config.json'))\" && "
            "python3 -m pytest /tmp/project/src/tests/test_utils.py -v"
        ),
        "difficulty": "easy",
    },
    {
        "id": "text-processing",
        "title": "Text Processing: word frequency counter",
        "instruction_file": "tasks/condition-a-calibration/02-text-processing-easy.txt",
        "verify_cmd": (
            "test -f /tmp/wordfreq.py && "
            "echo 'the the the dog dog cat' | python3 /tmp/wordfreq.py | head -1 | grep -q 'the'"
        ),
        "difficulty": "easy",
    },
    {
        "id": "debugging",
        "title": "Debugging: fix merge sort bugs",
        "instruction_file": "tasks/condition-a-calibration/03-debugging-medium.txt",
        "verify_cmd": (
            "test -f /tmp/buggy_sort.py && "
            "python3 /tmp/buggy_sort.py 2>&1 | grep -v FAIL | grep -c PASS | "
            "python3 -c \"import sys; n=int(sys.stdin.read().strip()); sys.exit(0 if n>=6 else 1)\""
        ),
        "difficulty": "medium",
    },
    {
        "id": "shell-scripting",
        "title": "Shell Scripting: log file analyzer",
        "instruction_file": "tasks/condition-a-calibration/04-shell-scripting-medium.txt",
        "verify_cmd": (
            "test -f /tmp/log_analyzer.sh && "
            "test -f /tmp/access.log && "
            "bash /tmp/log_analyzer.sh /tmp/access.log 2>&1 | grep -qE '[0-9]'"
        ),
        "difficulty": "medium",
    },
    {
        "id": "data-processing",
        "title": "Data Processing: JSON to CSV department summary",
        "instruction_file": "tasks/condition-a-calibration/05-data-processing-medium.txt",
        "verify_cmd": (
            "test -f /tmp/json_to_csv.py && "
            "test -f /tmp/employees.json && "
            "test -f /tmp/dept_summary.csv && "
            "python3 -c \"import csv; r=list(csv.DictReader(open('/tmp/dept_summary.csv'))); "
            "assert len(r)>=1\""
        ),
        "difficulty": "medium",
    },
    {
        "id": "algorithm",
        "title": "Algorithm: key-value store with transactions",
        "instruction_file": "tasks/condition-a-calibration/06-algorithm-hard.txt",
        "verify_cmd": (
            "test -f /tmp/kvstore.py && test -f /tmp/kv_test.txt && "
            "python3 /tmp/kvstore.py < /tmp/kv_test.txt | head -1 | grep -q '10'"
        ),
        "difficulty": "hard",
    },
    {
        "id": "ml",
        "title": "ML: k-means clustering from scratch",
        "instruction_file": "tasks/condition-a-calibration/07-ml-hard.txt",
        "verify_cmd": (
            "test -f /tmp/kmeans.py && "
            "python3 /tmp/kmeans.py 2>&1 | "
            "python3 -c \"import sys; o=sys.stdin.read().lower(); "
            "sys.exit(0 if 'centroid' in o or 'cluster' in o else 1)\""
        ),
        "difficulty": "hard",
    },
    {
        "id": "sysadmin",
        "title": "Sysadmin: rate-limited HTTP server",
        "instruction_file": "tasks/condition-a-calibration/08-sysadmin-hard.txt",
        "verify_cmd": (
            "test -f /tmp/ratelimit_server.py && "
            "python3 -c \"import ast; ast.parse(open('/tmp/ratelimit_server.py').read())\" && "
            "grep -q '8765' /tmp/ratelimit_server.py && "
            "grep -q '429' /tmp/ratelimit_server.py && "
            "grep -qi 'rate' /tmp/ratelimit_server.py"
        ),
        "difficulty": "hard",
    },
]

# 10 hard benchmark tasks
HARD_BENCHMARK_TASKS = [
    {
        "id": "configure-git-webserver",
        "title": "Configure Git Webserver: bare repo + post-receive hook + HTTP server",
        "instruction_file": "tasks/hard-benchmarks/01-configure-git-webserver.txt",
        "verify_cmd": (
            "test -d /tmp/git-server/repo.git && "
            "test -x /tmp/git-server/repo.git/hooks/post-receive && "
            "test -f /tmp/web/html/index.html && "
            "grep -q 'Version 2' /tmp/web/html/index.html && "
            "test -f /tmp/web/deploy.log && "
            "test $(wc -l < /tmp/web/deploy.log) -ge 2"
        ),
        "difficulty": "hard",
    },
    {
        "id": "mailman",
        "title": "Mailman: local mail system with mailing list manager",
        "instruction_file": "tasks/hard-benchmarks/02-mailman.txt",
        "verify_cmd": (
            "test -f /tmp/mailman/list_manager.py && "
            "test -f /tmp/mailman/cli.py && "
            "python3 -c \""
            "import json; "
            "members = json.load(open('/tmp/mailman/lists/test-list/members.json')); "
            "assert len(members) == 2, f'Expected 2 members, got {len(members)}'"
            "\" && "
            "python3 -c \""
            "import os; "
            "archive = '/tmp/mailman/lists/test-list/archive'; "
            "count = len([f for f in os.listdir(archive) if os.path.isfile(os.path.join(archive, f))]); "
            "assert count == 3, f'Expected 3 archive messages, got {count}'"
            "\""
        ),
        "difficulty": "hard",
    },
    {
        "id": "multi-source-data-merger",
        "title": "Multi-Source Data Merger: 3 formats -> merge -> conflict report",
        "instruction_file": "tasks/hard-benchmarks/03-multi-source-data-merger.txt",
        "verify_cmd": (
            "test -f /tmp/merger/merge.py && "
            "python3 /tmp/merger/merge.py && "
            "python3 -c \""
            "import csv; "
            "rows = list(csv.DictReader(open('/tmp/merger/output/merged.csv'))); "
            "assert len(rows) == 7, f'Expected 7 rows, got {len(rows)}'"
            "\" && "
            "python3 -c \""
            "import json; "
            "conflicts = json.load(open('/tmp/merger/output/conflicts.json')); "
            "assert len(conflicts) >= 4, f'Expected >= 4 conflicts, got {len(conflicts)}'"
            "\""
        ),
        "difficulty": "hard",
    },
    {
        "id": "financial-document-processor",
        "title": "Financial Document Processor: classify -> extract -> summarize",
        "instruction_file": "tasks/hard-benchmarks/04-financial-document-processor.txt",
        "verify_cmd": (
            "test -f /tmp/finproc/processor.py && "
            "test -f /tmp/finproc/summarizer.py && "
            "python3 /tmp/finproc/processor.py && "
            "python3 /tmp/finproc/summarizer.py && "
            "python3 -c \""
            "import os; "
            "extracted = [f for f in os.listdir('/tmp/finproc/extracted') if f.endswith('.json')]; "
            "assert len(extracted) == 5, f'Expected 5 extracted, got {len(extracted)}'"
            "\" && "
            "python3 -c '"
            "import json; "
            "d = json.load(open(\"/tmp/finproc/output/totals.json\")); "
            "assert abs(d[\"grand_total\"] - 6089.25) < 0.01"
            "'"
        ),
        "difficulty": "hard",
    },
    {
        "id": "cobol-modernization",
        "title": "COBOL Modernization: payroll COBOL -> Python with identical output",
        "instruction_file": "tasks/hard-benchmarks/05-cobol-modernization.txt",
        "verify_cmd": (
            "test -f /tmp/cobol-modern/python/payroll.py && "
            "test -f /tmp/cobol-modern/python/test_payroll.py && "
            "cd /tmp/cobol-modern && python3 python/payroll.py && "
            "cd /tmp/cobol-modern && python3 -m pytest python/test_payroll.py -v"
        ),
        "difficulty": "hard",
    },
    {
        "id": "build-cython-ext",
        "title": "Build Cython Extension: numpy integration, build, test",
        "instruction_file": "tasks/hard-benchmarks/06-build-cython-ext.txt",
        "verify_cmd": (
            "cd /tmp/cython-ext && "
            "python3 -c 'from fastmath import dot_product, matrix_multiply, moving_average, euclidean_distance; print(\"OK\")' && "
            "python3 -m pytest tests/ -v"
        ),
        "difficulty": "hard",
    },
    {
        "id": "fix-code-vulnerability",
        "title": "Fix Code Vulnerabilities: analyze -> report -> fix -> test",
        "instruction_file": "tasks/hard-benchmarks/07-fix-code-vulnerability.txt",
        "verify_cmd": (
            "test -f /tmp/vuln-app/vulnerability_report.json && "
            "test -f /tmp/vuln-app/app_fixed.py && "
            "python3 -c \""
            "import json; "
            "r = json.load(open('/tmp/vuln-app/vulnerability_report.json')); "
            "assert len(r) >= 6, f'Only {len(r)} findings'"
            "\""
        ),
        "difficulty": "hard",
    },
    {
        "id": "constraints-scheduling",
        "title": "Constraints Scheduling: ICS parsing + slot finding + meeting generation",
        "instruction_file": "tasks/hard-benchmarks/08-constraints-scheduling.txt",
        "verify_cmd": (
            "test -f /tmp/scheduler/find_slots.py && "
            "test -f /tmp/scheduler/schedule_meeting.py && "
            "python3 /tmp/scheduler/find_slots.py --date 2024-01-22 --duration 60 --participants alice,bob,carol && "
            "test -f /tmp/scheduler/output/meeting.ics && "
            "cd /tmp/scheduler && python3 -m pytest test_scheduler.py -v"
        ),
        "difficulty": "hard",
    },
    {
        "id": "multi-module-type-migration",
        "title": "Multi-Module Type Migration: UserId str -> dataclass across 6 modules",
        "instruction_file": "tasks/hard-benchmarks/09-multi-module-type-migration.txt",
        "verify_cmd": (
            "cd /tmp/type_migration && "
            "python3 -c 'from core.types import UserId; assert not isinstance(UserId, type(str))' && "
            "python3 -m pytest tests/ -v && "
            "python3 main.py"
        ),
        "difficulty": "hard",
    },
    {
        "id": "iterative-test-fix",
        "title": "Iterative Test Fix: 6 interrelated bugs, 15 tests, fix all",
        "instruction_file": "tasks/hard-benchmarks/10-iterative-test-fix.txt",
        "verify_cmd": (
            "cd /tmp/iterative_fix && "
            "python3 -m pytest tests/ -v 2>&1 | "
            "grep -c 'PASSED' | "
            "python3 -c 'import sys; n=int(sys.stdin.read().strip()); "
            "sys.exit(0 if n >= 15 else 1)'"
        ),
        "difficulty": "hard",
    },
]

# All tasks combined
ALL_TASKS = CALIBRATION_TASKS + HARD_BENCHMARK_TASKS

# Lookup by instruction_file path for matching instruction text → verify_cmd
_TASKS_BY_FILE = {t["instruction_file"]: t for t in ALL_TASKS}

# Lookup by task id
TASKS_BY_ID = {t["id"]: t for t in ALL_TASKS}


def lookup_verify_cmd(instruction: str) -> str | None:
    """Match instruction text to a known TB task and return its verify_cmd.

    Loads each instruction file and compares stripped content to find a match.
    Returns None if no match is found (e.g. custom/unknown task).
    """
    import os

    # Resolve TB root: this file is at terminal-bench/wg/tasks.py
    tb_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

    stripped_instruction = instruction.strip()
    for task_def in ALL_TASKS:
        filepath = os.path.join(tb_root, task_def["instruction_file"])
        try:
            with open(filepath) as f:
                file_content = f.read().strip()
            if file_content == stripped_instruction:
                return task_def["verify_cmd"]
        except FileNotFoundError:
            continue

    return None
