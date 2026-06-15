[Protobuf] This batch contains .proto files; pay extra attention to:

#### wire-compatibility (critical)
- Reusing or changing field numbers (breaks wire compat); changing field types incompatibly; renaming/removing fields without `reserved`
- Changing a field's label (`repeated` / `optional`) incompatibly

#### correctness
- Removing enum values without `reserved`; default-/zero-value semantics (proto3 scalars have no presence unless `optional`)
- Missing validation expectations; inconsistent package/option naming
