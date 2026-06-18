[package.json] This batch contains package.json; pay extra attention to:
- Avoid introducing dependencies with versions set to `latest` or `*`; use deterministic version numbers.
  Note: when the version number is not on a newly added line, ignore this rule.
- Dependency conflicts or duplicate declarations: the same dependency exists in both `dependencies` and `devDependencies`
- Required tool dependencies not declared: tool names such as eslint, jest, prettier appear in `scripts` but are not listed in `devDependencies`
