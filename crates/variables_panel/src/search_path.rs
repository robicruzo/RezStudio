//! RStudio's environment menu: the packages attached to R's search path and
//! the objects in each. Ark's variables comm only covers the global
//! environment, so these come from R code run silently.

use anyhow::{Context as _, Result, bail};
use runtime::comms::Variable;
use serde_json::Value;

pub const SEARCH_PATH_CODE: &str = "search()";

/// Lists one environment on the search path as variables records, the shape
/// Ark's variables comm uses. Functions are forced, which only loads their
/// code, but lazy-loaded data stays a promise, as in RStudio, since forcing
/// it can load a large dataset.
const LISTING_CODE: &str = r#"local({
    search_name <- SEARCH_NAME
    environment <- as.environment(search_name)
    lazy_data <- tryCatch({
        info <- getNamespaceInfo(sub("^package:", "", search_name), "lazydata")
        if (is.environment(info)) ls(info, all.names = TRUE) else character()
    }, error = function(error) character())
    one_line <- function(text) {
        text <- gsub("\\s+", " ", paste(text, collapse = " "))
        trimws(substr(text, 1, 1000))
    }
    record <- function(name, display_value, display_type, kind, size = 0, length = 0) {
        list(access_key = name, display_name = name, display_value = display_value,
            display_type = display_type, type_info = "",
            size = as.integer(min(size, .Machine$integer.max)), kind = kind,
            length = as.integer(min(length, .Machine$integer.max)),
            has_children = FALSE, has_viewer = FALSE, is_truncated = FALSE)
    }
    unname(lapply(ls(environment, sorted = TRUE), function(name) {
        if (name %in% lazy_data) {
            return(record(name, "<Promise>", "promise", "lazy"))
        }
        value <- tryCatch(get(name, envir = environment, inherits = FALSE),
            error = function(error) structure(conditionMessage(error), class = "rezstudio_error"))
        if (inherits(value, "rezstudio_error")) {
            return(record(name, one_line(unclass(value)), "error", "other"))
        }
        size <- tryCatch(as.numeric(utils::object.size(value)), error = function(error) 0)
        if (is.function(value)) {
            signature <- tryCatch(deparse(args(value)), error = function(error) NULL)
            display_value <- if (length(signature) > 1) {
                one_line(signature[-length(signature)])
            } else {
                one_line(deparse(value))
            }
            record(name, display_value, "function", "function", size, 1)
        } else if (is.data.frame(value)) {
            columns <- ncol(value)
            display_value <- sprintf("%d obs. of %d variable%s", nrow(value), columns,
                if (columns == 1) "" else "s")
            record(name, display_value, class(value)[1], "table", size, length(value))
        } else {
            display_value <- tryCatch(
                one_line(utils::capture.output(utils::str(value, give.attr = FALSE, vec.len = 4))[1]),
                error = function(error) "")
            kind <- switch(typeof(value), double = , integer = , complex = "number",
                character = "string", logical = "boolean", list = "collection", "other")
            record(name, display_value, class(value)[1], kind, size, length(value))
        }
    }))
})"#;

/// The `package:` entries of `search()`'s result, in search order.
pub fn attached_packages(result: Value) -> Result<Vec<String>> {
    let names: Vec<String> = match result {
        Value::Null => Vec::new(),
        Value::String(name) => vec![name],
        result @ Value::Array(_) => {
            serde_json::from_value(result).context("unexpected reply to `search()`")?
        }
        result => bail!("unexpected reply to `search()`: {result}"),
    };
    Ok(names
        .into_iter()
        .filter(|name| name.starts_with("package:"))
        .collect())
}

/// R code listing the objects in `search_name`, such as `package:stats`.
pub fn listing_code(search_name: &str) -> String {
    LISTING_CODE.replace("SEARCH_NAME", &runtime::r_string(search_name))
}

pub fn parse_listing(result: Value) -> Result<Vec<Variable>> {
    match result {
        // R's empty list arrives as null.
        Value::Null => Ok(Vec::new()),
        result => serde_json::from_value(result).context("unexpected listing of an environment"),
    }
}

/// A function's source, as RStudio's `View()` shows it.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct FunctionSource {
    pub source: String,
    /// The package namespace the function belongs to, empty for the global
    /// environment's own functions and primitives.
    pub namespace: String,
}

impl FunctionSource {
    /// The editor tab's title, as RStudio names it.
    pub fn title(&self, name: &str) -> String {
        match self.namespace.as_str() {
            "" | "R_GlobalEnv" => name.to_string(),
            namespace => format!("{name} (namespace:{namespace})"),
        }
    }
}

/// R code returning the source of the function `name` in `search_name`, or
/// in the global environment for `None`. Source references are kept, so
/// functions defined in the session show as written, comments included.
pub fn function_source_code(search_name: Option<&str>, name: &str) -> String {
    let environment = match search_name {
        Some(search_name) => format!("as.environment({})", runtime::r_string(search_name)),
        None => "globalenv()".to_string(),
    };
    format!(
        r#"local({{
    value <- get({name}, envir = {environment}, inherits = FALSE)
    enclosing <- environment(value)
    list(
        source = paste(deparse(value, control = "useSource"), collapse = "\n"),
        namespace = if (is.environment(enclosing)) environmentName(enclosing) else ""
    )
}})"#,
        name = runtime::r_string(name),
    )
}

pub fn parse_function_source(result: Value) -> Result<FunctionSource> {
    serde_json::from_value(result).context("unexpected reply for a function's source")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_attached_packages_keeps_package_entries_in_order() {
        let search = json!([
            ".GlobalEnv",
            "package:stats",
            "tools:rstudio",
            "Autoloads",
            "package:base"
        ]);
        assert_eq!(
            attached_packages(search).unwrap(),
            ["package:stats", "package:base"]
        );
        assert!(attached_packages(json!(".GlobalEnv")).unwrap().is_empty());
        assert!(attached_packages(json!({ "a": 1 })).is_err());
    }

    #[test]
    fn test_listing_code_quotes_the_name() {
        let code = listing_code("package:my\"pkg");
        assert!(code.contains(r#"search_name <- "package:my\"pkg""#));
        assert!(!code.contains("SEARCH_NAME"));
    }

    #[test]
    fn test_function_source() {
        let code = function_source_code(Some("package:forcats"), "as_factor");
        assert!(code.contains(
            r#"get("as_factor", envir = as.environment("package:forcats"), inherits = FALSE)"#
        ));
        assert!(
            function_source_code(None, "greet")
                .contains(r#"get("greet", envir = globalenv(), inherits = FALSE)"#)
        );

        let source = parse_function_source(json!({
            "source": "function (x, ...) \n{\n    UseMethod(\"as_factor\")\n}",
            "namespace": "forcats"
        }))
        .unwrap();
        assert_eq!(source.title("as_factor"), "as_factor (namespace:forcats)");
        let source = parse_function_source(
            json!({ "source": "function(name) 1", "namespace": "R_GlobalEnv" }),
        )
        .unwrap();
        assert_eq!(source.title("greet"), "greet");
        assert!(parse_function_source(json!("oops")).is_err());
    }

    #[test]
    fn test_parse_listing() {
        assert!(parse_listing(json!(null)).unwrap().is_empty());
        let listing = parse_listing(json!([{
            "access_key": "lm", "display_name": "lm", "display_value": "function (formula, data)",
            "display_type": "function", "type_info": "", "size": 4848, "kind": "function",
            "length": 1, "has_children": false, "has_viewer": false, "is_truncated": false
        }]))
        .unwrap();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].display_value, "function (formula, data)");
    }
}
