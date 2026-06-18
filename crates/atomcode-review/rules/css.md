[CSS] This batch contains CSS/SCSS/LESS; pay extra attention to:

#### correctness
- Selector specificity conflicts / unintended overrides; `!important` masking cascade issues
- Invalid or typo'd property names/values silently ignored; vendor-prefix-only properties without a standard fallback

#### maintainability / perf
- Hardcoded magic numbers / colors where variables exist; deeply nested SCSS selectors producing bloated output
- Expensive selectors / large reflows; z-index wars; unused or duplicate rules
