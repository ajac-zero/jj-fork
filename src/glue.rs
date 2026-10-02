//! Glue naming rules. A glue `glue/a+b` is a merge of the series `a` and `b` that holds only the
//! resolution of their conflicts. Names are series names without their prefix, so `a` resolves
//! to `patch/a` or `tooling/a`. A glue over a superset, such as `glue/a+b+c`, merges the glue
//! over the subset, `glue/a+b`, instead of `patch/a` and `patch/b`.

use std::collections::BTreeSet;

/// The series names a glue merges, sorted and deduplicated.
pub fn names(glue: &str, glue_prefix: &str) -> BTreeSet<String> {
    glue.strip_prefix(glue_prefix)
        .unwrap_or(glue)
        .split('+')
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolves each name a glue merges to its series bookmark.
pub fn series_of(
    glue: &str,
    glue_prefix: &str,
    prefixes: &[String],
    series: &[String],
) -> Result<Vec<String>, String> {
    let names = names(glue, glue_prefix);
    if names.len() < 2 {
        return Err(format!(
            "{glue} must name at least two series, as {glue_prefix}<a>+<b>"
        ));
    }
    let mut resolved = Vec::new();
    for name in names {
        let found: Vec<String> = prefixes
            .iter()
            .map(|p| format!("{p}{name}"))
            .filter(|candidate| series.contains(candidate))
            .collect();
        match found.as_slice() {
            [one] => resolved.push(one.clone()),
            [] => {
                return Err(format!(
                    "{glue} names {name}, but no series {} exists; delete the glue if that series was retired",
                    prefixes
                        .iter()
                        .map(|p| format!("{p}{name}"))
                        .collect::<Vec<_>>()
                        .join(" or ")
                ));
            }
            _ => {
                return Err(format!(
                    "{glue} names {name}, which is ambiguous between {}",
                    found.join(" and ")
                ));
            }
        }
    }
    Ok(resolved)
}

/// The other glues whose series are a proper subset of `glue`'s, which it must merge.
pub fn inner_glues<'a>(glue: &str, glue_prefix: &str, glues: &'a [String]) -> Vec<&'a String> {
    let outer = names(glue, glue_prefix);
    glues
        .iter()
        .filter(|other| {
            let inner = names(other, glue_prefix);
            other.as_str() != glue && inner != outer && inner.is_subset(&outer)
        })
        .collect()
}

/// The glue name that resolves a conflict between two fork parents, each a series or a glue.
pub fn suggested(a: &str, b: &str, glue_prefix: &str, prefixes: &[String]) -> String {
    let mut all = BTreeSet::new();
    for name in [a, b] {
        if name.starts_with(glue_prefix) {
            all.extend(names(name, glue_prefix));
        } else {
            let bare = prefixes
                .iter()
                .find_map(|p| name.strip_prefix(p.as_str()))
                .unwrap_or(name);
            all.insert(bare.to_string());
        }
    }
    format!(
        "{glue_prefix}{}",
        all.into_iter().collect::<Vec<_>>().join("+")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixes() -> Vec<String> {
        vec!["patch/".into(), "tooling/".into()]
    }

    #[test]
    fn names_are_sorted_and_deduplicated() {
        assert_eq!(
            names("glue/b+a+b", "glue/").into_iter().collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn series_resolve_across_prefixes() {
        let series = vec![
            "patch/a".to_string(),
            "tooling/b".to_string(),
            "patch/c".to_string(),
            "tooling/c".to_string(),
        ];
        assert_eq!(
            series_of("glue/a+b", "glue/", &prefixes(), &series).unwrap(),
            vec!["patch/a", "tooling/b"]
        );
        assert!(
            series_of("glue/a", "glue/", &prefixes(), &series)
                .unwrap_err()
                .contains("at least two")
        );
        assert!(
            series_of("glue/a+x", "glue/", &prefixes(), &series)
                .unwrap_err()
                .contains("names x, but no series")
        );
        assert!(
            series_of("glue/a+c", "glue/", &prefixes(), &series)
                .unwrap_err()
                .contains("ambiguous")
        );
    }

    #[test]
    fn a_superset_glue_merges_its_subset_glues() {
        let glues = vec![
            "glue/a+b".to_string(),
            "glue/a+b+c".to_string(),
            "glue/c+d".to_string(),
            "glue/b+a".to_string(),
        ];
        assert_eq!(
            inner_glues("glue/a+b+c", "glue/", &glues),
            vec!["glue/a+b", "glue/b+a"]
        );
        assert!(
            inner_glues("glue/a+b", "glue/", &glues).is_empty(),
            "an equal name set is not a proper subset"
        );
    }

    #[test]
    fn suggested_glue_flattens_glue_names() {
        assert_eq!(
            suggested("patch/b", "tooling/a", "glue/", &prefixes()),
            "glue/a+b"
        );
        assert_eq!(
            suggested("glue/a+b", "patch/c", "glue/", &prefixes()),
            "glue/a+b+c"
        );
    }
}
