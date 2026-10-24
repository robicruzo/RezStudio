# RezStudio — Design

RezStudio is a data-science IDE built on Zed: RStudio's workflow and panes, Zed's speed and editor.
Relationship to Zed is analogous to Positron's relationship to VS Code, but without Positron's
extension-host architecture — every pane is native GPUI, written in Rust.

## Decisions (agreed)

| Topic | Decision |
|---|---|
| Platform | macOS only for shipped builds. Code keeps compiling on Linux so development/CI can run there; macOS-only features are behind `cfg(target_os = "macos")`. |
| Languages | **R** (first class, incl. R Markdown and Quarto), **Python** (first class, second to R), **Typst**, and Quarto/Markdown documents. |
| R backend | **Ark** (posit-dev/ark, MIT): Jupyter kernel + LSP + DAP, plus Jupyter *comms* for variables, plots, data paging, help and UI events. |
| Python backend | `ipykernel` plus our own small add-on package that speaks the same comm protocol as Ark. (Positron's Python kernel is Elastic-licensed, so it is not reused.) |
| HTML content | Embedded **WKWebView** for the Viewer (htmlwidgets, Shiny, Quarto preview) and R help pages. Linux dev builds fall back to the system browser. |
| Typst | Tinymist LSP for editing; Typst compiler embedded as a library for a **native live preview** pane. |
| Fork style | Middle ground: delete AI, collab/calls and zed.dev cloud features; keep extensions and everything else close to upstream so we can keep merging Zed. |
| AI | Removed entirely. |
| MVP | Console + run-from-editor, Environment + data viewer, Plots, Packages (+ search), Rmd/Quarto inline chunks. |

## Fork surgery

Coupling was measured with `Cargo.toml` dependents. Crates nothing core depends on are deleted;
crates threaded through `editor`/`project`/`workspace` are reduced to no-op stubs so upstream
merges stay tractable.

- **Delete** (leaf or near-leaf): `agent*`, `acp_*`, `ai_onboarding`, `language_model*`,
  `language_models*`, every LLM provider crate (`anthropic`, `open_ai`, `google_ai`, `bedrock`,
  `ollama`, `lmstudio`, `mistral`, `deepseek`, `x_ai*`, `open_router`, `opencode`, `llama_cpp`,
  `codestral`, `openai_subscribed`), `copilot*`, `edit_prediction*`, `zeta_prompt`, `web_search*`,
  `prompt_store`, `collab`, `collab_ui`, `call`, `channel`, `livekit_*`, `cloud_llm_client`,
  `language_models_cloud`, `feedback`, eval/benchmark crates for removed features.
  `language_model` alone has 24 dependents (e.g. git commit-message generation), so each call site
  is removed, not stubbed.
- **Stub** (deeply wired): `client` (39 dependents) keeps its API surface but no sign-in, no
  zed.dev RPC. `telemetry` (34 dependents) becomes a no-op. The extension registry stays reachable
  (it is a public HTTP API), so extensions keep working.
- **Repoint / disable**: `auto_update` (off until we have our own update feed), branding, bundle
  identifier, app name, icons, default settings, onboarding.
- **License**: Zed's editor crates are GPL-3.0-or-later, so RezStudio is GPL-3.0-or-later.

### What M0 actually did

- Deleted ~70 crates (agents, LLM providers, Copilot, edit prediction providers, collab, calls,
  channels, livekit, audio, feedback). `Cargo.lock` lost 336 packages, including WebRTC.
- Kept two AI-adjacent pieces on purpose, because removing them would mean permanent conflicts in
  Zed's busiest crates: `edit_prediction_types` (a provider-less interface `editor` depends on;
  nothing implements it) and the agent settings schema in `settings_content` (read by `project`).
  `disable_ai` defaults to `true`, which hides any leftover affordance gated on it.
- `client` is inert rather than deleted: no sign-in UI, no LLM tokens, and `TelemetrySettings`
  forces metrics and diagnostics off. `ReleaseChannel::poll_for_updates` returns `false`, since
  Zed's update server would offer to replace RezStudio with Zed.
- Identity: app name `RezStudio`, binary `rezstudio`, bundle id `io.github.robicruzo.RezStudio`
  (`-Dev`, `-Preview`, `-Nightly` per channel), URL scheme `rezstudio://`, CLI `rezstudio`.
  Config lives in `~/.config/rezstudio`, data in `~/Library/Application Support/RezStudio`, the
  single-instance port range and SSH server directory differ from Zed's, so both apps coexist.
- Project-local settings still live in `.zed/`, so projects shared with Zed users keep working.
- The internal version stays at Zed's (`1.27.0`) because the extension registry uses it to pick
  compatible extensions.
- `script/bundle-rezstudio` builds an ad-hoc-signed `RezStudio.app` (optionally a DMG);
  `.github/workflows/rezstudio.yml` runs fmt, clippy and tests on Linux and builds the app on a
  macOS arm64 runner.
- Placeholder icon and logo; final artwork is still open.

## Architecture

```
┌──────────────────────────────────── RezStudio (GPUI) ───────────────────────────────────┐
│ Source editor        │ Console               │ Environment · History                    │
│ .R .py .Rmd .qmd .typ│ (per runtime)         │ Plots · Packages · Help · Viewer · Files │
├──────────────────────┴───────────┬───────────┴──────────────────────────────────────────┤
│ runtime: discovers interpreters, launches/supervises kernels, owns one session per      │
│ language per project, routes execute requests, and multiplexes comms to panes           │
├──────────────────────────────────┴──────────────────────────────────────────────────────┤
│ Jupyter protocol (runtimelib / jupyter-protocol, already in tree, comm messages supported)│
├─────────────────────────────────────────────────────────────────────────────────────────┤
│ Ark (R)            │ ipykernel + rezstudio add-on (Python) │ tinymist + embedded typst  │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

Panes never talk to a kernel directly. They subscribe to a `RuntimeSession` entity and its
typed comm channels. Because Python implements the same comms as Ark, every pane works for both
languages without language-specific code paths beyond formatting.

### New crates

| Crate | Responsibility |
|---|---|
| `runtime` | Interpreter discovery (R: rig, Homebrew, CRAN `R.framework`, `renv`; Python: uv, venv, conda, pyenv — reusing Zed's `toolchain` crate), kernel launch and supervision, busy/idle state, interrupt/restart, a typed client for each comm (variables, plot, data explorer, help, UI), and the active-session-per-language registry. Builds on the libraries Zed's existing `repl` crate uses and reuses `repl::outputs` renderers. |
| `console` | The console pane: an editor-backed input (completions, multi-line, syntax highlighting) over a virtualised scrollback of rich outputs (text, errors with tracebacks, images, tables, HTML). Handles history search, Esc to interrupt, prompt/continuation state, and `readline()`/`input()` requests. One tab per running session. |
| `variables_panel` | Environment pane: live object tree (name, type, size, value preview), lazy expansion for lists, environments, S4/R6 and Python objects; filters; clear/remove; click a data frame to open the data viewer. |
| `data_explorer` | `View()` / data-frame grid. Pages rows from the kernel on demand (never ships the whole frame), sorts and filters on the kernel side, shows column profiles (type, NA count, histogram). Reuses rendering from `tabular_data_preview`; the same grid also opens CSV/Parquet files directly. |
| `plots_panel` | Plot history with thumbnails; the kernel re-renders at the pane's exact pixel size and scale on resize; zoom window; export PNG/SVG/PDF; copy to clipboard. |
| `packages_panel` | Installed and loaded packages with versions, attach/detach, install/update/remove (runs in the console so output is visible), `renv` awareness. **Search** covers CRAN, Bioconductor and R-universe through a locally cached index refreshed in the background, so fuzzy search is instant and works offline. Python: the same pane over pip/uv metadata. |
| `help_panel` | `?topic` and help search; back/forward history; F1 on a symbol. Renders Ark's help server pages in the web view. |
| `web_view` | macOS: WKWebView hosted as a child view of the GPUI window, positioned to a pane's bounds. Used by the Viewer (htmlwidgets, Shiny, Quarto preview) and Help. Linux: opens the system browser. |
| `notebook_chunks` | Rmd/Qmd chunk detection and execution: run chunk, run all above, chunk gutter controls, and inline output blocks rendered under each chunk (same output renderers as the console). Output is kept with the session, not written into the source. |
| `typst_preview` | Embeds the `typst` compiler: incremental recompiles on edit, renders only visible pages natively, and jumps both ways between source and preview. Version-pinned to match the bundled tinymist. |

### Changes to existing crates (kept small for mergeability)

- `languages`: built-in R, Quarto/R Markdown (markdown with code-chunk injections) and Typst
  grammars, so first-class languages don't depend on an external extension. Python is already
  built in.
- `workspace` / `zed`: default four-quadrant layout (source top-left, console bottom-left,
  environment/history top-right, files/plots/packages/help/viewer bottom-right), using Zed's docks.
- `assets/keymaps`: RStudio-style default keymap (Cmd+Enter, Cmd+Shift+Enter, Cmd+Shift+S,
  Cmd+Shift+M for the pipe, Opt+- for `<-`, Cmd+Opt+I for a new chunk, Ctrl+1/2 to focus the
  source or console). Zed's keymap remains available as an alternative.
- `repl`: existing inline-REPL entry points are hidden/replaced by `console` + `notebook_chunks`;
  only its output renderers are reused.

### Projects

`.Rproj` files open as projects (their settings map onto ours). Per-project interpreter choice
lives in `.zed/settings.json`. `renv` and Python virtual environments are detected automatically.
Working directory is the project root, as in RStudio.

### Compatibility

Ark implements much of `rstudioapi` (for example `getActiveDocumentContext`, `insertText`,
`navigateToFile`) by sending UI comm requests to the frontend. RezStudio answers those requests so
packages and addins that call `rstudioapi` keep working.

## Milestones

| # | Goal | Done when |
|---|---|---|
| M0 | Fork surgery and rebrand | AI/collab/cloud crates gone, `client`/`telemetry` stubbed, app renamed, builds green on Linux CI and macOS CI. |
| M1 | `runtime` + `console` for R | Ark bundled; R discovered; Cmd+Enter / Cmd+Shift+S run code; interrupt and restart work; rich output in the console; Ark LSP completions in the editor and console. |
| M2 | Environment + data viewer | Live variables; `View(df)` opens the paged grid on a 10M-row frame without lag. |
| M3 | Plots | Plot history, resize re-render, export. |
| M4 | Packages + search | Installed/loaded list; instant offline search across CRAN, Bioconductor, R-universe. |
| M5 | Rmd/Quarto inline chunks | Chunk execution with inline output. |
| M6 | Help + Viewer | WKWebView embedded; help, htmlwidgets, Shiny. |
| M7 | Python parity | ipykernel add-on implements the comms; all panes work for Python. |
| M8 | Typst native preview | Live preview with source ↔ preview jumping. |
| M9 | Debugger | Ark DAP through Zed's `debugger_ui`: breakpoints, `browser()`, stepping. |

### M1 plan

Decisions: Ark's LSP is part of M1; plots are accepted but not shown until M3; the R session
starts when a project opens; Ark 0.1.252 is bundled in the app.

Findings that shape the plan:

- `jupyter-protocol` already models comm and stdin messages, and `repl::kernels` already runs all
  five channels and routes interrupts to the control channel, which is what Ark expects.
- Ark is launched as `ark --connection_file F --session-mode console`, with `R_HOME` in the
  environment (otherwise it runs `R RHOME` from `PATH`).
- Ark's Jupyter `complete_request` returns nothing; completions only come from its LSP. The LSP
  runs inside the R process and is started by opening a `positron.lsp` comm with
  `{"ip_address": "127.0.0.1"}`, which replies with a TCP port. Zed's `lsp` crate only speaks
  stdio, so a bridge is needed.
- Comm target names are `positron.<name>` (`ui`, `variables`, `help`, `lsp`, `dap`, `plot`,
  `dataViewer`). Ark opens `plot` and `dataViewer` itself; the frontend opens the rest.
- In console mode Ark sends plots over `positron.plot` comms rather than `display_data`.

Steps:

1. **R language.** Built-in R support in `languages`/`grammars` from `tree-sitter-r` (MIT):
   highlighting, brackets, indentation, outline, for `.R`, `.r`, `.Rprofile`.
2. **`runtime`.** Discover R (CRAN `R.framework` versions, rig, Homebrew, `PATH`), with a
   per-project choice in `.zed/settings.json`. Locate Ark (bundled, then a setting). Launch and
   supervise one R session per window, reusing `repl::kernels::start_kernel_tasks`. Track
   busy/idle, prompt and working directory; interrupt, restart, shut down on window close. Open
   `positron.ui` and answer every frontend request with a safe default so R never blocks; open
   links (including help) in the system browser until M6. Accept `positron.plot` comms and drop
   them until M3. Comm types are ported from Ark's MIT-licensed generated Rust, pinned to the
   bundled release, with contract tests.
3. **LSP bridge.** A hidden `rezstudio --lsp-bridge <port>` mode pipes stdio to Ark's LSP TCP port;
   an R `LspAdapter` launches it once the session reports its port.
4. **`console`.** A bottom-dock panel. Scrollback is a virtualised list of echoed input, ANSI
   stream text, errors with tracebacks and rich outputs (`repl::outputs`, made public). Input is
   an auto-height R editor: Enter sends when Ark's `is_complete_request` says the code is
   complete, otherwise inserts a continuation line; Up/Down history, Ctrl+R search, Esc/Ctrl+C
   interrupt, Ctrl+L clear; `readline()`/`menu()` answered inline. A header shows the R version,
   working directory, busy state and a restart button.
5. **Run from editor.** Cmd+Enter runs the selection or the statement under the cursor (found
   with tree-sitter) and moves to the next statement; Cmd+Shift+Enter sources with echo;
   Cmd+Shift+S sources; Cmd+Shift+F10 restarts; Ctrl+1/Ctrl+2 focus source/console; Opt+- and
   Cmd+Shift+M insert `<-` and the pipe.
6. **Bundling.** `script/bundle-rezstudio` downloads the pinned Ark release, checks its SHA-256,
   places it in `Contents/MacOS/ark`, signs it and ships Ark's `LICENSE` and `NOTICE`.
7. **Tests.** Linux: unit tests against a scripted fake kernel. macOS CI: a job installs R and
   drives the real Ark through execute, error, interrupt (`Sys.sleep(10)`), `readline()` and
   restart.

Main risks: console completions need Ark's LSP to treat the console input as a virtual document;
Ark's comm protocol may change between releases, which the pin and contract tests contain.

## Risks

- **Ark's comm protocol is Positron-driven and may change.** Mitigation: bundle a pinned Ark
  release, port comm types from Ark's own MIT-licensed Rust definitions (not Positron's
  Elastic-licensed sources), and add contract tests per comm.
- **WKWebView inside GPUI.** A native child view draws above GPUI's Metal layer, so GPUI popovers
  and menus cannot overlap it. Mitigation: confine web views to their own panes and hide or
  snapshot them while overlays are open.
- **Development happens on Linux; shipped builds are macOS only.** macOS-only code must be thin
  and covered by macOS CI.
- **Upstream drift.** Mitigation: keep R features in new crates, make small edits to Zed crates,
  and merge upstream on a regular cadence.

## Open questions (not blocking M0)

- Final app name, bundle identifier and icon.
- Distribution: signing/notarization requires an Apple Developer account; the update feed
  (e.g. Sparkle or a fork of Zed's updater) is undecided.
