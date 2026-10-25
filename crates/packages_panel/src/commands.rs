//! The R code the pane runs in the console, so what it does is visible and
//! can be rerun, as in RStudio.

use package_index::{IndexedPackage, PackageSource};

/// An R string literal.
pub fn r_string(text: &str) -> String {
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('"');
    for character in text.chars() {
        match character {
            '"' => literal.push_str("\\\""),
            '\\' => literal.push_str("\\\\"),
            '\n' => literal.push_str("\\n"),
            character => literal.push(character),
        }
    }
    literal.push('"');
    literal
}

/// `"a"` for one name, `c("a", "b")` for several.
fn r_strings(names: &[String]) -> String {
    match names {
        [name] => r_string(name),
        names => format!(
            "c({})",
            names
                .iter()
                .map(|name| r_string(name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub fn attach(name: &str) -> String {
    format!("library({})", name_or_string(name))
}

pub fn detach(name: &str) -> String {
    format!(
        "detach({}, unload = TRUE)",
        r_string(&format!("package:{name}"))
    )
}

/// Installs a package found in the index from where it's published. In an
/// renv project, renv installs it into the project library.
pub fn install(package: &IndexedPackage, renv: bool) -> String {
    let name = r_string(&package.name);
    match (package.source(), renv) {
        (PackageSource::Cran, false) => format!("install.packages({name})"),
        (PackageSource::Cran, true) => format!("renv::install({name})"),
        (PackageSource::Bioconductor, false) => format!(
            "if (!requireNamespace(\"BiocManager\", quietly = TRUE)) install.packages(\"BiocManager\")\nBiocManager::install({name})"
        ),
        (PackageSource::Bioconductor, true) => {
            format!(
                "renv::install({})",
                r_string(&format!("bioc::{}", package.name))
            )
        }
        (PackageSource::Universe(universe), renv) => {
            let repos = format!(
                "repos = c({}, getOption(\"repos\"))",
                r_string(&format!("https://{universe}.r-universe.dev"))
            );
            if renv {
                format!("renv::install({name}, {repos})")
            } else {
                format!("install.packages({name}, {repos})")
            }
        }
    }
}

pub fn remove(name: &str, renv: bool) -> String {
    if renv {
        format!("renv::remove({})", r_string(name))
    } else {
        format!("remove.packages({})", r_string(name))
    }
}

/// Installs the newest version of each package from the session's repositories.
pub fn update(names: &[String], renv: bool) -> String {
    if renv {
        format!("renv::update({})", r_strings(names))
    } else {
        format!("install.packages({})", r_strings(names))
    }
}

/// `library()` takes a bare name, as people type it; names that aren't valid
/// R symbols are quoted.
fn name_or_string(name: &str) -> String {
    let is_symbol = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '.');
    if is_symbol {
        name.to_string()
    } else {
        r_string(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str, universe: &str, on_cran: bool) -> IndexedPackage {
        IndexedPackage {
            name: name.into(),
            title: String::new(),
            description: String::new(),
            universe: universe.into(),
            used_by: 0,
            cran_version: on_cran.then(|| "1.0".into()),
        }
    }

    #[test]
    fn test_install_commands() {
        assert_eq!(
            install(&package("dplyr", "tidyverse", true), false),
            r#"install.packages("dplyr")"#
        );
        assert_eq!(
            install(&package("dplyr", "tidyverse", true), true),
            r#"renv::install("dplyr")"#
        );
        assert_eq!(
            install(&package("DESeq2", "bioc", false), false),
            "if (!requireNamespace(\"BiocManager\", quietly = TRUE)) install.packages(\"BiocManager\")\nBiocManager::install(\"DESeq2\")"
        );
        assert_eq!(
            install(&package("DESeq2", "bioc", false), true),
            r#"renv::install("bioc::DESeq2")"#
        );
        assert_eq!(
            install(&package("tidycode", "lucymcgowan", false), false),
            r#"install.packages("tidycode", repos = c("https://lucymcgowan.r-universe.dev", getOption("repos")))"#
        );
    }

    #[test]
    fn test_other_commands() {
        assert_eq!(attach("data.table"), "library(data.table)");
        assert_eq!(attach("2odd"), r#"library("2odd")"#);
        assert_eq!(detach("stats"), r#"detach("package:stats", unload = TRUE)"#);
        assert_eq!(remove("cli", false), r#"remove.packages("cli")"#);
        assert_eq!(remove("cli", true), r#"renv::remove("cli")"#);
        assert_eq!(update(&["cli".into()], false), r#"install.packages("cli")"#);
        assert_eq!(
            update(&["cli".into(), "rlang".into()], true),
            r#"renv::update(c("cli", "rlang"))"#
        );
        assert_eq!(r_string("a\"b\\c"), r#""a\"b\\c""#);
    }
}
