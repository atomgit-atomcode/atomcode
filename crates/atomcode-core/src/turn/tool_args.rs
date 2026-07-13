use serde_json::{Map, Value};

/// Canonicalise model-supplied tool arguments for equality and hash keys.
///
/// Object keys are sorted recursively so the result is stable regardless of
/// whether another workspace dependency enables serde_json's `preserve_order`
/// feature. Array order remains significant, and malformed/free-form input is
/// preserved byte-for-byte.
pub(super) fn canonicalize_tool_args(arguments: &str) -> String {
    match serde_json::from_str(arguments) {
        Ok(value) => serde_json::to_string(&sort_object_keys(value))
            .unwrap_or_else(|_| arguments.to_string()),
        Err(_) => arguments.to_string(),
    }
}

fn sort_object_keys(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<_> = object.into_iter().collect();
            entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));

            let mut sorted = Map::new();
            for (key, value) in entries {
                sorted.insert(key, sort_object_keys(value));
            }
            Value::Object(sorted)
        }
        Value::Array(values) => {
            Value::Array(values.into_iter().map(sort_object_keys).collect())
        }
        scalar => scalar,
    }
}

#[cfg(test)]
mod tests {
    use super::canonicalize_tool_args;

    #[test]
    fn whitespace_variants_collapse() {
        let compact = r#"{"pattern":"**/*.rs"}"#;
        let padded = r#"{ "pattern": "**/*.rs" }"#;

        assert_eq!(
            canonicalize_tool_args(compact),
            canonicalize_tool_args(padded)
        );
    }

    #[test]
    fn object_keys_are_sorted_recursively() {
        let ordered = r#"{"a":1,"outer":{"x":1,"y":2}}"#;
        let reordered = r#"{"outer":{"y":2,"x":1},"a":1}"#;

        assert_eq!(
            canonicalize_tool_args(ordered),
            canonicalize_tool_args(reordered)
        );
    }

    #[test]
    fn array_order_remains_significant() {
        let first = r#"{"items":[1,2]}"#;
        let second = r#"{"items":[2,1]}"#;

        assert_ne!(
            canonicalize_tool_args(first),
            canonicalize_tool_args(second)
        );
    }

    #[test]
    fn malformed_arguments_pass_through_unchanged() {
        let arguments = "not even json {{{";

        assert_eq!(canonicalize_tool_args(arguments), arguments);
    }
}
