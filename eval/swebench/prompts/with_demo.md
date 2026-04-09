You are working on the {repo} repository, checked out at commit {base_commit_short}.

A user reported the following issue. Read it carefully, explore the
repository to locate the relevant code, understand the root cause,
and make the minimal changes necessary to fix it.

Repository: {repo}
Base commit: {base_commit}

--- ISSUE ---
{problem_statement}
--- END ISSUE ---
{hints_block}

Rules:
- DO NOT modify any test file (anything under tests/, test_*.py,
  *_test.py, conftest.py).
- DO NOT add new dependencies.
- Prefer surgical fixes over rewrites — do not refactor surrounding
  code unrelated to the fix.
- You do not have access to the hidden test suite that grades your
  patch. Verify your fix by reading the code and reasoning about it,
  not by running the full test suite.

Workflow you should follow:
  1. LOCATE — Use grep with a distinctive symbol or string from the
     issue (a function name, error message, class) to find the file
     and line. Avoid blind list_directory walks.
  2. UNDERSTAND — Open just the relevant function with read_file
     using offset/limit. Read its callers if needed (one grep is
     usually enough). Stop reading once you can explain the bug in
     one sentence.
  3. FIX — Use edit_file with the smallest possible old_string /
     new_string pair that resolves the bug. Do not rewrite the
     surrounding function.
  4. CHECK — Re-read only the edited region to confirm the change
     applied as intended. Do not run pip install, do not run the
     project test suite. Then finish.

A typical good run looks like this (for a different issue, just to
illustrate the rhythm and tool sequence — DO NOT copy these literal
file paths or arguments, they are from a different bug):

    Turn 1 — grep("def parse_duration", path="src/utils")
    Turn 2 — read_file("src/utils/dateparse.py", offset=80, limit=40)
    Turn 3 — grep("parse_duration\\(", path="src/")  # check callers
    Turn 4 — read_file("src/utils/dateparse.py", offset=120, limit=20)
    Turn 5 — edit_file(
                file_path="src/utils/dateparse.py",
                old_string="    if not match:\\n        return None",
                new_string="    if not match:\\n        return None\\n    "
                           "groups = match.groupdict()\\n    "
                           "if groups.get('sign') == '-':\\n        "
                           "groups = {{k: '-' + v if v else v for k, v in groups.items()}}",
             )
    Turn 6 — read_file("src/utils/dateparse.py", offset=120, limit=20)
            (verify the new code is in place)
    Turn 7 — done

Notice the discipline in that run:
  - 7 turns total, not 25
  - One grep, then one read of the exact lines, no whole-file dump
  - Callers checked once, then commit to the fix
  - No bash, no pip install, no test runs
  - Verification is a re-read of the edited region, nothing else

Your turn now. Begin with a grep using the most distinctive symbol
from the issue above.
