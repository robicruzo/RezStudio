//! What rendering runs, and what its output says. `.qmd` files render with
//! `quarto preview`, which serves the result; `.Rmd` files with
//! `rmarkdown::render()` in `Rscript`, which writes a file.

use anyhow::{Context as _, Result};
use collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentKind {
    Quarto,
    RMarkdown,
}

impl DocumentKind {
    pub fn of(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "qmd" => Some(DocumentKind::Quarto),
            "rmd" | "rmarkdown" => Some(DocumentKind::RMarkdown),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub directory: PathBuf,
}

/// Where Quarto's installers and RStudio put it, for when it isn't on the
/// project's `PATH`.
const QUARTO_LOCATIONS: [&str; 5] = [
    "/Applications/quarto/bin/quarto",
    "/usr/local/bin/quarto",
    "/opt/homebrew/bin/quarto",
    "/Applications/RStudio.app/Contents/Resources/app/quarto/bin/quarto",
    "/Applications/Positron.app/Contents/Resources/app/quarto/bin/quarto",
];

pub fn find_on_path(name: &str, env: &HashMap<String, String>) -> Option<PathBuf> {
    env.get("PATH")?
        .split(':')
        .filter(|directory| !directory.is_empty())
        .map(|directory| Path::new(directory).join(name))
        .find(|path| path.is_file())
}

pub fn find_quarto(env: &HashMap<String, String>, home: &Path) -> Option<PathBuf> {
    find_on_path("quarto", env).or_else(|| {
        QUARTO_LOCATIONS
            .iter()
            .map(PathBuf::from)
            .chain([home.join("Applications/quarto/bin/quarto")])
            .find(|path| path.is_file())
    })
}

/// The folder holding the pandoc bundled with Quarto, which R Markdown can
/// use when pandoc isn't installed on its own.
fn quarto_pandoc_directory(quarto: &Path) -> Option<PathBuf> {
    let quarto = quarto.canonicalize().ok()?;
    let tools = quarto.parent()?.join("tools");
    [tools.join(std::env::consts::ARCH), tools]
        .into_iter()
        .find(|directory| directory.join("pandoc").is_file())
}

/// The command rendering `document`. `r_home` is the R the project uses,
/// when there is one.
pub fn render_command(
    document: &Path,
    env: &HashMap<String, String>,
    r_home: Option<&Path>,
    home: &Path,
) -> Result<RenderCommand> {
    let kind = DocumentKind::of(document)
        .with_context(|| format!("{} isn't a Quarto or R Markdown file", document.display()))?;
    let directory = document
        .parent()
        .context("the document has no folder")?
        .to_path_buf();
    let file_name = document
        .file_name()
        .context("the document has no name")?
        .to_string_lossy()
        .into_owned();
    let mut env = env.clone();
    // Escape codes would show up in the Render tab as text.
    env.insert("NO_COLOR".to_string(), "1".to_string());
    let quarto = find_quarto(&env, home);
    match kind {
        DocumentKind::Quarto => {
            let quarto = quarto.context(
                "Quarto isn't installed. Install it from quarto.org to render .qmd files.",
            )?;
            if let Some(r_home) = r_home {
                env.insert(
                    "QUARTO_R".to_string(),
                    r_home.join("bin").to_string_lossy().into_owned(),
                );
            }
            Ok(RenderCommand {
                program: quarto,
                args: vec![
                    "preview".to_string(),
                    file_name,
                    "--no-browser".to_string(),
                    "--no-watch-inputs".to_string(),
                ],
                env,
                directory,
            })
        }
        DocumentKind::RMarkdown => {
            let rscript = match r_home {
                Some(r_home) => r_home.join("bin").join("Rscript"),
                None => find_on_path("Rscript", &env)
                    .context("R isn't installed, so R Markdown can't be rendered.")?,
            };
            if !env.contains_key("RSTUDIO_PANDOC")
                && find_on_path("pandoc", &env).is_none()
                && let Some(pandoc) = quarto.as_deref().and_then(quarto_pandoc_directory)
            {
                env.insert(
                    "RSTUDIO_PANDOC".to_string(),
                    pandoc.to_string_lossy().into_owned(),
                );
            }
            Ok(RenderCommand {
                program: rscript,
                args: vec![
                    "-e".to_string(),
                    format!(
                        "rmarkdown::render({}, encoding = \"UTF-8\")",
                        runtime::r_string(&file_name)
                    ),
                ],
                env,
                directory,
            })
        }
    }
}

/// What a line of rendering's output says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputLine {
    /// `quarto preview` serves the result at this address.
    Browse(String),
    /// The renderer wrote this file, relative to the document's folder.
    Created(String),
    Other,
}

pub fn parse_line(line: &str) -> OutputLine {
    let line = line.trim();
    if let Some(url) = line.strip_prefix("Browse at ") {
        if let Some(url) = url.split_whitespace().next() {
            return OutputLine::Browse(url.to_string());
        }
    }
    if let Some(file) = line.strip_prefix("Output created: ") {
        return OutputLine::Created(file.trim().to_string());
    }
    OutputLine::Other
}

/// Drops terminal escape sequences, such as colors, from a line of output.
pub fn strip_escapes(line: &str) -> String {
    let mut text = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            text.push(character);
            continue;
        }
        if characters.peek() == Some(&'[') {
            characters.next();
            // Parameters, then one final letter.
            for character in characters.by_ref() {
                if character.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    text
}

/// Whether the Viewer can show a rendered result: HTML pages, PDFs, and
/// addresses without an extension. Word, PowerPoint and the like open in
/// their own apps.
pub fn shows_in_viewer(path_or_url: &str) -> bool {
    let path = path_or_url
        .split(['?', '#'])
        .next()
        .unwrap_or(path_or_url)
        .trim_end_matches('/');
    let file_name = path.rsplit('/').next().unwrap_or(path);
    match file_name.rsplit_once('.') {
        None => true,
        Some((_, extension)) => matches!(
            extension.to_ascii_lowercase().as_str(),
            "html" | "htm" | "pdf"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
    }

    #[test]
    fn test_document_kind() {
        assert_eq!(
            DocumentKind::of(Path::new("/a/report.qmd")),
            Some(DocumentKind::Quarto)
        );
        assert_eq!(
            DocumentKind::of(Path::new("/a/notes.Rmd")),
            Some(DocumentKind::RMarkdown)
        );
        assert_eq!(DocumentKind::of(Path::new("/a/notes.md")), None);
        assert_eq!(DocumentKind::of(Path::new("/a/Makefile")), None);
    }

    #[test]
    fn test_quarto_command() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        executable(&bin.join("quarto"));
        let env: HashMap<String, String> = [("PATH".to_string(), bin.to_string_lossy().into())]
            .into_iter()
            .collect();
        let command = render_command(
            Path::new("/work/report.qmd"),
            &env,
            Some(Path::new("/opt/R/lib/R")),
            temp.path(),
        )
        .unwrap();
        assert_eq!(command.program, bin.join("quarto"));
        assert_eq!(
            command.args,
            ["preview", "report.qmd", "--no-browser", "--no-watch-inputs"]
        );
        assert_eq!(command.directory, Path::new("/work"));
        assert_eq!(command.env["QUARTO_R"], "/opt/R/lib/R/bin");
        assert_eq!(command.env["NO_COLOR"], "1");
    }

    #[test]
    fn test_r_markdown_command_uses_quartos_pandoc_when_needed() {
        let temp = tempfile::tempdir().unwrap();
        let quarto = temp.path().join("quarto/bin/quarto");
        executable(&quarto);
        let pandoc_directory = temp
            .path()
            .join("quarto/bin/tools")
            .join(std::env::consts::ARCH);
        executable(&pandoc_directory.join("pandoc"));
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&quarto, bin.join("quarto")).unwrap();
        let env: HashMap<String, String> = [("PATH".to_string(), bin.to_string_lossy().into())]
            .into_iter()
            .collect();

        let command = render_command(
            Path::new("/work/my \"notes\".Rmd"),
            &env,
            Some(Path::new("/opt/R/lib/R")),
            temp.path(),
        )
        .unwrap();
        assert_eq!(command.program, Path::new("/opt/R/lib/R/bin/Rscript"));
        assert_eq!(
            command.args,
            [
                "-e",
                "rmarkdown::render(\"my \\\"notes\\\".Rmd\", encoding = \"UTF-8\")"
            ]
        );
        assert_eq!(
            Path::new(&command.env["RSTUDIO_PANDOC"])
                .canonicalize()
                .unwrap(),
            pandoc_directory.canonicalize().unwrap()
        );

        // Without the project's R, Rscript comes from the path.
        executable(&bin.join("pandoc"));
        let command = render_command(Path::new("/work/notes.Rmd"), &env, None, temp.path());
        assert!(command.is_err(), "no Rscript on the path and no R");
        executable(&bin.join("Rscript"));
        let command =
            render_command(Path::new("/work/notes.Rmd"), &env, None, temp.path()).unwrap();
        assert_eq!(command.program, bin.join("Rscript"));
        // pandoc on the path is used as it is.
        assert!(!command.env.contains_key("RSTUDIO_PANDOC"));
    }

    #[test]
    fn test_missing_quarto_says_so() {
        let temp = tempfile::tempdir().unwrap();
        let env = HashMap::default();
        // Only checks the message when no Quarto is installed here either.
        if find_quarto(&env, temp.path()).is_none() {
            let error = render_command(Path::new("/work/a.qmd"), &env, None, temp.path())
                .unwrap_err()
                .to_string();
            assert!(error.contains("quarto.org"), "{error}");
        }
        assert!(render_command(Path::new("/work/a.md"), &env, None, temp.path()).is_err());
    }

    #[test]
    fn test_output_lines() {
        assert_eq!(
            parse_line(&strip_escapes(
                "\u{1b}[32mBrowse at \u{1b}[39m\u{1b}[4m\u{1b}[32mhttp://localhost:6437/report.html\u{1b}[39m\u{1b}[24m"
            )),
            OutputLine::Browse("http://localhost:6437/report.html".to_string())
        );
        assert_eq!(
            parse_line("Output created: notes.html"),
            OutputLine::Created("notes.html".to_string())
        );
        assert_eq!(parse_line("processing file: report.qmd"), OutputLine::Other);
        assert_eq!(strip_escapes("plain"), "plain");
    }

    #[test]
    fn test_shows_in_viewer() {
        assert!(shows_in_viewer("http://localhost:6437/report.html"));
        assert!(shows_in_viewer("http://localhost:6437/"));
        assert!(shows_in_viewer("notes.pdf"));
        assert!(shows_in_viewer("http://localhost:6437/slides.html#/2"));
        assert!(!shows_in_viewer("http://localhost:6835/word.docx"));
        assert!(!shows_in_viewer("deck.pptx"));
    }
}
