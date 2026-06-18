[GraphQL] This batch contains GraphQL schemas/operations; pay extra attention to:

#### schema evolution
- Breaking changes: removing/renaming fields or types; changing nullability (nullable→non-null on input, non-null→nullable on output); changing argument types

#### security / perf
- Missing pagination on list fields (unbounded); N+1 resolver patterns (need batching / dataloader); unbounded query depth/complexity
- Sensitive fields exposed without authorization; introspection exposing internal types
