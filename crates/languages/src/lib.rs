use futures::FutureExt as _;
use gpui::{App, SharedString, UpdateGlobal};
use node_runtime::NodeRuntime;
use project::Fs;
use python::PyprojectTomlManifestProvider;
use rust::CargoManifestProvider;
use settings::{SemanticTokenRules, SettingsStore};
use smol::stream::StreamExt;
use std::sync::Arc;
use util::ResultExt;

pub use language::*;

use crate::{
    json::JsonTaskProvider,
    python::{BasedPyrightLspAdapter, PyreflyLspAdapter, RuffLspAdapter},
};

mod bash;
mod c;
mod cpp;
mod css;
mod eslint;
mod go;
mod json;
mod package_json;
mod python;
pub mod r;
mod rust;
mod tailwind;
mod tailwindcss;
mod typescript;
mod vtsls;
mod yaml;

pub(crate) use package_json::{PackageJson, PackageJsonData};

/// A shared grammar for plain text, exposed for reuse by downstream crates.
#[cfg(feature = "tree-sitter-gitcommit")]
pub static LANGUAGE_GIT_COMMIT: std::sync::LazyLock<Arc<Language>> =
    std::sync::LazyLock::new(|| {
        Arc::new(Language::new(
            LanguageConfig {
                name: "Git Commit".into(),
                soft_wrap: Some(language::SoftWrap::EditorWidth),
                matcher: (LanguageMatcher {
                    path_suffixes: vec!["COMMIT_EDITMSG".into()],
                    first_line_pattern: None,
                    ..LanguageMatcher::default()
                })
                .into(),
                line_comments: vec![Arc::from("#")],
                ..LanguageConfig::default()
            },
            Some(tree_sitter_gitcommit::LANGUAGE.into()),
        ))
    });

pub fn init(languages: Arc<LanguageRegistry>, fs: Arc<dyn Fs>, node: NodeRuntime, cx: &mut App) {
    #[cfg(feature = "load-grammars")]
    languages.register_native_grammars(grammars::native_grammars());

    let bash_lsp_adapter = Arc::new(bash::BashLspAdapter::new(node.clone()));
    let c_lsp_adapter = Arc::new(c::CLspAdapter);
    let css_lsp_adapter = Arc::new(css::CssLspAdapter::new(node.clone()));
    let eslint_adapter = Arc::new(eslint::EsLintLspAdapter::new(node.clone(), fs.clone()));
    let go_context_provider = Arc::new(go::GoContextProvider);
    let go_lsp_adapter = Arc::new(go::GoLspAdapter);
    let json_context_provider = Arc::new(JsonTaskProvider);
    let json_lsp_adapter = Arc::new(json::JsonLspAdapter::new(languages.clone(), node.clone()));
    let node_version_lsp_adapter = Arc::new(json::NodeVersionAdapter);
    let py_lsp_adapter = Arc::new(python::PyLspAdapter::new());
    let ty_lsp_adapter = Arc::new(python::TyLspAdapter::new(fs.clone()));
    let python_context_provider = Arc::new(python::PythonContextProvider);
    let python_lsp_adapter = Arc::new(python::PyrightLspAdapter::new(node.clone()));
    let basedpyright_lsp_adapter = Arc::new(BasedPyrightLspAdapter::new(node.clone()));
    let ruff_lsp_adapter = Arc::new(RuffLspAdapter::new(fs.clone()));
    let pyrefly_lsp_adapter = Arc::new(PyreflyLspAdapter::new(fs.clone()));
    let python_toolchain_provider = Arc::new(python::PythonToolchainProvider::new(fs.clone()));
    let rust_context_provider = Arc::new(rust::RustContextProvider);
    let rust_lsp_adapter = Arc::new(rust::RustLspAdapter);
    let tailwind_adapter = Arc::new(tailwind::TailwindLspAdapter::new(node.clone()));
    let tailwindcss_adapter = Arc::new(tailwindcss::TailwindCssLspAdapter::new(node.clone()));
    let typescript_context = Arc::new(typescript::TypeScriptContextProvider::new(fs.clone()));
    let typescript_lsp_adapter = Arc::new(typescript::TypeScriptLspAdapter::new(
        node.clone(),
        fs.clone(),
    ));
    let vtsls_adapter = Arc::new(vtsls::VtslsLspAdapter::new(node.clone(), fs.clone()));
    let yaml_lsp_adapter = Arc::new(yaml::YamlLspAdapter::new(node));

    let built_in_languages = [
        LanguageInfo {
            name: "bash",
            context: Some(Arc::new(bash::bash_task_context())),
            adapters: vec![bash_lsp_adapter],
            ..Default::default()
        },
        LanguageInfo {
            name: "c",
            adapters: vec![c_lsp_adapter.clone()],
            ..Default::default()
        },
        LanguageInfo {
            name: "cpp",
            adapters: vec![c_lsp_adapter],
            semantic_token_rules: Some(cpp::semantic_token_rules()),
            ..Default::default()
        },
        LanguageInfo {
            name: "css",
            adapters: vec![css_lsp_adapter],
            ..Default::default()
        },
        LanguageInfo {
            name: "diff",
            adapters: vec![],
            ..Default::default()
        },
        LanguageInfo {
            name: "env",
            adapters: vec![],
            ..Default::default()
        },
        LanguageInfo {
            name: "go",
            adapters: vec![go_lsp_adapter.clone()],
            context: Some(go_context_provider.clone()),
            semantic_token_rules: Some(go::semantic_token_rules()),
            ..Default::default()
        },
        LanguageInfo {
            name: "gomod",
            adapters: vec![go_lsp_adapter.clone()],
            context: Some(go_context_provider.clone()),
            ..Default::default()
        },
        LanguageInfo {
            name: "gowork",
            adapters: vec![go_lsp_adapter],
            context: Some(go_context_provider),
            ..Default::default()
        },
        LanguageInfo {
            name: "json",
            adapters: vec![json_lsp_adapter.clone(), node_version_lsp_adapter],
            context: Some(json_context_provider.clone()),
            ..Default::default()
        },
        LanguageInfo {
            name: "jsonc",
            adapters: vec![json_lsp_adapter],
            context: Some(json_context_provider),
            ..Default::default()
        },
        LanguageInfo {
            name: "markdown",
            adapters: vec![],
            ..Default::default()
        },
        LanguageInfo {
            name: "markdown-inline",
            adapters: vec![],
            ..Default::default()
        },
        LanguageInfo {
            name: "rmarkdown",
            adapters: vec![],
            ..Default::default()
        },
        LanguageInfo {
            name: "quarto",
            adapters: vec![],
            ..Default::default()
        },
        LanguageInfo {
            name: "python",
            adapters: vec![
                basedpyright_lsp_adapter,
                ruff_lsp_adapter,
                ty_lsp_adapter,
                py_lsp_adapter,
                python_lsp_adapter,
                pyrefly_lsp_adapter,
            ],
            context: Some(python_context_provider),
            toolchain: Some(python_toolchain_provider),
            manifest_name: Some(SharedString::new_static("pyproject.toml").into()),
            semantic_token_rules: Some(python::semantic_token_rules()),
        },
        LanguageInfo {
            name: "r",
            adapters: vec![],
            toolchain: Some(Arc::new(r::RToolchainLister)),
            ..Default::default()
        },
        LanguageInfo {
            name: "rust",
            adapters: vec![rust_lsp_adapter],
            context: Some(rust_context_provider),
            manifest_name: Some(SharedString::new_static("Cargo.toml").into()),
            semantic_token_rules: Some(rust::semantic_token_rules()),
            ..Default::default()
        },
        LanguageInfo {
            name: "tsx",
            adapters: vec![typescript_lsp_adapter.clone(), vtsls_adapter.clone()],
            context: Some(typescript_context.clone()),
            ..Default::default()
        },
        LanguageInfo {
            name: "typescript",
            adapters: vec![typescript_lsp_adapter.clone(), vtsls_adapter.clone()],
            context: Some(typescript_context.clone()),
            ..Default::default()
        },
        LanguageInfo {
            name: "javascript",
            adapters: vec![typescript_lsp_adapter.clone(), vtsls_adapter.clone()],
            context: Some(typescript_context),
            ..Default::default()
        },
        LanguageInfo {
            name: "jsdoc",
            adapters: vec![typescript_lsp_adapter.clone(), vtsls_adapter.clone()],
            ..Default::default()
        },
        LanguageInfo {
            name: "regex",
            adapters: vec![],
            ..Default::default()
        },
        LanguageInfo {
            name: "yaml",
            adapters: vec![yaml_lsp_adapter],
            ..Default::default()
        },
        LanguageInfo {
            name: "gitcommit",
            ..Default::default()
        },
        LanguageInfo {
            name: "zed-keybind-context",
            ..Default::default()
        },
    ];

    for registration in built_in_languages {
        register_language(
            &languages,
            registration.name,
            registration.adapters,
            registration.context,
            registration.toolchain,
            registration.manifest_name,
            registration.semantic_token_rules,
            cx,
        );
    }

    // Register globally available language servers.
    //
    // This will allow users to add support for a built-in language server (e.g., Tailwind)
    // for a given language via the `language_servers` setting:
    //
    // ```json
    // {
    //   "languages": {
    //     "My Language": {
    //       "language_servers": ["tailwindcss-language-server", "..."]
    //     }
    //   }
    // }
    // ```
    languages.register_available_lsp_adapter(
        LanguageServerName("tailwindcss-language-server".into()),
        tailwind_adapter.clone(),
    );
    languages.register_available_lsp_adapter(
        LanguageServerName("tailwindcss-intellisense-css".into()),
        tailwindcss_adapter,
    );
    languages.register_available_lsp_adapter(
        LanguageServerName("eslint".into()),
        eslint_adapter.clone(),
    );
    languages.register_available_lsp_adapter(LanguageServerName("vtsls".into()), vtsls_adapter);
    languages.register_available_lsp_adapter(
        LanguageServerName("typescript-language-server".into()),
        typescript_lsp_adapter,
    );

    // Register Tailwind for the existing languages that should have it by default.
    //
    // This can be driven by the `language_servers` setting once we have a way for
    // extensions to provide their own default value for that setting.
    let tailwind_languages = [
        "Astro",
        "CSS",
        "ERB",
        "HTML+ERB",
        "HEEx",
        "HTML",
        "JavaScript",
        "TypeScript",
        "PHP",
        "Svelte",
        "TSX",
        "Vue.js",
    ];

    for language in tailwind_languages {
        languages.register_lsp_adapter(language.into(), tailwind_adapter.clone());
    }

    let eslint_languages = ["TSX", "TypeScript", "JavaScript", "Vue.js", "Svelte"];
    for language in eslint_languages {
        languages.register_lsp_adapter(language.into(), eslint_adapter.clone());
    }

    let mut subscription = languages.subscribe();
    let mut prev_language_settings = languages.language_settings();

    cx.spawn(async move |cx| {
        while subscription.next().await.is_some() {
            let language_settings = languages.language_settings();
            if language_settings != prev_language_settings {
                cx.update(|cx| {
                    SettingsStore::update_global(cx, |settings, cx| {
                        settings
                            .set_extension_settings(
                                settings::ExtensionsSettingsContent {
                                    all_languages: language_settings.clone(),
                                },
                                cx,
                            )
                            .log_err();
                    });
                });
                prev_language_settings = language_settings;
            }
        }
        anyhow::Ok(())
    })
    .detach();
    let manifest_providers: [Arc<dyn ManifestProvider>; 2] = [
        Arc::from(CargoManifestProvider),
        Arc::from(PyprojectTomlManifestProvider),
    ];
    for provider in manifest_providers {
        project::ManifestProvidersStore::global(cx).register(provider);
    }
}

#[derive(Default)]
struct LanguageInfo {
    name: &'static str,
    adapters: Vec<Arc<dyn LspAdapter>>,
    context: Option<Arc<dyn ContextProvider>>,
    toolchain: Option<Arc<dyn ToolchainLister>>,
    manifest_name: Option<ManifestName>,
    semantic_token_rules: Option<SemanticTokenRules>,
}

fn register_language(
    languages: &LanguageRegistry,
    name: &'static str,
    adapters: Vec<Arc<dyn LspAdapter>>,
    context: Option<Arc<dyn ContextProvider>>,
    toolchain: Option<Arc<dyn ToolchainLister>>,
    manifest_name: Option<ManifestName>,
    semantic_token_rules: Option<SemanticTokenRules>,
    cx: &mut App,
) {
    let config = load_config(name);
    if let Some(rules) = &semantic_token_rules {
        SettingsStore::update_global(cx, |store, _| {
            store.set_language_semantic_token_rules(config.name.0.clone(), rules.clone());
        });
    }
    for adapter in adapters {
        languages.register_lsp_adapter(config.name.clone(), adapter);
    }
    languages.register_language(
        config.name.clone(),
        config.grammar.clone(),
        config.matcher.clone(),
        config.hidden,
        manifest_name.clone(),
        Arc::new(move || {
            let config = config.clone();
            let context = context.clone();
            let toolchain = toolchain.clone();
            let manifest_name = manifest_name.clone();
            async move {
                Ok(LoadedLanguage {
                    config,
                    queries: grammars::load_queries(queries_name(name)),
                    context_provider: context,
                    toolchain_provider: toolchain,
                    manifest_name,
                })
            }
            .boxed()
        }),
    );
}

/// R Markdown and Quarto are Markdown with their own names and file types;
/// their R chunks come from Markdown's code fence injection.
const MARKDOWN_DIALECTS: [&str; 2] = ["rmarkdown", "quarto"];

fn queries_name(name: &str) -> &str {
    if MARKDOWN_DIALECTS.contains(&name) {
        "markdown"
    } else {
        name
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn language(name: &str, grammar: tree_sitter::Language) -> Arc<Language> {
    Arc::new(
        Language::new(grammars::load_config(name), Some(grammar))
            .with_queries(grammars::load_queries(queries_name(name)))
            .unwrap(),
    )
}

fn load_config(name: &str) -> LanguageConfig {
    let grammars_loaded = cfg!(any(feature = "load-grammars", test));
    grammars::load_config_for_feature(name, grammars_loaded)
}

#[cfg(test)]
mod tests {
    use gpui::{App, AppContext as _, BorrowAppContext as _};
    use language::{File, LanguageRegistry, TestFile};
    use settings::SettingsStore;
    use std::sync::Arc;
    use util::rel_path::rel_path;

    #[gpui::test]
    fn test_env_files_are_not_shell_scripts(cx: &mut App) {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);

        let registry = Arc::new(LanguageRegistry::test(cx.background_executor().clone()));
        registry.add(crate::language("env", tree_sitter_bash::LANGUAGE.into()));
        registry.add(crate::language("bash", tree_sitter_bash::LANGUAGE.into()));

        let language_of = |path: &str, cx: &App| {
            let file: Arc<dyn File> = Arc::new(TestFile {
                path: Arc::from(rel_path(path)),
                root_name: "zed".into(),
                local_root: None,
            });

            registry
                .language_for_file(&file, None, cx)
                .and_then(|id| registry.language_name_for_id(id))
        };

        assert_eq!(language_of(".env", cx), Some("Env".into()));
        assert_eq!(language_of(".envrc", cx), Some("Shell Script".into()));
        assert_eq!(language_of("deploy.sh", cx), Some("Shell Script".into()));
    }

    #[gpui::test]
    fn test_r_file_detection(cx: &mut App) {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);

        let registry = Arc::new(LanguageRegistry::test(cx.background_executor().clone()));
        registry.add(crate::language("r", tree_sitter_r::LANGUAGE.into()));

        let language_of = |path: &str, cx: &App| {
            let file: Arc<dyn File> = Arc::new(TestFile {
                path: Arc::from(rel_path(path)),
                root_name: "project".into(),
                local_root: None,
            });

            registry
                .language_for_file(&file, None, cx)
                .and_then(|id| registry.language_name_for_id(id))
        };

        assert_eq!(language_of("analysis.R", cx), Some("R".into()));
        assert_eq!(language_of("utils.r", cx), Some("R".into()));
        assert_eq!(language_of(".Rprofile", cx), Some("R".into()));
        assert_eq!(language_of("notes.Rmd", cx), None, "R Markdown isn't R");

        let r_settings = registry.language_settings().languages.0.get("R").cloned();
        assert_eq!(
            r_settings.and_then(|settings| settings.tab_size),
            std::num::NonZeroU32::new(2)
        );
    }

    #[gpui::test]
    async fn test_r_markdown_and_quarto(cx: &mut gpui::TestAppContext) {
        use language::{Buffer, Point};

        let registry = cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            let registry = Arc::new(LanguageRegistry::test(cx.background_executor().clone()));
            registry.add(crate::language(
                "rmarkdown",
                tree_sitter_md::LANGUAGE.into(),
            ));
            registry.add(crate::language("quarto", tree_sitter_md::LANGUAGE.into()));
            registry.add(crate::language(
                "markdown-inline",
                tree_sitter_md::INLINE_LANGUAGE.into(),
            ));
            registry.add(crate::language("r", tree_sitter_r::LANGUAGE.into()));
            registry
        });
        let language_of = |path: &str, cx: &mut gpui::TestAppContext| {
            cx.update(|cx| {
                let file: Arc<dyn File> = Arc::new(TestFile {
                    path: Arc::from(rel_path(path)),
                    root_name: "project".into(),
                    local_root: None,
                });
                registry
                    .language_for_file(&file, None, cx)
                    .and_then(|id| registry.language_name_for_id(id))
            })
        };
        assert_eq!(language_of("report.Rmd", cx), Some("R Markdown".into()));
        assert_eq!(
            language_of("notes.rmarkdown", cx),
            Some("R Markdown".into())
        );
        assert_eq!(language_of("paper.qmd", cx), Some("Quarto".into()));

        // R chunks, with or without options, are parsed as R; prose stays Markdown.
        let text = "---\ntitle: Report\n---\n\nSome *prose*.\n\n```{r setup, echo=FALSE}\nx <- 1\n```\n\n```{r}\nmean(x)\n```\n";
        let rmarkdown = registry.language_for_name("R Markdown").await.unwrap();
        // The registry comes first, so the parse can find R for the chunks.
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::local(text, cx);
            buffer.set_language_registry(registry.clone());
            buffer.set_language(Some(rmarkdown), cx);
            buffer
        });
        cx.run_until_parked();
        buffer.read_with(cx, |buffer, _| {
            let language_at = |row: u32| {
                buffer
                    .snapshot()
                    .language_at(Point::new(row, 1))
                    .map(|language| language.name().to_string())
            };
            assert_eq!(language_at(4).as_deref(), Some("R Markdown"));
            assert_eq!(language_at(7).as_deref(), Some("R"));
            assert_eq!(language_at(11).as_deref(), Some("R"));
        });
    }

    #[gpui::test]
    async fn test_r_outline(cx: &mut gpui::TestAppContext) {
        let language = crate::language("r", tree_sitter_r::LANGUAGE.into());
        let text = unindent::Unindent::unindent(
            r#"
            # Load data ----
            data <- read.csv("data.csv")

            clean <- function(df) {
              helper = function(x) x + 1
              df
            }

            # Models ====
            "fit model" <- function(df, ...) lm(y ~ x, df)
            summarise = \(df) summary(df)
            # not a section --
            "#,
        );

        let buffer = cx.new(|cx| {
            let mut buffer = language::Buffer::local(text, cx);
            buffer.set_sync_parse_timeout(None);
            buffer.set_language(Some(language), cx);
            buffer
        });
        buffer
            .read_with(cx, |buffer, _| buffer.parsing_idle())
            .await;
        let outline = buffer.read_with(cx, |buffer, _| buffer.snapshot().outline(None));
        pretty_assertions::assert_eq!(
            outline
                .items
                .iter()
                .map(|item| (item.text.as_str(), item.depth))
                .collect::<Vec<_>>(),
            &[
                ("# Load data ----", 0),
                ("clean", 0),
                ("helper", 1),
                ("# Models ====", 0),
                ("\"fit model\"", 0),
                ("summarise", 0),
            ]
        );
    }

    #[gpui::test]
    fn test_r_highlights() {
        let language = crate::language("r", tree_sitter_r::LANGUAGE.into());
        let capture_names = [
            "variable",
            "variable.parameter",
            "variable.special",
            "function",
            "keyword",
            "operator",
            "property",
            "namespace",
            "string",
            "number",
            "boolean",
            "constant.builtin",
            "comment",
            "comment.doc",
        ];
        let theme =
            theme::SyntaxTheme::new_test(capture_names.map(|name| (name, gpui::Hsla::default())));
        language.set_theme(&theme);
        let grammar = language.grammar().expect("R has a grammar");

        let text = unindent::Unindent::unindent(
            r#"
            #' Fit a model
            fit <- function(df, ...) {
              if (is.null(df)) return(NA)
              stats::lm(y ~ x, data = df$values, 1L, TRUE)
            }
            "#,
        );
        let runs = language
            .highlight_text(text.as_str(), 0..text.len())
            .collect::<Vec<_>>();
        let capture_at = |needle: &str, occurrence: usize| {
            let start = text
                .match_indices(needle)
                .nth(occurrence)
                .unwrap_or_else(|| panic!("{needle:?} not in sample"))
                .0;
            runs.iter()
                .find(|(range, _)| *range == (start..start + needle.len()))
                .map(|(_, id)| *id)
        };
        let expect = |needle: &str, occurrence: usize, capture: &str| {
            let expected = grammar
                .highlight_id_for_name(capture)
                .unwrap_or_else(|| panic!("{capture} is not in the test theme"));
            assert_eq!(
                capture_at(needle, occurrence),
                Some(expected),
                "{needle:?} should be highlighted as {capture}"
            );
        };

        expect("#' Fit a model", 0, "comment.doc");
        expect("fit", 0, "function");
        expect("<-", 0, "operator");
        expect("function", 0, "keyword");
        expect("df", 0, "variable.parameter");
        expect("...", 0, "variable.special");
        expect("if", 0, "keyword");
        expect("is.null", 0, "function");
        expect("return", 0, "keyword");
        expect("NA", 0, "constant.builtin");
        expect("stats", 0, "namespace");
        expect("lm", 0, "function");
        expect("data", 0, "variable.parameter");
        expect("values", 0, "property");
        expect("1L", 0, "number");
        expect("TRUE", 0, "boolean");
    }

    #[gpui::test]
    async fn test_r_autoindent(cx: &mut gpui::TestAppContext) {
        cx.executor().set_block_on_ticks(usize::MAX..=usize::MAX);
        let language = crate::language("r", tree_sitter_r::LANGUAGE.into());
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.all_languages.defaults.tab_size = std::num::NonZeroU32::new(2);
                });
            });
        });

        // The editor auto-closes brackets, so newlines are typed between a matched pair.
        cx.new(|cx| {
            let mut buffer = language::Buffer::local("", cx).with_language(language, cx);

            buffer.edit([(0..0, "f <- function(x) {}")], None, cx);
            let before_brace = buffer.len() - 1;
            buffer.edit(
                [(before_brace..before_brace, "\n\n")],
                Some(language::AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(buffer.text(), "f <- function(x) {\n  \n}");

            let end = buffer.len();
            buffer.edit([(end..end, "\nmean()")], None, cx);
            let before_paren = buffer.len() - 1;
            buffer.edit(
                [(before_paren..before_paren, "\n\n")],
                Some(language::AutoindentMode::EachLine),
                cx,
            );
            assert_eq!(buffer.text(), "f <- function(x) {\n  \n}\nmean(\n  \n)");

            buffer
        });
    }
}
