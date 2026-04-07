+++
id = "999-bad-frontmatter"
description = "intentionally invalid — malformed TOML syntax"
this line is deliberately not valid TOML >>>>
+++

This case should be reported as 'invalid' in the eval report,
demonstrating that the validation pipeline (and the TOML parse
error path specifically) works.
