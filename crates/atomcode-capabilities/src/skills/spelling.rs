//! Conservative suggestions only: never resolution, execution or authorization.

/// The first token not resolved as a skill in an invocation's leading run.
#[derive(Debug, PartialEq, Eq)]
pub struct Boundary<'a> {
    pub word: &'a str,
    pub explicit: bool,
    pub after_skill: bool,
}

pub fn invocation_boundary(args: &str, known: impl Fn(&str) -> bool) -> Option<Boundary<'_>> {
    let mut after_skill = false;
    for token in args.split_whitespace() {
        let explicit = token.strip_prefix('$').or_else(|| token.strip_prefix('¥'));
        let word = explicit.unwrap_or(token);
        if !known(word) {
            return Some(Boundary {
                word,
                explicit: explicit.is_some(),
                after_skill,
            });
        }
        after_skill = true;
    }
    None
}

fn normalized(word: &str) -> String {
    word.chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

// One insertion, deletion, substitution, or adjacent transposition. No quadratic
// distance matrix, and never a broad fuzzy-subsequence match on task prose.
fn one_edit(a: &[char], b: &[char]) -> bool {
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let first = a
        .iter()
        .zip(b)
        .position(|(a, b)| a != b)
        .unwrap_or(a.len().min(b.len()));
    if a.len() == b.len() {
        a.get(first + 1..) == b.get(first + 1..)
            || (first + 1 < a.len()
                && a[first] == b[first + 1]
                && a[first + 1] == b[first]
                && a[first + 2..] == b[first + 2..])
    } else if a.len() < b.len() {
        a[first..] == b[first + 1..]
    } else {
        a[first + 1..] == b[first..]
    }
}

/// At most three canonical installed names. Qualified tokens never match a
/// different namespace; bare tokens may suggest several qualified alternatives.
pub fn suggestions<'a>(word: &str, names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    if word.chars().count() > 64
        || word.is_empty()
        || !word
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':'))
    {
        return Vec::new();
    }
    let typed = normalized(word);
    let a: Vec<_> = typed.chars().collect();
    if a.len() < 3 {
        return Vec::new();
    }
    let mut ranked = Vec::new();
    for name in names {
        if name.len() > 256 {
            continue;
        }
        let target = if let Some((namespace, _)) = word.rsplit_once(':') {
            let Some((candidate_namespace, _)) = name.rsplit_once(':') else {
                continue;
            };
            if namespace != candidate_namespace {
                continue;
            }
            name
        } else {
            name.rsplit(':').next().unwrap_or(name)
        };
        let b: Vec<_> = normalized(target).chars().collect();
        let score = if a == b {
            0
        } else if a.len() >= 4 && one_edit(&a, &b) {
            1
        } else {
            continue;
        };
        ranked.push((score, name.contains(':'), name.to_string()));
    }
    ranked.sort();
    ranked.dedup();
    ranked
        .into_iter()
        .take(3)
        .map(|(_, _, name)| name)
        .collect()
}

/// Unmarked task prose gets only a soft, conservative hint at its first word.
pub fn task_candidates<'a>(
    boundary: &Boundary<'_>,
    names: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    if boundary.explicit || !boundary.after_skill || boundary.word.chars().count() < 5 {
        return Vec::new();
    }
    suggestions(boundary.word, names)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn suggestions_are_bounded_ranked_and_namespace_aware() {
        let names = [
            "review",
            "review-code",
            "a:review",
            "b:review",
            "c:review",
            "plan",
        ];
        assert_eq!(
            suggestions("reveiw", names),
            ["review", "a:review", "b:review"]
        );
        assert_eq!(suggestions("a:reveiw", names), ["a:review"]);
        assert!(suggestions("other:reveiw", names).is_empty());
        assert_eq!(suggestions("REVIEW_code", names), ["review-code"]);
        for word in ["reviw", "revieew", "reviex"] {
            assert_eq!(suggestions(word, ["review"]), ["review"]);
        }
        for word in ["re", "./review", "review,", "$review", "unrelated"] {
            assert!(suggestions(word, ["review"]).is_empty());
        }
        assert!(suggestions(&"x".repeat(65), ["review"]).is_empty());
    }
    #[test]
    fn only_the_first_unresolved_prefix_word_is_considered() {
        let known = |name: &str| matches!(name, "plan" | "review");
        let boundary = invocation_boundary("plan reveiw  fix $missing", known).unwrap();
        assert_eq!(
            boundary,
            Boundary {
                word: "reveiw",
                explicit: false,
                after_skill: true
            }
        );
        assert_eq!(task_candidates(&boundary, ["review"]), ["review"]);
        let boundary = invocation_boundary("plan fix reveiw", known).unwrap();
        assert!(task_candidates(&boundary, ["fixx", "review"]).is_empty());
        let boundary = invocation_boundary("plan $reveiw task", known).unwrap();
        assert!(boundary.explicit);
        assert!(task_candidates(&boundary, ["review"]).is_empty());
        assert!(invocation_boundary("plan $review", known).is_none());
    }
}
