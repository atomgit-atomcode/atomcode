[Markdown] This batch contains documentation/README; pay extra attention to:

#### Dangerous Operation Instructions
- Does the document add unsafe commands or configurations, such as `chmod 777`, disabling TLS verification, disabling auth, exposing `0.0.0.0`, writing plaintext secrets, or using weak default passwords.
- Does the README / example commands guide users to execute untrusted scripts, pipe remote content, overwrite system directories, or leak tokens.

#### Configuration and Supply Chain Misguidance
- Does the document recommend installing suspicious dependencies, outdated high-risk dependencies, or unpinned versions, or configurations that conflict with security requirements in the code.
- Do examples for permissions, ports, CORS, databases, caches, and queues make the production environment insecure by default.

#### Testing and Usage Instructions
- If the document adds test/deployment steps, check whether they skip critical tests, hide failures, or mislead users into believing functionality is verified.

#### Reporting Boundary
- Only report documentation issues that cause security risks, production misuse, test failures, or data destruction; do not report wording, layout, or typos as pure documentation quality issues.
