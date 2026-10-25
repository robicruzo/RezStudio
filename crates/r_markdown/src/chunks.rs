//! Code chunks in R Markdown and Quarto: fenced blocks whose info string is
//! braced, such as ```` ```{r label, echo=FALSE} ````. Plain fences
//! (```` ```r ````) are just code to show, not chunks to run.

use std::ops::Range;

#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    /// The language that runs it, such as `r` or `python`, as written.
    pub engine: String,
    pub label: Option<String>,
    /// From the header (`{r, echo=FALSE}`) and then Quarto's `#|` lines, in order.
    pub options: Vec<(String, String)>,
    /// The row of the opening fence.
    pub header_row: u32,
    /// The rows between the fences: the chunk's code.
    pub code_rows: Range<u32>,
    /// The row of the closing fence; `None` when the chunk runs to the end.
    pub closing_row: Option<u32>,
}

impl Chunk {
    pub fn is_r(&self) -> bool {
        self.engine.eq_ignore_ascii_case("r")
    }

    /// The value of an option, the last one given winning, as in knitr.
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// Whether `eval` says the chunk should be skipped when running many.
    pub fn is_eval_false(&self) -> bool {
        self.option("eval")
            .is_some_and(|value| matches!(value, "FALSE" | "F" | "false"))
    }

    /// The last row that belongs to the chunk, fences included.
    pub fn last_row(&self) -> u32 {
        self.closing_row
            .unwrap_or(self.code_rows.end.saturating_sub(1).max(self.header_row))
    }

    pub fn contains_row(&self, row: u32) -> bool {
        (self.header_row..=self.last_row()).contains(&row)
    }
}

/// Every chunk in `text`, in order.
pub fn parse_chunks(text: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    let line_count = lines.len() as u32;
    let mut chunks = Vec::new();
    let mut row = 0;
    while (row as usize) < lines.len() {
        let Some(fence) = Fence::opening(lines[row as usize]) else {
            row += 1;
            continue;
        };
        let closing_row = (row + 1..line_count).find(|&candidate| {
            fence.is_closed_by(lines.get(candidate as usize).copied().unwrap_or_default())
        });
        let code_end = closing_row.unwrap_or(line_count);
        if let Some(header) = fence.chunk_header {
            let (engine, label, mut options) = parse_header(&header);
            let code = lines
                .get(row as usize + 1..code_end as usize)
                .unwrap_or_default();
            options.extend(quarto_options(code));
            let label = label.or_else(|| {
                options
                    .iter()
                    .rev()
                    .find(|(name, _)| name == "label")
                    .map(|(_, value)| value.clone())
            });
            chunks.push(Chunk {
                engine,
                label,
                options,
                header_row: row,
                code_rows: row + 1..code_end,
                closing_row,
            });
        }
        // Whatever is inside a fence, chunk or not, can't start another.
        row = closing_row.map_or(line_count, |closing| closing + 1);
    }
    chunks
}

/// The chunk at `row`, fences included.
pub fn chunk_at_row(chunks: &[Chunk], row: u32) -> Option<&Chunk> {
    chunks.iter().find(|chunk| chunk.contains_row(row))
}

struct Fence {
    character: char,
    length: usize,
    /// The text between the braces, for a chunk.
    chunk_header: Option<String>,
}

impl Fence {
    fn opening(line: &str) -> Option<Self> {
        let trimmed = line.trim_start();
        let character = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
        let length = trimmed.chars().take_while(|c| *c == character).count();
        if length < 3 {
            return None;
        }
        let info = trimmed[length..].trim();
        // A backtick fence's info string can't contain backticks; that's inline code.
        if character == '`' && info.contains('`') {
            return None;
        }
        let chunk_header = info
            .strip_prefix('{')
            .and_then(|rest| rest.strip_suffix('}'))
            .map(|header| header.trim().to_string())
            .filter(|header| {
                header
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_alphabetic())
            });
        Some(Self {
            character,
            length,
            chunk_header,
        })
    }

    fn is_closed_by(&self, line: &str) -> bool {
        let trimmed = line.trim();
        trimmed.chars().count() >= self.length && trimmed.chars().all(|c| c == self.character)
    }
}

/// `r label, echo=FALSE, fig.cap="A, B"` into the engine, label and options.
fn parse_header(header: &str) -> (String, Option<String>, Vec<(String, String)>) {
    let engine_end = header
        .find(|c: char| c.is_whitespace() || c == ',')
        .unwrap_or(header.len());
    let engine = header[..engine_end].to_string();
    let mut label = None;
    let mut options = Vec::new();
    for (index, part) in split_top_level(&header[engine_end..])
        .into_iter()
        .enumerate()
    {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('=') {
            Some((key, value)) => {
                options.push((key.trim().to_string(), unquote(value.trim()).to_string()))
            }
            // Only the first part can be a bare label.
            None if index == 0 => label = Some(part.to_string()),
            None => {}
        }
    }
    (engine, label, options)
}

/// Splits on commas outside quotes and brackets.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quote = None;
    let mut start = 0;
    let mut previous = '\0';
    for (index, character) in text.char_indices() {
        match (quote, character) {
            (Some(open), c) if c == open && previous != '\\' => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(character),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']' | '}') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
        previous = character;
    }
    parts.push(&text[start..]);
    parts
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

/// Quarto's `#| key: value` lines at the top of a chunk.
fn quarto_options(code: &[&str]) -> Vec<(String, String)> {
    code.iter()
        .map(|line| line.trim_start())
        .take_while(|line| line.starts_with("#|"))
        .filter_map(|line| {
            let (key, value) = line.trim_start_matches("#|").split_once(':')?;
            Some((key.trim().to_string(), unquote(value.trim()).to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOCUMENT: &str = "---
title: \"Report\"
output: html_document
---

Prose with `inline code` and ```` four backticks ````.

```{r setup, include=FALSE}
library(dplyr)
```

```r
# Shown, not run: a plain fence.
```

```{r, fig.cap=\"Miles, gallons\", fig.width=7}
plot(mtcars$mpg)
```

```{python}
print(1)
```

~~~{r}
#| label: quarto-style
#| echo: false
summary(cars)
~~~

  ```{r indented}
  x
  ```

```{r unterminated}
tail()
";

    #[test]
    fn test_parse_chunks() {
        let chunks = parse_chunks(DOCUMENT);
        let summary: Vec<(String, Option<String>, u32, Range<u32>, Option<u32>)> = chunks
            .iter()
            .map(|chunk| {
                (
                    chunk.engine.clone(),
                    chunk.label.clone(),
                    chunk.header_row,
                    chunk.code_rows.clone(),
                    chunk.closing_row,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("r".into(), Some("setup".into()), 7, 8..9, Some(9)),
                ("r".into(), None, 15, 16..17, Some(17)),
                ("python".into(), None, 19, 20..21, Some(21)),
                (
                    "r".into(),
                    Some("quarto-style".into()),
                    23,
                    24..27,
                    Some(27)
                ),
                ("r".into(), Some("indented".into()), 29, 30..31, Some(31)),
                ("r".into(), Some("unterminated".into()), 33, 34..35, None),
            ]
        );
        assert_eq!(chunks[0].option("include"), Some("FALSE"));
        assert_eq!(chunks[1].option("fig.cap"), Some("Miles, gallons"));
        assert_eq!(chunks[1].option("fig.width"), Some("7"));
        assert_eq!(chunks[3].option("echo"), Some("false"));
        assert!(!chunks[2].is_r());
        assert_eq!(chunks[5].last_row(), 34);
    }

    #[test]
    fn test_chunk_at_row() {
        let chunks = parse_chunks(DOCUMENT);
        let label_at = |row| chunk_at_row(&chunks, row).map(|chunk| chunk.header_row);
        assert_eq!(label_at(7), Some(7), "the opening fence");
        assert_eq!(label_at(8), Some(7));
        assert_eq!(label_at(9), Some(7), "the closing fence");
        assert_eq!(label_at(10), None, "between chunks");
        assert_eq!(label_at(12), None, "a plain fence isn't a chunk");
    }

    #[test]
    fn test_eval_option() {
        let chunks = parse_chunks(
            "```{r, eval=FALSE}\nx\n```\n```{r}\n#| eval: false\ny\n```\n```{r}\nz\n```\n",
        );
        let skipped: Vec<bool> = chunks.iter().map(Chunk::is_eval_false).collect();
        assert_eq!(skipped, [true, true, false]);
    }

    #[test]
    fn test_header_splitting() {
        assert_eq!(
            split_top_level(" a, b=c(1, 2), d='x,y'"),
            [" a", " b=c(1, 2)", " d='x,y'"]
        );
        let (engine, label, options) = parse_header("r my-label, fig.height = 4");
        assert_eq!(engine, "r");
        assert_eq!(label.as_deref(), Some("my-label"));
        assert_eq!(options, [("fig.height".to_string(), "4".to_string())]);
    }
}
