//! R's help pages in the app's theme and fonts. Ark serves its own `R.css`
//! and `prism.css` with help pages, which take their fonts and colours from
//! variables Positron defines; this defines them from the app's theme, and
//! adds what those sheets lack to set code apart, as RStudio does.

use gpui::{App, Hsla, Rgba};
use settings::Settings as _;
use theme::ActiveTheme as _;
use theme_settings::ThemeSettings;
use web_view::PageStyle;

const SANS_FALLBACK: &str = "-apple-system, system-ui, sans-serif";
const MONO_FALLBACK: &str = "ui-monospace, \"SF Mono\", Menlo, monospace";

pub(crate) fn help_page_style(cx: &App) -> PageStyle {
    let theme = cx.theme();
    let colors = theme.colors();
    let settings = ThemeSettings::get_global(cx);
    let background = colors.panel_background;
    let text = colors.text;
    let syntax = |name: &str| {
        css_color(
            theme
                .syntax()
                .style_for_name(name)
                .and_then(|style| style.color)
                .unwrap_or(text),
        )
    };
    let css = format!(
        r#":root {{
  color-scheme: {scheme};
  --vscode-font-family: {ui_font};
  --vscode-font-size: {ui_size}px;
  --vscode-editor-font-family: {code_font};
  --vscode-editor-font-size: {code_size}px;
  --vscode-editor-font-weight: normal;
  --vscode-editor-foreground: {text};
  --vscode-editor-background: {background};
  --vscode-editor-selectionBackground: {selection};
  --vscode-textLink-foreground: {link};
  --vscode-textBlockQuote-border: {border};
  --vscode-positronConsole-ansiRed: {ansi_red};
  --vscode-positronConsole-ansiGreen: {ansi_green};
  --vscode-positronConsole-ansiCyan: {ansi_cyan};
  --vscode-positronConsole-ansiMagenta: {ansi_magenta};
  --vscode-positronConsole-ansiBrightYellow: {ansi_bright_yellow};
}}
html body {{ margin: 0; padding: 8px 16px 24px; }}
html body h2 {{ font-size: 1.5em; font-weight: 600; line-height: 1.3; }}
html body h3 {{ font-size: 1.15em; font-weight: 600; margin: 1.4em 0 0.5em; }}
html body pre {{
  background: {code_background};
  border: 1px solid {border};
  border-radius: 6px;
  padding: 8px 12px;
  overflow-x: auto;
  line-height: 1.4;
}}
html body pre code {{ background: none; border: none; padding: 0; }}
html body table td {{ padding: 2px 12px 2px 0; }}
html body a {{ text-decoration: none; }}
html body a:hover {{ text-decoration: underline; }}
html body img {{ max-width: 100%; }}
html .token.comment {{ color: {comment}; }}
html .token.string, html .token.char {{ color: {string}; }}
html .token.number {{ color: {number}; }}
html .token.boolean, html .token.constant {{ color: {boolean}; }}
html .token.keyword {{ color: {keyword}; }}
html .token.operator {{ color: {operator}; }}
html .token.punctuation {{ color: {punctuation}; }}
"#,
        scheme = if theme.appearance().is_light() {
            "light"
        } else {
            "dark"
        },
        ui_font = font_stack(&settings.ui_font.family, SANS_FALLBACK),
        ui_size = f32::from(settings.ui_font_size(cx)),
        code_font = font_stack(&settings.buffer_font.family, MONO_FALLBACK),
        code_size = f32::from(settings.buffer_font_size(cx)),
        text = css_color(text),
        background = css_color(background),
        selection = css_color(theme.players().local().selection),
        link = css_color(colors.text_accent),
        border = css_color(colors.border_variant),
        code_background = css_color(colors.editor_background),
        ansi_red = css_color(colors.terminal_ansi_red),
        ansi_green = css_color(colors.terminal_ansi_green),
        ansi_cyan = css_color(colors.terminal_ansi_cyan),
        ansi_magenta = css_color(colors.terminal_ansi_magenta),
        ansi_bright_yellow = css_color(colors.terminal_ansi_bright_yellow),
        comment = syntax("comment"),
        string = syntax("string"),
        number = syntax("number"),
        boolean = syntax("boolean"),
        keyword = syntax("keyword"),
        operator = syntax("operator"),
        punctuation = syntax("punctuation"),
    );
    PageStyle {
        css,
        background: Rgba::from(background),
    }
}

fn css_color(color: Hsla) -> String {
    let color = Rgba::from(color);
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u8;
    format!(
        "rgba({}, {}, {}, {:.3})",
        channel(color.r),
        channel(color.g),
        channel(color.b),
        color.a.clamp(0., 1.)
    )
}

/// `family` as WebKit can find it, then the system's fonts. WebKit draws in
/// its own process, which can't see the fonts the app bundles, so those fall
/// back to the system's unless they're also installed.
fn font_stack(family: &str, fallback: &str) -> String {
    let family = gpui::font_name_with_fallbacks(family, "");
    if family.is_empty() {
        return fallback.to_string();
    }
    let family = family.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{family}\", {fallback}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_font_stack() {
        assert_eq!(
            font_stack(".ZedMono", MONO_FALLBACK),
            format!("\"Lilex\", {MONO_FALLBACK}")
        );
        assert_eq!(font_stack(".SystemUIFont", SANS_FALLBACK), SANS_FALLBACK);
        assert_eq!(
            font_stack("My \"Odd\" Font", SANS_FALLBACK),
            format!("\"My \\\"Odd\\\" Font\", {SANS_FALLBACK}")
        );
    }

    #[test]
    fn test_css_color() {
        assert_eq!(
            css_color(Hsla::from(gpui::rgba(0x282c33ff))),
            "rgba(40, 44, 51, 1.000)"
        );
    }
}
