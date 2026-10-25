//! The lines of the Packages pane: installed packages matching the filter,
//! then packages from the index that aren't installed.

use collections::HashMap;
use package_index::IndexedPackage;
use runtime::comms::{InstalledPackage, PackageDetail};

#[derive(Clone, Debug, PartialEq)]
pub enum DetailState {
    Loading,
    Loaded(PackageDetail),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Heading(String),
    Installed {
        package: InstalledPackage,
        /// A newer version in the session's repositories.
        latest: Option<String>,
        expanded: bool,
    },
    /// One fact about an expanded package.
    Detail {
        label: &'static str,
        value: String,
        url: Option<String>,
    },
    Available(IndexedPackage),
    Note(String),
}

pub struct RowInputs<'a> {
    pub installed: &'a [InstalledPackage],
    pub outdated: &'a HashMap<String, String>,
    pub filter: &'a str,
    /// Search results for `filter`, best first.
    pub results: &'a [IndexedPackage],
    pub searching: bool,
    pub expanded: Option<&'a str>,
    pub details: &'a HashMap<String, DetailState>,
}

pub fn build_rows(inputs: RowInputs) -> Vec<Row> {
    let filter = inputs.filter.trim().to_lowercase();
    let mut installed: Vec<&InstalledPackage> = inputs
        .installed
        .iter()
        .filter(|package| {
            filter.is_empty()
                || package.name.to_lowercase().contains(&filter)
                || package.description.to_lowercase().contains(&filter)
        })
        .collect();
    installed.sort_by_cached_key(|package| package.name.to_lowercase());

    let mut rows = Vec::new();
    if !filter.is_empty() {
        rows.push(Row::Heading(format!("Installed ({})", installed.len())));
    }
    for package in installed {
        let expanded = inputs.expanded == Some(package.name.as_str());
        rows.push(Row::Installed {
            package: package.clone(),
            latest: inputs.outdated.get(&package.name).cloned(),
            expanded,
        });
        if expanded {
            push_detail(&mut rows, package, inputs.details.get(&package.name));
        }
    }
    if filter.is_empty() {
        return rows;
    }

    rows.push(Row::Heading("Available".to_string()));
    let installed_names: collections::HashSet<&str> = inputs
        .installed
        .iter()
        .map(|package| package.name.as_str())
        .collect();
    let available: Vec<&IndexedPackage> = inputs
        .results
        .iter()
        .filter(|package| !installed_names.contains(package.name.as_str()))
        .collect();
    if available.is_empty() {
        rows.push(Row::Note(if inputs.searching {
            "Searching…".to_string()
        } else {
            "No other packages match.".to_string()
        }));
    }
    rows.extend(available.into_iter().cloned().map(Row::Available));
    rows
}

fn push_detail(rows: &mut Vec<Row>, package: &InstalledPackage, detail: Option<&DetailState>) {
    let mut push = |label, value: Option<String>, url: Option<String>| {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            rows.push(Row::Detail { label, value, url });
        }
    };
    match detail {
        None | Some(DetailState::Loading) => push("", Some("Loading…".to_string()), None),
        Some(DetailState::Failed(error)) => push("", Some(error.clone()), None),
        Some(DetailState::Loaded(detail)) => {
            push("Title", detail.title.clone(), None);
            push("Description", Some(package.description.clone()), None);
            push("Maintainer", detail.author.clone(), None);
            push("License", detail.license.clone(), None);
            push("From", detail.source_repository.clone(), None);
            push("Published", detail.published_date.clone(), None);
            push("Website", package.url.clone(), package.url.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(name: &str, description: &str) -> InstalledPackage {
        InstalledPackage {
            name: name.into(),
            version: "1.0".into(),
            attached: false,
            description: description.into(),
            url: None,
        }
    }

    fn available(name: &str) -> IndexedPackage {
        IndexedPackage {
            name: name.into(),
            title: format!("About {name}"),
            description: String::new(),
            universe: "cran".into(),
            used_by: 0,
            cran_version: Some("1.0".into()),
        }
    }

    fn labels(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Heading(text) => format!("# {text}"),
                Row::Installed {
                    package,
                    latest,
                    expanded,
                } => format!(
                    "{}{}{}",
                    package.name,
                    latest
                        .as_ref()
                        .map(|latest| format!(" -> {latest}"))
                        .unwrap_or_default(),
                    if *expanded { " v" } else { "" }
                ),
                Row::Detail { label, value, .. } => format!("  {label}: {value}"),
                Row::Available(package) => format!("+ {}", package.name),
                Row::Note(text) => format!("({text})"),
            })
            .collect()
    }

    #[test]
    fn test_rows() {
        let packages = vec![
            installed("stats", "R statistical functions"),
            installed("ggplot2", "A system for declaratively creating graphics"),
            installed("cli", "Helpers for command line interfaces"),
        ];
        let outdated = HashMap::from_iter([("cli".to_string(), "3.7.0".to_string())]);
        let details = HashMap::from_iter([(
            "cli".to_string(),
            DetailState::Loaded(PackageDetail {
                name: "cli".into(),
                title: Some("Helpers for Developing Command Line Interfaces".into()),
                license: Some("MIT".into()),
                ..Default::default()
            }),
        )]);
        let results = vec![available("ggplot2"), available("ggplotify")];
        let rows =
            |filter: &str, expanded: Option<&str>, results: &[IndexedPackage], searching: bool| {
                labels(&build_rows(RowInputs {
                    installed: &packages,
                    outdated: &outdated,
                    filter,
                    results,
                    searching,
                    expanded,
                    details: &details,
                }))
            };

        // Sorted by name, with updates marked and the expanded one's details.
        assert_eq!(
            rows("", Some("cli"), &[], false),
            [
                "cli -> 3.7.0 v",
                "  Title: Helpers for Developing Command Line Interfaces",
                "  Description: Helpers for command line interfaces",
                "  License: MIT",
                "ggplot2",
                "stats",
            ]
        );
        // A filter matches names and descriptions, then searches what isn't installed.
        assert_eq!(
            rows("gg", None, &results, false),
            ["# Installed (1)", "ggplot2", "# Available", "+ ggplotify"]
        );
        assert_eq!(
            rows("graphics", None, &[], true),
            ["# Installed (1)", "ggplot2", "# Available", "(Searching…)"]
        );
        assert_eq!(
            rows("zzz", None, &[], false),
            [
                "# Installed (0)",
                "# Available",
                "(No other packages match.)"
            ]
        );
        assert_eq!(
            rows("", Some("stats"), &[], false)[2..],
            ["stats v", "  : Loading…"]
        );
    }
}
