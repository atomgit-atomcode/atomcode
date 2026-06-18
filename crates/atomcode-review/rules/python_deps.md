[Python Dependencies] This batch contains Python dependency/packaging configuration; pay extra attention to:

#### Supply Chain Security
- Are newly added or upgraded dependencies suspicious/malicious: typo-squatting,冷门包, unofficial replacements, or packages that clearly should not appear (e.g., dependencies that could trigger deserialization/RCE risks).
- Are versions outdated with high-severity CVEs; pay special attention to security-sensitive libraries (PyYAML, requests, Django, Flask, cryptography, etc.).
- Are dependency versions unpinned, using wildcards, or directly referencing untrusted URLs / git repositories / local paths.

#### Packaging Script Risks
- Do `setup.py` / build scripts perform network downloads, execute shell commands, write to system directories, read secrets, or modify permissions during installation.
- Do extras / scripts / entry_points expose dangerous commands or bypass auth entry points.

#### Reporting Boundary
- Only report real supply chain, CVE, or execution risks caused by newly added/modified dependencies; do not report pure "suggest upgrading to the latest version" advice.
