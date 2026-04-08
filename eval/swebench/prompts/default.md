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

Begin by exploring the repository structure with list_dir / grep /
read_file to locate the relevant code, then make the fix.
