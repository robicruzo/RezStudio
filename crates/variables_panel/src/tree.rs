//! The variables shown in the Environment pane, as a tree that Ark fills in
//! lazily: the session's variables arrive whole, children when expanded.

use collections::{HashMap, HashSet};
use runtime::comms::Variable;

/// A variable's position: the access keys from a top-level variable down.
pub type VariablePath = Vec<String>;

#[derive(Default)]
pub struct VariableTree {
    variables: Vec<Variable>,
    children: HashMap<VariablePath, Children>,
    expanded: HashSet<VariablePath>,
}

enum Children {
    Loading,
    Loaded {
        variables: Vec<Variable>,
        /// Can exceed `variables.len()` when Ark truncates the list.
        length: i64,
    },
    Failed(String),
}

/// One line of the pane.
#[derive(Clone, Debug, PartialEq)]
pub enum Row {
    Variable {
        path: VariablePath,
        depth: usize,
        variable: Variable,
        expanded: bool,
    },
    Loading {
        depth: usize,
    },
    Error {
        depth: usize,
        message: String,
    },
    /// Children Ark left out of a long list.
    More {
        depth: usize,
        count: i64,
    },
}

impl VariableTree {
    pub fn is_empty(&self) -> bool {
        self.variables.is_empty()
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Replaces every variable. Expanded variables stay expanded, so their
    /// children must be fetched again; returns their paths.
    pub fn refresh(&mut self, variables: Vec<Variable>) -> Vec<VariablePath> {
        self.variables = variables;
        sort_by_name(&mut self.variables);
        self.children.clear();
        let existing: HashSet<&String> = self
            .variables
            .iter()
            .map(|variable| &variable.access_key)
            .collect();
        self.expanded
            .retain(|path| path.first().is_some_and(|key| existing.contains(key)));
        self.refetch_expanded()
    }

    /// Applies an update after R ran code. Expanded variables that changed
    /// need their children fetched again; returns their paths.
    pub fn update(&mut self, assigned: Vec<Variable>, removed: &[String]) -> Vec<VariablePath> {
        let changed: HashSet<String> = assigned
            .iter()
            .map(|variable| variable.access_key.clone())
            .chain(removed.iter().cloned())
            .collect();
        self.variables
            .retain(|variable| !changed.contains(&variable.access_key));
        self.variables.extend(assigned);
        sort_by_name(&mut self.variables);

        let is_stale = |path: &VariablePath| path.first().is_some_and(|key| changed.contains(key));
        self.children.retain(|path, _| !is_stale(path));
        let removed: HashSet<&String> = removed.iter().collect();
        self.expanded
            .retain(|path| path.first().is_none_or(|key| !removed.contains(key)));
        self.expanded
            .iter()
            .filter(|path| is_stale(path))
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .inspect(|path| {
                self.children.insert(path.clone(), Children::Loading);
            })
            .collect()
    }

    /// Expands or collapses `path`. Returns whether its children need fetching.
    pub fn toggle(&mut self, path: &VariablePath) -> bool {
        if self.expanded.remove(path) {
            return false;
        }
        self.expanded.insert(path.clone());
        if self.children.contains_key(path) {
            return false;
        }
        self.children.insert(path.clone(), Children::Loading);
        true
    }

    pub fn set_children(&mut self, path: &VariablePath, mut variables: Vec<Variable>, length: i64) {
        // Children keep R's order (list elements, columns), unlike the top level.
        variables.truncate(usize::try_from(length).unwrap_or(usize::MAX));
        self.children
            .insert(path.clone(), Children::Loaded { variables, length });
    }

    pub fn set_children_error(&mut self, path: &VariablePath, message: String) {
        self.children
            .insert(path.clone(), Children::Failed(message));
    }

    /// The lines to show. With a filter, only top-level variables whose names
    /// contain it (ignoring case) are listed, with their expanded children.
    pub fn rows(&self, filter: &str) -> Vec<Row> {
        let filter = filter.trim().to_lowercase();
        let mut rows = Vec::new();
        for variable in &self.variables {
            if !filter.is_empty() && !variable.display_name.to_lowercase().contains(&filter) {
                continue;
            }
            self.push_rows(vec![variable.access_key.clone()], variable, 0, &mut rows);
        }
        rows
    }

    fn push_rows(
        &self,
        path: VariablePath,
        variable: &Variable,
        depth: usize,
        rows: &mut Vec<Row>,
    ) {
        let expanded = variable.has_children && self.expanded.contains(&path);
        rows.push(Row::Variable {
            path: path.clone(),
            depth,
            variable: variable.clone(),
            expanded,
        });
        if !expanded {
            return;
        }
        match self.children.get(&path) {
            None | Some(Children::Loading) => rows.push(Row::Loading { depth: depth + 1 }),
            Some(Children::Failed(message)) => rows.push(Row::Error {
                depth: depth + 1,
                message: message.clone(),
            }),
            Some(Children::Loaded { variables, length }) => {
                for child in variables {
                    let mut child_path = path.clone();
                    child_path.push(child.access_key.clone());
                    self.push_rows(child_path, child, depth + 1, rows);
                }
                let missing = length - variables.len() as i64;
                if missing > 0 {
                    rows.push(Row::More {
                        depth: depth + 1,
                        count: missing,
                    });
                }
            }
        }
    }

    fn refetch_expanded(&mut self) -> Vec<VariablePath> {
        let paths: Vec<VariablePath> = self.expanded.iter().cloned().collect();
        for path in &paths {
            self.children.insert(path.clone(), Children::Loading);
        }
        paths
    }
}

fn sort_by_name(variables: &mut [Variable]) {
    variables.sort_by_cached_key(|variable| variable.display_name.to_lowercase());
}

/// A size in bytes the way RStudio shows it.
pub fn format_size(bytes: i64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = UNITS[0];
    for next in &UNITS[1..] {
        if size < 1024.0 {
            break;
        }
        size /= 1024.0;
        unit = next;
    }
    format!("{size:.1} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::comms::VariableKind;

    pub(crate) fn variable(name: &str, has_children: bool) -> Variable {
        Variable {
            access_key: name.to_string(),
            display_name: name.to_string(),
            display_value: format!("value of {name}"),
            display_type: "list".to_string(),
            type_info: String::new(),
            size: 56,
            kind: if has_children {
                VariableKind::Map
            } else {
                VariableKind::Number
            },
            length: 1,
            has_children,
            has_viewer: false,
            is_truncated: false,
            updated_time: 0,
        }
    }

    fn names(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Variable {
                    variable, depth, ..
                } => {
                    format!("{}{}", "  ".repeat(*depth), variable.display_name)
                }
                Row::Loading { depth } => format!("{}…", "  ".repeat(*depth)),
                Row::Error { depth, message } => format!("{}! {message}", "  ".repeat(*depth)),
                Row::More { depth, count } => format!("{}+{count}", "  ".repeat(*depth)),
            })
            .collect()
    }

    #[test]
    fn test_refresh_sorts_and_update_merges() {
        let mut tree = VariableTree::default();
        tree.refresh(vec![
            variable("zeta", false),
            variable("Alpha", false),
            variable("beta", false),
        ]);
        assert_eq!(names(&tree.rows("")), ["Alpha", "beta", "zeta"]);

        let mut changed = variable("beta", false);
        changed.display_value = "new".to_string();
        tree.update(
            vec![changed, variable("gamma", false)],
            &["zeta".to_string()],
        );
        let rows = tree.rows("");
        assert_eq!(names(&rows), ["Alpha", "beta", "gamma"]);
        assert!(
            matches!(&rows[1], Row::Variable { variable, .. } if variable.display_value == "new")
        );
    }

    #[test]
    fn test_expanding_fetches_children_once() {
        let mut tree = VariableTree::default();
        tree.refresh(vec![variable("df", true)]);
        let path = vec!["df".to_string()];

        assert!(tree.toggle(&path), "first expansion fetches");
        assert_eq!(names(&tree.rows("")), ["df", "  …"]);
        tree.set_children(
            &path,
            vec![variable("mpg", false), variable("cyl", false)],
            11,
        );
        assert_eq!(names(&tree.rows("")), ["df", "  mpg", "  cyl", "  +9"]);

        assert!(!tree.toggle(&path), "collapsing doesn't fetch");
        assert_eq!(names(&tree.rows("")), ["df"]);
        assert!(!tree.toggle(&path), "children are kept while collapsed");
        assert_eq!(names(&tree.rows("")).len(), 4);
    }

    #[test]
    fn test_changed_variables_refetch_expanded_children() {
        let mut tree = VariableTree::default();
        tree.refresh(vec![variable("df", true), variable("other", true)]);
        let df = vec!["df".to_string()];
        let other = vec!["other".to_string()];
        tree.toggle(&df);
        tree.set_children(&df, vec![variable("a", false)], 1);
        tree.toggle(&other);
        tree.set_children(&other, vec![variable("b", false)], 1);

        let refetch = tree.update(vec![variable("df", true)], &[]);
        assert_eq!(refetch, std::slice::from_ref(&df));
        assert_eq!(names(&tree.rows("")), ["df", "  …", "other", "  b"]);

        // A removed variable forgets that it was expanded.
        let refetch = tree.update(Vec::new(), &["df".to_string()]);
        assert!(refetch.is_empty());
        tree.update(vec![variable("df", true)], &[]);
        assert_eq!(names(&tree.rows("")), ["df", "other", "  b"]);
    }

    #[test]
    fn test_filter_and_errors() {
        let mut tree = VariableTree::default();
        tree.refresh(vec![variable("model_fit", true), variable("data", false)]);
        let path = vec!["model_fit".to_string()];
        tree.toggle(&path);
        tree.set_children_error(&path, "object not found".to_string());
        assert_eq!(
            names(&tree.rows("FIT")),
            ["model_fit", "  ! object not found"]
        );
        assert_eq!(names(&tree.rows("dat")), ["data"]);
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(848), "848 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(80_000_000), "76.3 MB");
    }
}
