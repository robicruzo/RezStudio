//! Ranking packages for a query. Name matches come first, from exact to
//! fuzzy, then packages whose title or description has every word; within
//! each, the most used package wins, as people usually want the popular one.

use crate::IndexedPackage;
use std::cmp::Reverse;

/// A package with the lowercase text search compares against.
pub(crate) struct Entry {
    pub package: IndexedPackage,
    /// Without punctuation, so "data table" finds `data.table` by name.
    name: String,
    title: String,
    description: String,
}

impl Entry {
    pub fn new(package: IndexedPackage) -> Self {
        Self {
            name: name_key(&package.name),
            title: package.title.to_lowercase(),
            description: package.description.to_lowercase(),
            package,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Tier {
    ExactName,
    NamePrefix,
    NameContains,
    /// The query's letters appear in the name in order, as with a typo or an
    /// abbreviation. Ranked by how tightly they cluster.
    FuzzyName {
        span: usize,
    },
    Title,
    Description,
}

/// The indices of the `limit` best matches, best first.
pub(crate) fn search(entries: &[Entry], query: &str, limit: usize) -> Vec<usize> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() || limit == 0 {
        return Vec::new();
    }
    // Names have no spaces, so "data table" looks for "datatable" in names.
    let name_query = name_key(&words.concat());
    let mut matches: Vec<(Tier, Reverse<u32>, usize, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let tier = rank(entry, &name_query, &words)?;
            Some((
                tier,
                Reverse(entry.package.used_by),
                entry.name.len(),
                index,
            ))
        })
        .collect();
    if matches.len() > limit {
        matches.select_nth_unstable(limit - 1);
        matches.truncate(limit);
    }
    matches.sort_unstable();
    matches.into_iter().map(|(.., index)| index).collect()
}

/// A name in lowercase without its dots and other punctuation.
fn name_key(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn rank(entry: &Entry, name_query: &str, words: &[String]) -> Option<Tier> {
    if name_query.is_empty() {
        return None;
    }
    if entry.name == name_query {
        return Some(Tier::ExactName);
    }
    if entry.name.starts_with(name_query) {
        return Some(Tier::NamePrefix);
    }
    if entry.name.contains(name_query) {
        return Some(Tier::NameContains);
    }
    // A single letter would match most names; it only counts as a prefix.
    if name_query.chars().count() >= 3
        && let Some(span) = subsequence_span(&entry.name, name_query)
    {
        return Some(Tier::FuzzyName { span });
    }
    if words.iter().all(|word| entry.title.contains(word.as_str())) {
        return Some(Tier::Title);
    }
    if words.iter().all(|word| {
        entry.title.contains(word.as_str()) || entry.description.contains(word.as_str())
    }) {
        return Some(Tier::Description);
    }
    None
}

/// How many characters of `text` it takes to find `query`'s characters in
/// order, starting at the earliest place it can; `None` if they aren't there.
fn subsequence_span(text: &str, query: &str) -> Option<usize> {
    let mut query_chars = query.chars().peekable();
    let mut start = None;
    for (position, character) in text.chars().enumerate() {
        if query_chars.peek() == Some(&character) {
            start.get_or_insert(position);
            query_chars.next();
            if query_chars.peek().is_none() {
                return Some(position + 1 - start.unwrap_or(position));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, title: &str, used_by: u32) -> Entry {
        Entry::new(IndexedPackage {
            name: name.to_string(),
            title: title.to_string(),
            description: format!("About {name}."),
            universe: "cran".to_string(),
            used_by,
            cran_version: Some("1.0".to_string()),
        })
    }

    fn names(entries: &[Entry], query: &str) -> Vec<String> {
        search(entries, query, 10)
            .into_iter()
            .map(|index| entries[index].package.name.clone())
            .collect()
    }

    #[test]
    fn test_ranking() {
        let entries = vec![
            entry("ggplotify", "Convert Plot to 'grob' or 'ggplot' Object", 40),
            entry(
                "ggplot2",
                "Create Elegant Data Visualisations Using the Grammar of Graphics",
                9000,
            ),
            entry(
                "plotly",
                "Create Interactive Web Graphics via 'plotly.js'",
                1500,
            ),
            entry("ggpubr", "'ggplot2' Based Publication Ready Plots", 800),
            entry("data.table", "Extension of `data.frame`", 3000),
            entry("rqdatatable", "'rquery' for 'data.table'", 9500),
            entry("dplyr", "A Grammar of Data Manipulation", 9859),
        ];
        // Prefix matches, most used first, then a title that mentions it.
        assert_eq!(names(&entries, "ggpl"), ["ggplot2", "ggplotify", "ggpubr"]);
        // An exact name beats more popular partial matches.
        assert_eq!(names(&entries, "PLOTLY")[0], "plotly");
        // Letters in order find a name despite a typo, then titles follow.
        assert_eq!(names(&entries, "ggplt"), ["ggplot2", "ggplotify"]);
        // Punctuation in names doesn't count, so `data.table` is an exact match.
        assert_eq!(names(&entries, "data table")[0], "data.table");
        assert_eq!(names(&entries, "data.table")[0], "data.table");
        // Words anywhere in the title, in any order.
        assert_eq!(names(&entries, "grammar data"), ["dplyr", "ggplot2"]);
        // Then the description.
        assert_eq!(names(&entries, "about ggpubr"), ["ggpubr"]);
        assert!(names(&entries, "  ").is_empty());
        assert!(names(&entries, "zzzz").is_empty());
        // A single letter only matches name prefixes and words.
        assert_eq!(names(&entries, "g")[0], "ggplot2");
    }

    #[test]
    fn test_limit_keeps_the_best() {
        let entries: Vec<Entry> = (0..100)
            .map(|index| entry(&format!("pkg{index}"), "", index))
            .collect();
        let top: Vec<usize> = search(&entries, "pkg", 3);
        assert_eq!(top, [99, 98, 97]);
    }

    #[test]
    fn test_subsequence_span() {
        assert_eq!(subsequence_span("ggplot2", "ggplt"), Some(6));
        assert_eq!(subsequence_span("abc", "abd"), None);
        assert_eq!(subsequence_span("xaxbxc", "abc"), Some(5));
    }
}
