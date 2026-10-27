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
  environment/history top-right, files/plots/packages/help/viewer bottom-right). Zed's docks
  show one panel at a time, switched from the status bar, so this needs the dock changes M7
  plans.
- `assets/settings`: lines soft-wrap at the editor's width by default (Zed's default is not
  to wrap), so code stays readable when the panes beside the editor are widened. Users can
  turn it off with `"soft_wrap": "none"`, globally or per language. Tests keep Zed's default,
  as `settings::test_settings` does for AI and telemetry, since Zed's editor and search tests
  assume lines don't wrap.
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
| M1 | `runtime` + `console` for R *(done)* | Ark bundled; R discovered; Cmd+Enter / Cmd+Shift+S run code; interrupt and restart work; rich output in the console; Ark LSP completions in the editor and console. |
| M2 | Environment + data viewer *(done)* | Live variables; `View(df)` opens the paged grid on a 10M-row frame without lag. |
| M3 | Plots *(done)* | Plot history, resize re-render, export. |
| M4 | Packages + search *(done)* | Installed/loaded list; instant offline search across CRAN, Bioconductor, R-universe. |
| M5 | Rmd/Quarto inline chunks *(done)* | Chunk execution with inline output. |
| M6 | Help + Viewer *(done)* | WKWebView embedded; help, htmlwidgets, Shiny. |
| M7 | RStudio layout *(done)* | Two stacked, tabbed panes in the right dock, with tabs dragged between them; Console, Terminal and Debug as tabs; an outline sidebar from an editor button. |
| M8 | Documents and Environment *(done)* | Render button for Quarto and R Markdown with output in the Viewer; Environment pane with RStudio's environment menu, grouping, memory use, workspace save/load and Import Dataset. |
| M9 | Dark plots and Viewer | A moon toggle in the Plots and Viewer toolbars darkens plots and pages to match a dark theme; Copy, Save and Open in Browser keep the originals. |
| M10 | Python parity | ipykernel add-on implements the comms; all panes work for Python. |
| M11 | Typst native preview | Live preview with source ↔ preview jumping. |
| M12 | Debugger | Ark DAP through Zed's `debugger_ui`: breakpoints, `browser()`, stepping. |

### M1 plan (done)

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
2. **`runtime`.** *(Done.)* R installations (CRAN/rig `R.framework` versions, Homebrew, rig on
   Linux, `PATH`, `R_HOME`) are Zed toolchains for the R language (`languages/src/r.rs`), so the
   status bar's toolchain picker chooses R per project and remembers it; the default is the `R`
   on `PATH`. Ark is found through `REZSTUDIO_ARK`, the app bundle, `PATH`, then (debug builds
   only) Positron's copy. One `RSession` per window starts when its first local folder opens, once
   that folder is trusted (R runs the project's `.Rprofile`, so it waits for Zed's Restricted
   Mode like language servers do), and stops gracefully when the window closes. Ark runs under a small shell supervisor that stops it
   if RezStudio dies, since Ark doesn't watch its parent. Ark can ignore SIGTERM, which left R
   running after RezStudio quit, so the supervisor kills it 5 seconds after SIGTERM if it's
   still there. An execution is finished only once both
   its `execute_reply` (shell) and iopub `idle` have arrived, because output can trail the reply.
   Ark's `rpc_request`s on stdin are always answered: questions and dialogs use native prompts,
   the workspace folder is the project root, and unsupported ones return a JSON-RPC error. Links
   (including help) open in the system browser until M6; `positron.plot` comms are dropped until
   M3. Comm types are ported from Ark 0.1.252's MIT-licensed generated Rust.
3. **LSP bridge.** *(Done.)* The hidden `rezstudio --lsp-bridge <port>` mode copies stdio to Ark's
   LSP TCP port, like Zed's `--askpass` netcat mode. The `ark` language server adapter (registered
   by `runtime` for R) waits for the window's session to report its port, then launches the
   bridge; when R restarts on a new port, `ark` is restarted for the project's open buffers.
   Ark serves one connection per LSP comm, and Zed can start a new `ark` at any time, as when
   the project's toolchain changes. So each port goes to one server: a second server waits
   while the session closes the LSP comm and opens a new one, and takes the new port Ark
   reports, as Positron does when it restarts its language server. Only a restart of R
   restarts the servers, which avoids a loop of new ports and restarts.
4. **`r_console`.** *(Done.)* The crate is `r_console` because the
   workspace already depends on the external `console` crate and the debugger uses the
   `console::` action namespace. A bottom-dock panel, open by default next to the terminal like
   RStudio's Console/Terminal tabs. Scrollback is one display-only terminal (Zed's
   `TerminalView`), not `repl::outputs`: R output is a text stream, and a terminal gets ANSI
   colors, `\r` progress bars, long scrollback, selection and links right, while `repl`'s
   per-output terminals cap each output at a few dozen lines. Input is an auto-height R editor:
   Enter asks Ark's `is_complete_request` (skipped while R is busy, since the check would queue
   behind the running code) and either runs the code or continues it on a new line; Up/Down
   browse history at the first/last line; Esc interrupts when busy and otherwise clears; Ctrl+L
   clears; `readline()`/`menu()` prompts take over the input row. Errors and stderr are red,
   echoed input blue. The header shows the R version, working directory, a busy spinner and
   interrupt/clear/restart buttons. Completions in the input come from the project's `ark`
   language server through Zed's existing connection (Ark serves one client): the input is a
   document that exists only in the server, opened on first use and updated before each
   request with ever-increasing versions (Ark panics on a version that goes backwards).
   Completions are only requested while R is idle, with a 2 s timeout, because Ark computes
   them on R's thread and a request during a long computation would stall its whole language
   server. Ark's snippet items (`mean($0)`) insert with the cursor inside the parentheses, and
   help is fetched with `completionItem/resolve`. Tab opens completions; the console's Enter,
   arrow and Escape bindings step aside while the completion menu is open.
5. **Run from editor.** *(Done.)* In `.R`/`.r` editors: Cmd+Enter runs the selection, or the
   whole top-level statement under the cursor (found in the tree-sitter tree, so multi-line calls
   and function definitions run as one; statements sharing a line run together; a statement with
   a syntax error runs only the cursor's line) and moves to the next statement; on a blank or
   comment line it only moves. Cmd+Shift+Enter saves and runs `source(file, echo = TRUE)`;
   Cmd+Shift+S saves and runs `source(file)` (replacing Save As in R files). Opt+- and
   Cmd+Shift+M insert `<-` and `|>` in R editors and the console input. Ctrl+1/Ctrl+2 focus the
   editor/console (replacing Zed's tab switching on those keys), Cmd+Shift+F10 restarts R.
6. **Bundling.** *(Done.)* `script/bundle-rezstudio` downloads the Ark release matching
   `runtime::ARK_VERSION` (cached under `target/rezstudio-downloads/`), checks it against the
   release's published SHA-256 (the script refuses to run if the version and checksums drift
   apart), and installs it as `Contents/MacOS/ark`, which the runtime finds before any other
   Ark. It's signed like the rest of the bundle, with `crates/zed/resources/ark.entitlements`
   (library validation off and `DYLD_*` variables allowed, which R needs under the hardened
   runtime), and Ark's `LICENSE` and `NOTICE` ship in `Contents/Resources/ark/`. macOS CI checks
   the bundled Ark's version, signature and license files. Bundling git like Zed does is
   separate: git is GPL-2.0-only and its archive has no license text, so it needs its own
   license and source notice first.
7. **Tests.** *(Done.)* Linux CI runs the unit tests: sessions driven by scripted kernel
   messages, the console against a scripted session, R discovery, the bridge and the
   supervisor. A macOS CI job installs R from CRAN, fetches the pinned Ark with
   `script/fetch-ark` (shared with the bundle script, so both use the same version and
   checksums), and runs the ignored real-Ark test: startup, UI and LSP comms, output, errors,
   `is_complete`, `readline()`, interrupt and recovery, LSP completions, and shutdown. Locally:
   `REZSTUDIO_ARK=$PWD/$(script/fetch-ark)/ark cargo test -p runtime -- --include-ignored`.

Main risks: console completions need Ark's LSP to treat the console input as a virtual document;
Ark's comm protocol may change between releases, which the pin and contract tests contain.
Both are now handled: completions use an in-server document, and macOS CI runs the real-Ark
test on every push.

### M2 plan (done)

Decisions: the Environment pane goes in the right dock (files stay left, the console bottom);
`View(df)` opens the data viewer as an editor tab beside scripts, as in RStudio; column headers
show the type and the number of missing values (summary statistics and histograms come later);
clicking a header sorts, done by R (row filters come later).

Findings that shape the plan:

- Both panes talk to Ark over comms with request/reply RPCs: the frontend sends a `comm_msg`
  whose data carries `"id"` plus `method`/`params`, and Ark answers with a `comm_msg` on iopub
  whose parent header is the request and whose data is `{"method", "result"}` or `{"error"}`.
  The runtime only answers Ark's own requests so far, so it needs the frontend side.
- `positron.variables` is opened by the frontend. Ark then sends a `refresh` event with every
  variable, and an `update` event (assigned, removed) after each execution. Each variable has
  an access key, display name/type/value, size, length, `has_children` and `has_viewer`;
  `inspect(path)` lists children, `view(path)` opens a data explorer, and `delete`, `clear` and
  `clipboard_format` do what they say.
- `positron.dataExplorer` is opened by Ark, with `{"title", "inline_only"}`, for `View(df)` or a
  variables `view`. `get_state` gives the shape, `get_schema` the columns,
  `get_data_values` formatted values for a range of rows per column, `get_row_labels` the row
  names, `set_sort_columns` sorts in R, and `get_column_profiles` (answered asynchronously by a
  `return_column_profiles` event) gives null counts. Ark sends `data_update`/`schema_update`
  when the data frame changes in R. The whole frame never leaves R.
- Zed's `ui::Table` renders only visible rows (`uniform_list`) and has resizable columns; it's
  what `tabular_data_preview` uses, and the viewer can drive it with rows fetched on demand.

Steps:

1. **Runtime comm RPCs.** *(Done.)* `RSession::comm_request(comm_id, method, params)` returning the result
   (or Ark's error), matched by parent header, with a timeout. Open `positron.variables` on
   connect; surface variables events and Ark-opened data explorer comms as session events. Port
   the variables and data explorer types from Ark 0.1.252's MIT-licensed generated Rust.
   Methods without parameters must omit `params` entirely; Ark rejects `"params": {}`. Against
   real Ark and a 10M-row data frame, `get_state` plus `get_schema` take about 160 ms and a
   100-row page from the middle about 6 ms.
2. **`variables_panel`.** *(Done.)* A right-dock Environment pane: a flattened tree in a `uniform_list`
   with Name, Type, Size and Value columns, expanding lists, environments, S4/R6 objects and
   data frames lazily with `inspect`; live updates from `update` events; a filter box; a button
   to open data frames in the viewer; delete and clear (with confirmation). Top-level variables
   are sorted by name, children keep R's order, and expanded variables stay expanded (and are
   re-inspected) when R changes them. Rows are laid out by hand rather than with `ListItem`,
   whose disclosure arrow sits outside unindented rows and whose hover slot shifts columns.
3. **`data_explorer`.** *(Done.)* An editor tab per explorer comm. A `ui::Table` asks for the
   visible rows in 100-row blocks (`get_data_values`, at most 4 in flight and 64 cached, least
   recently shown dropped first), so a 10M-row frame only ever loads what's on screen; row names from `get_row_labels`;
   headers with the column name, type and NA count (from `get_column_profiles`, answered by a
   `return_column_profiles` event); click to cycle ascending/descending/original order through
   `set_sort_columns`; refreshes on `data_update`/`schema_update`; closing the tab closes the
   comm, and a tab whose R stops says it's no longer live. Wide frames page through 50 columns
   at a time, as RStudio does. `View()` again on the same data frame reuses its tab. No rows
   are fetched while a reload or sort is pending, since they'd come back stale. The cache
   can't drop the blocks farthest from view: `uniform_list` renders row 0 every frame to
   measure row height, which would push out the rows on screen and refetch them forever. Numbers use
   Ark's formatting (as in Positron), so `0.6260` and `1.59` can sit in one column.
4. **Tests.** *(Done.)* Unit tests drive both panes with scripted comm messages: the tree's
   refresh, update, expansion and filter; the viewer opening from a backend comm, fetching one
   block from the middle of 10M rows, the four-fetch cap, sorting, `data_update`, tab reuse,
   closing, and R stopping. The real-Ark test (macOS CI) covers variables refresh, update and
   inspect after an assignment, and a `View()` of a 10M-row data frame with timed page fetches,
   sorting and NA counts.

Done when: assignments in the console appear in the Environment pane immediately, children
expand, and `View()` of a 10M-row data frame opens and scrolls without visible lag. All met:
checked by hand down to the 10,000,000th row, with CI green on every job.

### M3 plan (done)

Decisions: the Plots pane goes in the right dock beside Environment, one shown at a time like
RStudio's tabs; a new plot brings Plots to the front, opening the dock if needed, without
taking focus from the editor or console; history is RStudio's back/forward arrows with
"3 / 7", delete and clear (thumbnails may come later); M3 includes saving as PNG, SVG or PDF at
a chosen size, copying to the clipboard, and a zoom window.

Findings that shape the plan (Ark 0.1.252, `plot_comm.rs` and `graphics_device.rs`):

- Ark uses dynamic plots only in console mode with the UI comm connected, which RezStudio
  already does. Each new plot gets its own `positron.plot` comm opened by Ark, whose open
  data carries a `pre_render`: a PNG already drawn at the default size, so it can show at once.
- The default is 640×400 at pixel ratio 1 until the frontend sends the UI comm notification
  `did_change_plots_render_settings` `{settings: {size, pixel_ratio, format}}` (a `comm_msg`
  without an `id`). Sending the pane's size and scale on every resize makes new plots arrive
  at the right size, with no second render.
- `render {size?, pixel_ratio, format}` redraws a plot from R's recording at any size, as `png`,
  `svg`, `pdf`, `jpeg` or `tiff`. Sizes are logical pixels; `pixel_ratio` scales them. The reply
  is `{data, mime_type, settings}`, with `data` in base64 **without padding**. `get_metadata`
  gives a name (like `ggplot2 1`), kind, and the code that drew it.
- Drawing more on the current plot (`lines()` after `plot()`) sends an `update` event with a
  fresh pre-render; `show` asks the frontend to bring a plot to the front.
- Closing a plot's comm deletes its recording in R. After R restarts, old plots can't be
  redrawn, but their last image can still be shown.
- Ark renders on R's thread, so renders wait while R is busy. Ark uses `ragg` (and `svglite`
  for SVG) when installed, and R's own devices otherwise.
- GPUI shows PNG and SVG with `img()` from bytes, and `ClipboardItem::new_image` copies images.

Steps:

1. **Runtime plot comm.** *(Done.)* Port the plot comm types. New session events: a plot opened
   (comm id, pre-render), updated, asked to be shown, and closed (including when R stops).
   `RSession::render_plot` and `plot_metadata` wrap the RPCs. `set_plot_render_settings`
   sends `did_change_plots_render_settings` on the UI comm, only when the settings change. The
   session keeps them and sends them again after `frontend_ready` when R restarts. The
   real-Ark test checks that:
   - `plot(1:10)` opens a comm whose pre-render is 1000×600 after asking for 500×300 at 2x;
   - `render` at 800×600 and 2x returns a 1600×1200 PNG, in about 35 ms;
   - SVG and PDF render;
   - `get_metadata` returns the code;
   - `lines()` sends `update` with a new pre-render;
   - after a plot's comm closes, the next plot gets a new one.

   `parse_method` now keeps an empty `params` for methods that take parameters, such as
   `show`, and drops it only for methods without any.
2. **`plots_panel`.** *(Done.)* A right-dock Plots pane. It shows the current plot fitted to
   the pane, using the pre-render at once and then a render at the pane's exact size and scale.
   Resizing re-renders 150 ms after it stops and sends the new settings to Ark. Back/forward
   arrows with "3 / 7", remove (closes the comm) and clear all (with confirmation). A new plot,
   or `show`, makes it current and brings the pane to the front without taking focus, through
   `Workspace::open_panel`; that's deferred, because activating the panel updates it. Plots
   from a stopped R stay as images, marked "Not live".
   - **Measuring:** the pane measures its plot area with a `canvas` that updates the view
     while painting.
   - **Decoding:** PNGs are decoded in the background into a `RenderImage` and shown with
     `ImageSource::Render`, so swapping images never shows a blank frame.
   - **Freeing images:** replaced and removed images are freed with `App::drop_image`, since
     the GPU atlas keeps them otherwise.
   - **Ordering:** renders are numbered, so one that finishes late can't replace a newer one.
   - **Settled sizes only:** plots are drawn only at a size the pane has settled on, never
     at the size of the moment. Zed animates dock resizes, and a render or decode that
     finished mid-drag used to start another at whatever width the dock had right then.
     That's how a fast drag asked R for a plot 65 px wide.
   - **Minimum size:** below 120 px, R fails with "invalid graphics state", so the pane
     shows the last image scaled instead.
   - **Errors:** a failed render keeps the last image and shows one line over the plot,
     with the full text in a tooltip. Showing R's whole error and backtrace above the plot
     used to shrink the plot area to 0 px tall, which stopped all redraws.
   - **Checked live:** `plot()` and a ggplot2 plot drawn after startup appear at once. The
     first arrives at Ark's 640×400 default and is redrawn to fit; the second arrives already
     fitted.
3. **Export, copy and zoom.** *(Done.)*
   - **Save:** opens a small dialog for format (PNG/SVG/PDF), width and height, then a save
     dialog, and writes R's render.
     - The dialog starts from the pane's size, or from the last format and size used.
     - Sizes are checked before anything is drawn.
     - A PNG is drawn at pixel ratio 1, so the file has exactly the pixels asked for.
     - A name without an extension gets the format's extension.
     - Save is off for plots that aren't live.
   - **Copy:** puts the image shown on the clipboard. It's R's PNG at the pane's size and
     scale, kept beside the decoded image, so copying needs no new render.
   - **Zoom:** opens the plot in its own window, starting from the pane's image. It's
     redrawn to fit once a resize settles, and again when R updates the plot. The title is
     the plot's name from `get_metadata`. Esc or Cmd-W closes it.
   - **SVG without svglite:** Ark draws SVGs with svglite when it's installed, and falls back
     to R's own `svg()` otherwise. On CRAN R for macOS, `svg()` needs XQuartz and silently
     writes nothing, so Ark fails with "No such file or directory". Saving as SVG then says
     to install svglite, and the macOS CI job installs it so the real-Ark test checks a
     real SVG.
   - **Shared code:** the pane and the zoom window share `fitting.rs`: measuring, settling,
     the minimum size, decoding and the one-line error.
   - **Checked live:** Copy put the plot's PNG on the macOS clipboard. The zoom window opened.
     While the main window was in macOS full screen, its bounds looked odd, which is worth
     checking by hand.
4. **Tests.** *(Done.)* Unit tests drive the pane with scripted plot comms:
   - history, navigation, remove and clear;
   - opening without taking focus;
   - window, dock and mouse-drag resizes, drawing only the size it settles at;
   - renders that land mid-resize;
   - failed renders and the minimum size;
   - updates, `show` and R stopping;
   - copy, the save dialog and the saved file, and the SVG hint;
   - the zoom window.

   The real-Ark test (macOS CI, with svglite installed) covers pre-renders at the pane's size,
   PNG/SVG/PDF renders, metadata, updates, and closing a plot.

Done when: `plot(1:10)` and a ggplot2 plot appear in Plots straight away and sharp on a Retina
screen; resizing the dock redraws them at the new size; the arrows step through history;
saved PNG, SVG and PDF files open in Preview; a copied plot pastes into another app; and the
zoom window redraws as it's resized. All met: checked by hand, including dragging the dock,
save, copy and zoom, with CI green on every job.

### M4 plan (done)

Decisions:
- The Packages pane goes in the right dock beside Environment and Plots, one shown at a
  time.
- In a project with an active renv library, Install and Remove use `renv::install()` and
  `renv::remove()`, and the pane shows an renv badge.
- Search uses an offline index of CRAN, Bioconductor and R-universe, refreshed daily in the
  background.
- M4 includes updates: installed packages with a newer version are marked, with Update and
  Update All.
- Install, Remove, Update, and attaching or detaching all run in the console, so their
  output is visible.

Findings that shape the plan (Ark 0.1.252's `packages_pane.R` and `ui_comm.rs`, and the
indexes' public APIs):

- **R functions for the pane:** Ark already has them, reached through the UI comm's
  `call_method` RPC (`{method, params}`), which runs `.ps.rpc.<method>` in R and returns
  JSON:
  - `pkg_list` gives installed packages (name, version, whether attached, description,
    URL). It takes under 50 ms on a library of 250 packages, so it can rerun after every
    execution.
  - `pkg_detail(name)` gives the title, maintainer, license and repository.
  - `pkg_outdated` gives packages with a newer version in the session's repositories. It
    asks those repositories, so it needs the network.
  - `evaluate_code` runs a line silently and returns its JSON value, for checking whether
    renv is active (`RENV_PROJECT`).
- **Ark's own search** (`pkg_search`) asks `pak` or `available.packages()` on every query.
  That needs the network and is too slow per keystroke, so search uses our own index.
- **The index:** r-universe's `api/search?q=` with an empty query lists every indexed
  package, about 31,500, most used first, in pages of `limit`/`skip`. It covers CRAN
  (universe `cran`, down to obscure packages), Bioconductor (`bioc`) and other universes.
  Each record has the name, title, description, universe, reverse-dependency count and
  stars. There's no version, and the `fields` filter is ignored, so a full listing is
  about 36 MB of JSON, much less gzipped.
- **Where a package installs from:** CRAN's `src/contrib/PACKAGES.gz` (2 MB) lists every
  CRAN package and version. A package in it installs with `install.packages()`. One whose
  universe is `bioc` installs with `BiocManager::install()`. Any other comes from its
  universe, `https://<universe>.r-universe.dev`, ahead of the session's repositories.
- **Running code from the pane:** `ConsolePanel::run_from_editor` runs code in the console
  as Cmd+Enter does, showing the code and its output.

Steps:

1. **Runtime calls.** *(Done.)* `RSession::call_method` and `evaluate_code` on the UI comm,
   with typed wrappers: `installed_packages`, `package_detail` (`None` when the package isn't
   installed) and `outdated_packages`.
   - **Without R:** calls fail at once instead of waiting.
   - **Empty lists:** R's empty list can arrive as `[]`, `{}` or `null`, so list replies
     accept all three.
   - **`pkg_outdated`:** its timeout is three minutes, since it asks every repository.
   - **Checked against real Ark:**
     - 247 installed packages listed in about 115 ms, including the round trip;
     - `stats` attached and `tools` not, until `library(tools)`;
     - `pkg_detail("stats")` has a title, and `pkg_detail` of a missing package is `null`;
     - `evaluate_code` returns a value;
     - an unknown method returns Ark's "No such method" error.
2. **`package_index`.** *(Done.)* Downloads the r-universe listing and CRAN's `PACKAGES` in
   the background, at most daily, and keeps a copy in RezStudio's data directory
   (`package-index.json`). It checks hourly whether the copy is due, and retries then after
   a failure.
   - **Download:** about 32,000 packages in 5 to 15 s, in pages of 5,000, gzipped. Zed's
     HTTP client doesn't decompress, so the crate decodes gzip itself.
   - **Copy on disk:** 15 MB, with descriptions cut to 500 characters. It's written to a
     `.partial` file and renamed, so a crash never leaves a broken copy.
   - **Failures:**
     - a failed download keeps the last copy;
     - a listing under 10,000 packages counts as a failure;
     - a broken copy is ignored and replaced.
   - **Ranking:** exact name, name prefix, name contains, then letters in order (for typos
     and abbreviations, closest first), then every word in the title, then in the
     description. Within each, the most used wins.
   - **Names ignore punctuation,** so "data table" finds `data.table` first.
   - **Where it installs from:** a package on CRAN (in `PACKAGES`) installs from CRAN, one
     in `bioc` from Bioconductor, and anything else from its universe.
   - **Speed:** searching all 32,000 packages takes 3 to 5 ms per keystroke in a release
     build, off the main thread.
   - **Checked live:** an ignored test downloads the real index and searches it. "ggpl"
     finds ggplot2 first, "bayesian regression" finds brms, BART and rstanarm, and "sf"
     finds sf. The app starts the index in step 3, with the pane.
3. **`packages_panel`.** *(Done.)* A right-dock Packages pane:
   - **Filter box:** filters the installed list by name or description as you type, and
     lists matching packages that aren't installed from the index below it, with where each
     comes from (CRAN, Bioconductor or `<universe>.r-universe.dev`).
   - **Rows:** an attached checkbox, the name, the version with an update badge
     (`→ 3.7.0`) and the description. Clicking a row expands its details: title,
     maintainer, license, where it's from, published date and a website link.
   - **Actions:** Install (on hover, in search results), Remove (asks first), Update, Update
     All (in the toolbar, with the count) and Refresh. Refresh lists again, checks for
     updates, and retries a failed index download.
   - **Running actions:** the R code goes through `ConsolePanel::run_from_editor` and the
     console comes forward, so the code and its output are visible. Without a console
     panel, the session runs it directly.
   - **Install commands:**
     - CRAN: `install.packages()`;
     - Bioconductor: BiocManager, installing BiocManager first if needed;
     - another universe: `install.packages(repos = c("https://<universe>.r-universe.dev",
       getOption("repos")))`.

     In an renv project they become `renv::install()`, with `bioc::` for Bioconductor, and
     Remove and Update use `renv::remove()` and `renv::update()`.
   - **Refresh:** the list is fetched when the pane is shown, and again whenever R goes
     idle after running code, which catches `library()`, installs and removals from the
     console. A hidden pane asks R nothing and catches up when shown.
   - **Updates:** checked once per session, after updates run from the pane, and on
     Refresh, since `pkg_outdated` needs the network.
   - **Search index:** `packages_panel::init` starts it, so it downloads on the first
     launch. Until then the pane says it's downloading.
   - **Checked live:** the installed list with update badges and "Update All (28)" appeared.
     "ggpl" showed the 5 installed matches, then available ones from CRAN, Bioconductor
     and other universes. `library(tidyverse)` in the console ticked ggplot2's box.
4. **Tests.** *(Done.)* Unit tests drive the pane with scripted comm replies:
   - the installed list, its attached state and updates;
   - refreshing after executions, and not while hidden;
   - details;
   - R stopping;
   - filtering and search;
   - the exact code for attach, detach, install (CRAN, Bioconductor, r-universe), update
     all and remove (after asking);
   - renv's install and remove.

   The index has tests for:
   - downloading, storing and reloading;
   - where packages install from;
   - a failed, too-small or broken download;
   - ranking;
   - search speed on a full-size index;
   - an ignored live download.

   The real-Ark test covers `pkg_list`, `pkg_detail` and `evaluate_code`.

Done when: the installed list matches `installed.packages()`, and the checkbox follows
`library()` and `detach()`. Typing "ggpl" with the network off lists ggplot2 and its
relatives at once. Installing from search runs in the console and the list updates
afterwards. Outdated packages are marked and update from the pane. In an renv project,
Install uses renv. Met with CI green on every job. Checked by hand: the checkbox, Install
from search, details and Update. Search reads only the copy on disk, so it works offline,
and renv's commands are covered by tests; neither was tried by hand.

### M5 plan (done)

Decisions:
- Plots from a chunk appear only under it, sized to the editor; the Plots pane doesn't take
  them.
- Outputs last while the file is open and clear when it closes.
- Chunks run with RStudio's shortcuts:
  - Cmd+Shift+Enter: the chunk under the cursor;
  - Cmd+Alt+P: every chunk above;
  - Cmd+Alt+R: all chunks.

  Each chunk's ```` ```{r} ```` line also has a run button.
- R code in chunks gets completion from Ark's language server.

Findings that shape the plan (Zed's `repl` and Markdown grammar, and Ark 0.1.252):

- **Highlighting:** Zed's Markdown grammar already reads R Markdown fences. For
  ```` ```{r} ```` and ```` ```{r, echo=FALSE} ````, the info string's `language` node is
  `r`, so the Markdown injection picks the R language with no grammar changes. Zed doesn't
  know the `.Rmd` and `.qmd` extensions yet (an M1 test asserts it).
- **Rendering outputs:** Zed's REPL shows outputs under code with an editor block
  (`BlockPlacement::Below`) holding an `ExecutionView`. That view renders plain Jupyter
  messages: streams with ANSI colors, `display_data` (text, tables, images, HTML as
  text), errors with tracebacks, and `clear_output`. `RSession` already emits those per
  execution, as `SessionEvent::Output` with the request id. `ExecutionView` is private to
  the `repl` crate, so it gets exported. The REPL's own kernels and sessions aren't used,
  since chunks must run in the console's R session.
- **Plots:** with the UI comm open, Ark sends plots as `positron.plot` comms, not
  `display_data`. A plot comm's parent header isn't the execute request. But
  `get_metadata`'s `execution_id` is the request's message id (checked against real Ark),
  and executions run in order. So a plot opened while an execution is the oldest pending
  one belongs to it. The session can tag each `PlotOpened` with that request without
  waiting for R, which a `get_metadata` round trip would do mid-execution.
- **Plot size:** Ark's execute request has a `positron` extension (`output_width_px`,
  `output_pixel_ratio`, Quarto's `fig-width`/`fig-height`) that sizes the first render.
  `jupyter-protocol`'s `ExecuteRequest` has no such field, so chunk plots are redrawn at
  the editor's width with `render` instead (about 30 ms). Quarto's `fig-width` can use
  the extension later.

Steps:

1. **File types and chunks.** *(Done.)*
   - **File types:** `.Rmd`, `.rmd`, `.Rmarkdown` and `.rmarkdown` open as "R Markdown",
     and `.qmd` as "Quarto". Their configs in `grammars` use the Markdown grammar, and
     `languages` loads Markdown's queries for them, so nothing is copied.
   - **Separate languages:** they show their own names in the status bar, and Zed's
     Markdown REPL, which runs code blocks only in "Markdown", leaves them alone.
   - **Highlighting:** a test checks that R Markdown prose is R Markdown and both
     ```` ```{r} ```` and ```` ```{r setup, echo=FALSE} ```` chunks are R. Live, a sample
     report showed Markdown and YAML highlighting with R in its chunks. The buffer needs its
     language registry before its language, so the first parse can find R.
   - **Chunk parser:** `r_markdown::chunks` reads lines, not the syntax tree, so it works on
     any text. It finds each braced fence with its engine, label and options: header
     options split on top-level commas (quotes and brackets respected), then Quarto's `#|`
     lines, the last winning. It also gives the code rows and the closing fence.
     - **Accepted:** backtick or tilde fences, indented chunks, and a chunk left open to
       the end.
     - **Not chunks:** plain fences such as ```` ```r ````, whose contents can't start
       another chunk.
     - **Helpers:** `is_r`, `is_eval_false` and `chunk_at_row`.
2. **Runtime.** *(Done.)*
   - **Inline executions:** `RSession::execute_inline(code)` runs code for a chunk, and
     `is_inline(request_id)` says whether a request was one.
   - **Keeping the IDs:** inline request IDs are kept for the session's life. GPUI delivers
     events after the update that emitted them, so handlers ask about executions that have
     already finished.
   - **Plot tags:** `PlotOpened` carries `request_id`, the oldest pending execution when the
     comm opened, or `None` for a plot drawn outside any execution.
   - **The panes:** the console skips `Output` from inline executions, and the Plots pane
     skips their plots and ignores later events on comms it doesn't hold.
   - **Tests:** unit tests cover the session's tags, the console and the Plots pane. The
     real-Ark test checks the ordering the tags rely on: a plot's comm opens on iopub before
     its execution goes idle.
3. **`r_markdown` (chunk execution).** *(Done.)* Each R Markdown or Quarto editor gets a
   `ChunkRunner`, attached as an editor addon once its buffer's language is known
   (languages often finish loading after the editor opens).
   - **Actions:** Run Chunk (Cmd+Shift+Enter), Run Chunks Above (Cmd+Alt+P), Run All
     Chunks (Cmd+Alt+R) and Clear Chunk Outputs, with Ctrl on Linux. They're registered on
     the workspace and act on the active editor.
   - **Running a selection:** with a selection in an R chunk, Run Chunk runs only the
     selected code, with the output under that chunk. The selection is clipped to the
     chunk's code lines, so a fence never reaches R. A selection of only fence lines runs
     the whole chunk. The run line above a chunk always runs all of it.
   - **Run line:** above each R chunk's fence, "▶ Run Chunk | Run Chunks Above", drawn as a
     spacer block like Zed's code lenses. Positron puts the same lens above Quarto cells.
     Zed has no API for custom gutter buttons, and its run triangles only launch shell
     tasks. The lines are rebuilt 200 ms after edits stop, keeping existing ones.
   - **Running:** chunks go into a queue and run one at a time through `execute_inline`.
     That's what lets an error stop the rest of a Run All, as in RStudio: chunks still
     waiting say "Not run: an earlier chunk failed." So does R stopping. Run All and Run
     Chunks Above skip `eval=FALSE` chunks; Run Chunk runs them. Chunks in other
     languages are skipped.
   - **Output block:** below the closing fence, framed like Zed's REPL output, with a
     button to clear it. It holds the chunk's `ExecutionView`, which `repl` now exports, and
     the session's `Output` events for that request go into it. A rerun resets the same
     block.
   - **Plots:** plots tagged with the chunk's request are drawn by R as PNG. The width is
     the output area's own, measured as it paints and settled after 150 ms; between 200
     and 900 px. The shape comes from `fig.width`/`fig.height` (or Quarto's
     `fig-width`/`fig-height`), with knitr's 7 by 5 as the default. Clearing or rerunning
     closes the plots' comms, so R frees them.
   - **Following edits:** blocks are placed by anchors, so they move with the text.
   - **Checked live:** a sample report run with Run All showed a `message()`, a printed
     data frame, a plot drawn inline at the chunk's 7:4 shape, an error, and the next chunk
     skipped. The console and Plots pane stayed out of it.
4. **Completion in chunks.** *(Done.)* Like the console:
   - **The hidden document:** an R document mirrors the file, with each R chunk's code on
     its own line numbers and every other line blank, Python chunks included. Ark gets it
     before each request as `rezstudio-chunks-<editor>.R`, and its edits apply to the
     editor as they are.
   - **Shared code:** the console's completion code is now a public API in
     `r_console::completions`: `VirtualDocument`, `ark_language_server`,
     `request_completions`, `resolve_completions` and `is_completion_trigger`. Both use it.
   - **Starting Ark's server:** the adapter is also registered for R Markdown and Quarto,
     so Zed starts Ark's server when only such files are open. Ark ignores a document's
     language ID and reads every document as R, so the adapter's `process_diagnostics`
     drops what it reports for `.Rmd`, `.rmd`, `.Rmarkdown`, `.rmarkdown` and `.qmd`
     files.
   - **The editor:** gets a completion provider that wraps the default one, the project.
     Inside R chunk code it asks Ark through the hidden document. Elsewhere it uses the
     default, minus what Ark offers there: Ark sees the whole file as R, so prose would
     get R names.
   - **Checked live:** with only an `.Rmd` open, typing `summary(mtc` in a chunk offered
     `mtcars {datasets}` first, and the prose showed no false errors. A test checks the
     prose filter by failing without it.
5. **Tests.** *(Done.)*
   - **Unit tests:**
     - chunk parsing: labels, options, Quarto lines, unterminated, indented and non-R
       chunks;
     - running a chunk from scripted session events, a selection, rerunning and clearing;
     - Run Chunks Above, and Run All stopping at an error;
     - inline plots drawn at the output's width and shape, and kept out of the Plots pane
       and console;
     - run lines following edits, and the shortcuts reaching the runner;
     - the hidden document's line mapping, completions from a fake Ark in a chunk, and
       prose left alone;
     - R Markdown diagnostics dropped.
   - **Real-Ark test:** the plot ordering that chunk tagging relies on.

Done when: opening an `.Rmd` or `.qmd` file highlights R in its chunks. Cmd+Shift+Enter
runs a chunk and shows its printed output, a data frame, an error, and a plot sized to the
editor under it. Run Chunks Above and Run All work. Rerunning replaces the output. Chunk
plots stay out of the Plots pane. Typing `mtc` in a chunk offers `mtcars`.

All met, with CI green on every job:
- **Checked by hand:** running one chunk, Run All stopping at an error, inline plots,
  rerunning and clearing, and running a selection with Cmd+Shift+Enter.
- **Checked live:** highlighting and `mtc` offering `mtcars`.
- **Not tried by hand:** Quarto (`.qmd`), which shares R Markdown's code and has tests.

### M6 plan (done)

Decisions:
- Help and Viewer join the right dock as two more tabs, next to Environment, Plots and
  Packages.
- Shiny apps and other `http://127.0.0.1` / `localhost` URLs open in the Viewer, with a
  button to pop out to the browser. Other web URLs still go to the browser.
- Every htmlwidget, plot-like or not, shows in the Viewer. Plots keeps R's own plots.
- F1 opens help for the symbol under the cursor in R files, R Markdown chunks and the
  console.

Findings that shape the plan (Ark 0.1.252, and the crates in Zed's tree):

- **Help:** comes over a `positron.help` comm that the frontend opens.
  - `?topic`, `help()` and help links reach R's `browseURL`, which Ark turns into a
    `show_help` event: `{content, kind: "url", focus}`. The URL is on Ark's help proxy at
    `http://127.0.0.1:<port>/`, which rewrites R's help server pages.
  - The `show_help_topic {topic}` request looks a topic up, returning whether it found
    one.
- **Topic at a position:** Ark's language server answers
  `positron/textDocument/helpTopic` with `{textDocument: {uri, version}, position}` and
  returns `{topic}` or null, handling `pkg::fn`. For chunks and the console, it's asked
  about their hidden documents (M4 and M5).
- **Viewer:** fed by UI comm events.
  - `show_html_file {path, title, destination, height}` comes from htmlwidgets, whose HTML
    is in R's temp directory with its `*_files/` dependencies beside it. `destination` is
    `plot`, `viewer` or `editor`. `height` is in pixels, 0 means no preference and -1 as
    tall as possible.
  - `show_url {url}` comes from `browseURL` with an `http(s)` URL, which is how
    `shiny::runApp()` opens its app.
  - RezStudio currently sends both to the system browser.
- **Web view:** `wry` (Tauri's web view crate, MIT/Apache) makes a WKWebView a child of a
  window from a `raw-window-handle`, which GPUI's `Window` provides. `set_bounds` places
  it, and custom protocols can serve local files. It builds on the `objc2` 0.6 family
  already in the tree.
- **Overlap:** a native view draws over GPUI's Metal layer. Whatever GPUI paints in its
  rectangle is hidden: menus, tooltips, the command palette, modals. It also has to be
  hidden whenever its pane isn't painted.

Steps:

1. **Runtime.** *(Done.)*
   - **Help comm:** the session opens it at startup, after the variables comm, so startup is
     now six requests. Its `show_help` URL events become `SessionEvent::ShowHelp {url,
     focus}`. Ark only sends `url` help; other kinds are logged and dropped.
   - **Topics:** `RSession::show_help_topic(topic)` resolves to whether R found the topic;
     the page arrives as a `ShowHelp`.
   - **Viewer events:** `show_html_file` becomes `ShowHtmlFile {path, title, destination,
     height}`. `show_url` for a local address (`127.0.0.1`, `localhost`, `::1` or
     `0.0.0.0`, by `comms::is_local_url`) becomes `ShowUrl {url}`. Other web URLs still
     open in the browser.
   - **For now:** until the panes exist, `runtime::init` sends all three events to the
     browser or the system opener, as before.
   - **Real-Ark test:**
     - `?mean` sends a URL on the help proxy, and fetching it over HTTP returns R's
       "Arithmetic Mean" page;
     - `show_help_topic("lm")` finds the topic;
     - R's `viewer` option, which htmlwidgets use, sends `show_html_file` for a file in
       the temp directory that exists;
     - `browseURL('http://127.0.0.1:5678/')` sends `show_url`.
2. **`web_view`.** *(Done.)* A GPUI element hosting a `wry` web view.
   - **`wry`:** 0.57, a macOS-only dependency with default features off; on macOS it
     always uses WKWebView, and its defaults are Linux's GTK backend. Other platforms get a
     "can't show web pages" placeholder, and pages open in the browser.
   - **Placement:** the native view is made on the element's first paint (it needs the
     window's handle) as a child of GPUI's content view, starting hidden. Each paint moves
     it to the element's bounds.
   - **Visibility follows state, not painting:** docked panels are cached views, and on
     frames where they don't change GPUI replays their last paint without running element
     code. "Not painted this frame" can't mean hidden. So the owner calls
     `set_shown(bool)`, and `watch_cover(workspace, own_id, …)` reports what would draw
     GPUI content over the view: an open modal (observing the modal layer, which the
     workspace now exposes through `modal_layer()`), another item zoomed over the
     workspace, or another workspace active in the window. Panels also get `set_active`
     when their dock closes or another tab shows. Dropping a view hides it at once.
   - **Under a modal:** rather than going blank, a covered view shows a snapshot of its
     page, drawn by GPUI, so the modal draws over it. The theme picker's previews then
     show on help pages too.
     - `set_covered(true)` moves the native view just left of the window. There nobody
       sees or clicks it, but it's still in the window, so WebKit keeps drawing it. It
       also hands the keyboard back to GPUI, since the page would otherwise keep it.
     - WebKit's `takeSnapshotWithConfiguration` gives an `NSImage`, which is encoded as
       PNG and decoded into an image for GPUI to draw at the element's bounds.
     - A restyle or a page load while covered takes a new snapshot, so previewing themes
       under the theme picker shows each one. Uncovering puts the page back and drops
       the image.
     - Checked live with a throwaway window: a page restyled from red to blue while out
       of sight snapshotted blue, and the window showed the snapshot in the page's
       place.
     - Snapshots don't scroll or take clicks, and the first one arrives a frame or two
       after the modal opens.
     - `WebView` is a view of its own, rendered as a child of its pane. As plain state the
       pane read while rendering, a snapshot arriving didn't redraw the docked pane, which
       GPUI caches. So the pane stayed blank under the picker, and a theme preview showed
       for a moment before the next snapshot freed the image the cached drawing still
       used. A test draws the view inside a cached pane to keep this so.
   - **Tooltips and menus:** GPUI draws these over everything, the page included, and a
     native view hides whatever GPUI draws in its rectangle. So the page is cut away where
     they overlap it, live, with no snapshot.
     - GPUI now records where it drew over the window's elements each frame, as
       `Window::overlay_bounds`: the tooltip, and each deferred draw (menus, popovers) that
       painted something, as the union of its hitboxes. Deferred draws that paint nothing,
       such as dock resize handles, don't count.
     - `Context::observe_overlay_bounds` calls a listener after frames where that set
       changed, and once after the frame following its registration. Opening a menu doesn't
       redraw a cached pane, so a check while the pane paints would miss it.
     - The web view cuts the overlapping parts, plus 8 px for shadows, out of its native
       layer with an even-odd `CAShapeLayer` mask. Under a modal it uses the snapshot
       instead, with no cut-outs.
     - Checked live with a throwaway window: two GPUI boxes drawn over a page showed through
       the page at their own positions, the right way up.
   - **Local files:** `load_file` serves the file through a `rezfile://` scheme rooted at
     its folder, since WebKit won't let a page loaded from `file://` read files beside it.
     Paths are percent-encoded; anything outside the folder or with `..` gets a 403.
   - **Navigation:** the view goes to its own files, local addresses and `about:blank`.
     Other links, and links that open a new window, go to the browser.
   - **Page information:** WebKit's title and page-load callbacks reach GPUI through a
     channel, as `WebViewEvent::{TitleChanged, Navigated}`. Back and forward run
     `history.back()` and `history.forward()`, since `wry` has no API for them.
   - **Testing:** the native view is behind a `NativeView` trait, so tests use a fake.
   - **Checked live:** a test window with a 300 px GPUI panel showed a local page beside
     it at exactly the element's bounds (the page measured 668×608 for a 668×608
     element), the right way up, with its `lib/` stylesheet and script loaded through
     `rezfile://`.
3. **`help_panel`.** *(Done.)* A right-dock Help pane showing Ark's help pages.
   - **Showing help:** a `ShowHelp` page loads in the pane and brings it to the front.
     Ark's `focus` asks for no more than that, as in Positron: keyboard focus stays where
     it was, so typing at the console carries on. Builds without a web view open pages in
     the browser. The runtime no longer sends help to the browser itself.
   - **History:** the pane keeps its own list of pages, not WebKit's, so back and
     forward know when they can go.
     - A link followed in a page adds a page after the current one, dropping any
       forward of it.
     - A page the pane asked for (new help, back, forward) takes the URL it ends up at,
       after any redirect, in place of its own entry.
     - `WebView::load_url` now always loads, even the URL it loaded last, since links may
       have led away from it.
     - When R stops, the pages go, as that R's help server served them.
   - **Styling:** pages follow the app's theme and fonts.
     - Ark serves its own `R.css` and `prism.css` with help pages, which take every font
       and colour from variables Positron defines (`--vscode-font-family`,
       `--vscode-editor-background` and so on). Undefined, they fell back to the
       browser's defaults: Times on white, with code no different from prose.
     - The pane defines them from the theme (the panel's background, the text and link
       colours, the terminal's ANSI colours) and from the UI and editor fonts and sizes,
       in a `<style>` a page script adds at document start, before the page draws. It
       also adds what Ark's sheets lack: code blocks on the editor's background with a
       border, bolder headings, table spacing, and the editor's syntax colours for
       highlighted examples.
     - The app's bundled fonts (IBM Plex Sans and Lilex) aren't visible to WebKit, which
       draws in its own process, so unless they're installed pages use the system's.
     - The pane rebuilds the style as it renders, and the web view ignores an unchanged
       one. Theme, setting and font size changes all redraw every window, cached panes
       included, but zooming the font size (cmd-= and cmd--) only changes values that
       can't be observed. A changed style restyles the page shown and adds a new page
       script for later pages. `wry` only takes page scripts as the view is made, so
       this adds a `WKUserScript` to WebKit directly; the newest runs last and wins.
     - The view's background is the panel's, not WebKit's white, so nothing flashes
       white between pages. Views with no page style (the Viewer's) keep white, since
       htmlwidgets expect it.
   - **Keyboard:** a clicked page is AppKit's first responder, and GPUI moving its own
     focus doesn't change that, so keys kept going to the page, even while it was hidden
     under the command palette.
     - Hiding the view hands the keyboard back to GPUI's view if the page has it.
     - The page reports clicks through `wry`'s message channel, and the pane then takes
       GPUI's focus for the page's area. When that focus moves on (the palette, an editor,
       the console), the pane hands the keyboard back.
   - **Toolbar:**
     - back and forward;
     - the help index, from `utils::help.start()` run silently through `evaluate_code`,
       which reaches the pane through `browseURL` like any page;
     - a topic box, where Enter runs `show_help_topic`, and a miss says "No help found
       for `x`." under the toolbar;
     - open in browser.
   - **F1:** `help_panel::ShowHelpAtCursor`, bound to F1 in editors on macOS (on Linux
     F1 opens the command palette). Outside R code it does nothing.
     - It asks Ark's `positron/textDocument/helpTopic`, which only parses the document,
       ignoring its version, and handles `pkg::fn` and `obj$member`.
     - For an R file it asks about the file itself, which Zed keeps open on Ark. For a
       chunk it asks about the hidden chunk document (M5), and for the console input,
       the console's hidden document (M4).
     - It then runs `show_help_topic`. Nothing at the cursor does nothing. A miss, or R
       being busy, says so in the pane and opens it. Notices stay in the pane, since a
       toast at the bottom right would be under the web view.
     - The lookup runs once the action handler returns, since finding Ark's server reads
       the workspace.
   - **Tests:**
     - **Pane:** pages from Ark, links, back and forward, a redirect, hiding under a
       modal, topic search hits and misses, and F1 in an R file and the console against a
       fake Ark server.
     - **Chunks:** F1 is checked in R Markdown's completion test.
     - **Real Ark:** `help.start()` through `evaluate_code` sends the index, which loads
       over HTTP.
4. **`viewer_panel`.** *(Done.)* A right-dock Viewer pane, after Help.
   - **What it shows:** htmlwidgets from `show_html_file`, whatever its `destination`
     (served from their folder through `rezfile://`, with their `lib/` beside them), and
     local web apps from `show_url`, such as Shiny's. Each brings the pane to the front.
     The runtime no longer sends either to the browser; builds without a web view open
     them in the browser or the system's opener.
   - **History:** like Plots: what was shown, oldest first, with previous and next and a
     "2 / 3" count. Showing the same app again (as when it's run again on its port)
     reloads it rather than adding to the history. When R stops, the history goes, as R
     deletes its temporary directory and its apps stop.
   - **Toolbar:** previous and next, the widget's title (or the page's), Stop while R is
     busy running the shown app (it interrupts R, as Shiny's console does), reload, open in
     browser, remove and clear all.
   - **Pages** keep their own styling on a white background, unlike Help's. They share the
     web view's snapshot under modals and its keyboard handoff.
   - **Sizing (changed from the plan):** the height R asks for is kept with the widget but
     not applied. The right dock is full height, so there's nothing to resize until M7
     splits it.
   - **Tests:** widgets and apps, history, reloading the same app, remove and clear, and
     Stop interrupting R.
   - **Checked live:** a reactable widget written the way R writes it for the Viewer
     rendered through the web view in a throwaway window, its React table drawn from the
     `lib/` scripts beside it.
5. **Tests.** *(Done.)* Written with each step; the web view is behind a `NativeView`
   trait, so tests use a fake instead of WebKit.
   - **Unit tests:**
     - the help comm and viewer events in the session;
     - the web view's placement, page styles, snapshots under a modal (inside a cached
       pane too), and cut-outs for overlays;
     - the Help pane's history, links, topic search, F1 against a fake Ark server,
       restyling, and the keyboard handoff;
     - the Viewer's history, apps, and Stop interrupting R;
     - in GPUI, overlay bounds and their listener.
   - **Real-Ark test:** the step 1 checks; `help.start()` through `evaluate_code`; and a
     second LSP comm getting its own port.

Done when: `?mean` shows R's help for `mean` in the Help pane, links in it work, and back
and forward step through pages. F1 on `lm` in an R file, a chunk and the console opens its
help. `plotly::plot_ly(mtcars, x = ~wt, y = ~mpg)` shows an interactive plot in the
Viewer. `shiny::runApp()` on an example app runs it in the Viewer, and Stop ends it. Menus
and the command palette are never hidden behind the web view.

All met, with CI green on every job:
- **Checked by hand:**
  - `?filter` and links in help;
  - help styled from the theme, following font size changes and theme previews under
    the theme picker;
  - typing in the command palette after clicking a help page;
  - F1 on `lm` in an R file, an R Markdown chunk and the console;
  - a plotly widget, and `shiny::runExample("01_hello")` stopped with Stop;
  - status bar tooltips showing over a help page;
  - opening a `.Rproj` from Finder.
- **Fixed after trying it:**
  - Ark's language server failing after Zed restarted it (each server now gets its own
    port);
  - closed windows staying in memory, which also hung CI's Linux tests;
  - Ark outliving RezStudio (the supervisor now kills it);
  - `.Rproj` files opening without their project, and Finder not offering RezStudio
    for R files;
  - a Linux-only lint error.
- **Known limits:**
  - snapshots under a picker lag the theme by up to a second;
  - File > Open on a `.Rproj` still opens just the file;
  - a widget's requested height waits for M7.

### M7 plan (done)

Decisions:
- **Right dock:** two stacked panes with tabs, split as in RStudio. Environment and Git are on
  top; Files, Plots, Packages, Help and Viewer are below.
- **Moving tabs:** tabs can be dragged to reorder them or to move them to the other pane,
  and each window remembers where they are.
- **Outline:** RStudio's document outline, a sidebar inside the editor. It lists sections,
  chunks and functions, jumps to one when clicked, and is toggled by a button in the editor's
  toolbar. The outline panel goes.
- **Bottom dock:** Console, Terminal and Debug as tabs.
- **Status bar:** the panel buttons stay, for getting panes back when a dock is closed.

Findings that shape the plan:
- **How a dock draws:** `workspace::Dock` keeps a list of panels and one active index, and
  draws only that panel, as a cached view.
  - Panels learn when they show or hide through `Panel::set_active`, which Help and Viewer
    rely on.
  - The status bar's `PanelButtons` lists every panel in the dock, and clicking one makes it
    the active panel.
- **Sizes:** each panel keeps its own width or height (`PanelSizeState`), saved under
  `dock_panel_size` in the key-value store. A split dock needs one width shared by both
  panes.
- **What's saved:** `DockData` saves only whether the dock is open, its active panel and
  zoom. Pane contents, order and the divider need saving too, in the key-value store like
  sizes, which avoids a database migration.
- **Where panels are now:** Files, Outline and Git already default to the right dock, beside
  Environment, Plots, Packages, Help and Viewer. Console, Terminal and Debug are in the
  bottom dock.
- **Bottom dock layout:** `bottom_dock_layout` is already `contained`, which keeps the bottom
  dock under the editor with the right dock full height. That is RStudio's arrangement.
- **Terminal:** the Terminal panel has its own tabs, one per terminal, so it sits as one tab
  holding its own row of terminal tabs.
- **Editor toolbar:** `crates/zed/src/zed/quick_action_bar.rs` already has buffer search,
  Selection Controls and Editor Controls. The Outline button goes beside them.
- **Outlines:** R's outline query lists functions and RStudio code sections (`# Title ----`).
  R Markdown and Quarto use Markdown's, which lists headings but not chunks.
- **Web views:** the Help and Viewer web views follow their element's bounds, snapshot under
  modals and cut out tooltips and menus. A half-height pane needs no special handling, but
  it should be checked.

Steps:

1. **Dock panes (`workspace`).** *(Done.)* A dock can hold one or two panes, each with its own
   panels, the panel it shows, and a tab row.
   - **Layout:** `DockLayout` lists each pane's panels by persistent name, in tab order,
     with the one it shows, and the divider's share.
     - Panels it doesn't name go at the end of the last pane, so new panels still appear.
       A pane with none of its panels present gives the dock to the other.
     - `Dock::set_layout` applies a layout and saves it. The workspace restores a saved
       one when the dock gets its first panel.
     - It's saved per window under `dock_layout` in the key-value store, keyed like panel
       sizes, so there's no database migration.
   - **What's shown:** each pane shows its remembered panel, else the dock's active panel
     if it's there, else its first.
     - `Dock::active_panel()` keeps Zed's meaning: the panel last chosen, which focus and
       zoom follow. `visible_panels()` lists what each pane shows.
     - Choosing a panel hides only the one its own pane showed. Opening or closing the
       dock tells every shown panel.
     - A panel shown because the layout changed, rather than because it was chosen, is
       only told while the dock is open, so a web view never shows in a closed dock.
   - **Tab rows:** a pane with more than one panel shows each one's icon and name: its
     status bar tooltip without "Panel", so "Project Panel" is "Project". Clicking a tab
     shows and focuses that panel. The status bar highlights every shown panel, and its
     buttons still work.
   - **Divider:** side docks stack their panes and the bottom dock puts them side by side.
     The divider can be dragged (each pane keeps at least a tenth of the dock), and
     double-clicking it resets it to half and half. The layout is saved when a drag ends.
   - **Width:** a split dock resizes all its panels together, and splitting evens their
     sizes, so both panes share one width.
   - **Zed's tests:** single-pane docks behave as before, and the workspace's 250 tests
     pass unchanged.
   - **New tests:** split showing and hiding, panes remembering what they show, the shared
     width, closing and reopening, a new panel joining the last pane, an emptied pane, and
     saving and restoring.
   - **Checked live:** a temporary split of a real window's right dock showed Environment
     and Git above and Project, Plots, Packages, Help, Viewer and Outline below, with tab
     rows and the divider. The bottom dock showed Terminal, Debug and R Console as tabs.
2. **Dragging tabs.** *(Done.)* Tabs drag within their dock.
   - **Dropping:**
     - on a tab puts the dragged one before it, with an accent line at the tab's start
       showing where;
     - on the empty end of a tab row puts it last;
     - in a split dock, on a pane's content moves it into that pane.
   - **Splitting:** while a tab is dragged in a dock with one pane, the pane's top and
     bottom thirds (left and right in the bottom dock) become drop zones that split the
     dock, with the tab in the new pane. A split dock takes no third pane.
   - **Moving:** `Dock::move_panel` takes the panel and a `TabDropTarget` (before a tab,
     at a pane's end, or a new pane at either end).
     - It writes out the panes as the dock shows them, panels the layout didn't name
       included, then moves the panel and shows it where it lands.
     - Emptying a pane gives the dock to the other. The layout is saved.
   - **Dragging:** a dragged tab shows as a small copy of itself. Tabs move only within
     their own dock.
   - **Over web views:** GPUI draws the drop highlights and the dragged tab, so they're
     hidden over a Help or Viewer page, but drops there still work. AppKit keeps sending
     the drag to GPUI's view, where it started.
   - **Tests:** every kind of move; dragging a tab with the mouse onto a tab in the other
     pane; and dropping one near the bottom of a one-pane dock to split it.
3. **RStudio defaults.** *(Done.)*
   - **Default layouts:** `workspace::dock::DefaultDockLayouts` is a global an app can set.
     A dock with no saved layout for its window starts from it. Without it, docks have one
     pane, as in Zed, so Zed's tests don't change.
   - **RezStudio's defaults** (`rstudio_dock_layouts` in `zed.rs`):
     - **Right dock:** Environment and Git above; Project, Plots, Packages, Help and
       Viewer below. Each pane starts on its first tab.
     - **Bottom dock:** R Console, Terminal and Debug, in that order.
     - Windows with a saved layout keep theirs.
   - **Outline panel:** no longer added to the workspace, and its View menu item is gone.
     Its actions still exist, so key bindings that name them still load. cmd-shift-b is
     left for the outline sidebar (step 4).
   - **Widget heights (from M6):** `Dock::make_room_for(panel, PaneRoom)` moves the divider
     so the panel's pane has at least that much room, tab row included, or as much as the
     other pane allows. The dock remembers its split area's size as it draws. The Viewer
     asks for a widget's height plus its toolbar when it shows one, and for `-1`, as much
     as possible. A dock with one pane is left alone.
   - **Tests:** a dock starting from its default layout, and making room (enough already,
     growing the lower pane, as much as possible for either pane).
   - **Checked live:** a new window on an empty folder opened with Environment and Git
     above, Project, Plots, Packages, Help and Viewer below, and no Outline tab. The bottom
     dock showed R Console, Terminal and Debug.
4. **Outline sidebar.** A toolbar button toggles a sidebar at the right of the editor,
   remembered per editor. *(Done.)*
   - **Contents:** the document's outline: R sections and functions; Markdown headings; and
     in R Markdown and Quarto, chunks by label.
   - **Using it:** clicking an entry jumps there, and the entry at the cursor is highlighted.
   - **Where it lives:** it belongs to the editor, not to a dock.
   - **How it's built:**
     - **The pane:** `workspace::Pane` can hold a side view, drawn right of the active item
       and under the toolbar while shown. The pane keeps it while it's hidden.
     - **The crate:** `outline_sidebar` gives each center pane a sidebar (from
       `initialize_pane` in `zed.rs`). It shows when the pane's active editor has its outline
       on. Which editors have it on is kept in memory by editor, so an editor moved to another
       pane keeps it. A split makes a new editor, which starts with it off.
     - **Toggling:** the Outline button (a list icon at the end of the editor toolbar), or
       `outline_sidebar::Toggle` on cmd-shift-b, which used to focus the outline panel.
   - **Entries:**
     - **R:** from R's outline query. A section's level is its number of leading `#`, and
       its name drops the `#`s and the trailing `-`, `=` or `#`. Divider lines with no name
       are left out. Functions sit one level under the section above them, and nested
       functions under theirs.
     - **R Markdown and Quarto:** headings from the Markdown outline query. Chunks come from
       `r_markdown::chunks` rather than a query, so they match what the Run buttons see. They
       read "Chunk 2" or "Chunk 2: label" (the label from the header or `#| label:`) and sit
       one level under the heading above them. The outline also carries items from embedded
       languages (front-matter keys, functions in chunks); those are left out, as in RStudio.
     - **Other languages:** their outline as it is.
     - **Updating:** entries are rebuilt off the main thread when the buffer is edited or
       reparsed; the highlight follows the cursor and scrolls into view.
   - **Tests:** R sections and functions with nesting, levels and unnamed dividers;
     headings and chunks; and in a workspace, toggling the sidebar, its entries for an R
     file, the cursor's entry, and clicking one to jump there.
   - **Checked live:** an R file showed its sections and functions nested by level, and an
     R Markdown file its headings and chunks, each with the cursor's entry highlighted.
5. **Tests.** *(Done.)*
   - **Unit tests:**
     - dock panes: showing, `set_active`, the divider, sharing a width, saving and
       restoring, dragging tabs, emptying a pane;
     - the default layout and new panels;
     - the Viewer's requested height;
     - the outline sidebar's entries for R, R Markdown and Quarto, and jumping to them.
   - **Checks by hand:** Help and Viewer in a half-height pane, with snapshots under the
     command palette and tooltips over pages; a Shiny app in the bottom pane.
   - **Added:** the Viewer's requested height, in a split right dock: no preference leaves
     the divider alone, a height makes the Viewer's pane at least that tall plus its toolbar,
     and `-1` takes as much as the other pane allows. The other tests came with steps 1–4.
   - **Checked live,** in the lower right pane of a new window:
     - `?lm` showed the styled help page, which stayed styled under the command palette;
     - a plotly widget showed in the Viewer;
     - a Shiny app showed in the Viewer, with its stop button, and quitting left no R
       running.
     - Not rechecked: tooltips and menus over a page in the half-height pane. They use the
       same cut-outs checked in M6, which follow the web view's bounds.

Done when:
- A new window shows Environment and Git above, and Files, Plots, Packages, Help and Viewer
  below, in the right dock, with the console under the editor.
- Clicking a tab switches panes; dragging one between panes or reordering it is remembered
  after reopening the window.
- Console, Terminal and Debug are tabs in the bottom dock.
- The editor's Outline button shows an outline of an R file's sections and functions, and
  of an R Markdown file's headings and chunks, and clicking an entry jumps to it.
- Help pages and htmlwidgets work in a half-height pane, under menus and the palette.

### M8 plan (done)

Decisions:
- **Order:** this comes before Python parity, which moved to M9 (now M10). Neither
  feature needs Python, and Python parity then ports a finished Environment pane once
  instead of twice.
- **Environment menu:** as in RStudio, a menu at the top of the Environment pane lists
  Global Environment and then every `package:` environment on R's search path, in
  `search()` order. Package environments are read-only: no delete or Remove All. RStudio's
  language picker beside it ("R") waits for Python parity (M10), when there is a
  second language.
- **Grouping:** RStudio's list view, with rows under Data, Values and Functions headings.
  Data frames read "150 obs. of 5 variables". Clicking a column heading sorts by it.
- **Memory:** RStudio's memory indicator in the pane's toolbar, showing what the R session
  uses. Clicking it shows a report with the session's and the system's memory, and a way
  to free unused R memory.
- **Workspace:** buttons to save the environment to an `.RData` file and to load one. They
  run `save.image()` and `load()` in the console, so the code shows there, as in RStudio.
- **Import Dataset:** RStudio's menu with From Text (readr), From Excel (readxl) and From
  SPSS, SAS and Stata (haven). A dialog picks the file, previews the first rows, sets the
  options and shows the code. Import runs that code in the console and opens the result
  with `View()`.
- **Rendering:** a Render button in the editor toolbar of `.qmd` and `.Rmd` files, and
  Cmd+Shift+K, as in RStudio.
  - Rendering runs in its own process, not the console's R session.
  - Its output streams to a Render tab in the bottom dock, which has a stop button.
  - HTML opens in the Viewer pane. PDF and Word open in the system's default app.
  - `.qmd` files use `quarto preview`, which serves the page for the Viewer. `.Rmd` files
    use `rmarkdown::render()` in `Rscript`, and the Viewer shows the file it writes.
  - A Render on Save toggle re-renders whenever the file is saved.

To check before starting:
- *(Checked in step 1.)* Ark's variables comm lists only the global environment (or the
  frame being debugged), and its `inspect` paths start there too, so package environments
  are listed by R code run with `evaluate_code`.
- *(Checked in step 5.)* The Viewer already serves a local HTML file's folder under its
  `rezfile://` scheme (from M6), so a rendered page's `_files/` folder loads. `quarto
  preview` prints `Browse at <url>` once it serves, and both renderers print `Output
  created: <file>`.
- *(Checked in step 3.)* The R session's memory is read with `sysinfo`, from the process
  Ark runs under, so it doesn't need R and keeps updating while R is busy.

Steps:
1. **Environment menu.** *(Done.)* A menu at the left of the Environment pane's toolbar
   lists Global Environment and then the `package:` entries of `search()`, in order.
   - **Search path:** fetched with `evaluate_code` after each refresh of the global
     environment and after each execution, since `library()` and `detach()` don't touch
     the global environment. Detaching the package being shown goes back to Global
     Environment, as in RStudio.
   - **Listing a package:** R code returns one record per object in the shape Ark's
     variables comm uses, so the pane's tree shows it unchanged. `evaluate_code` turns an
     unnamed R list into a JSON array and an empty one into null.
     - Functions are forced, which only loads their code, and show their arguments.
     - Lazy-loaded data stays a `<Promise>`, as in RStudio. Package environments hold
       their data as promises (all of `package:datasets`, and `ggplot2::diamonds`), and
       forcing them could load a large dataset.
     - Data frames read "32 obs. of 11 variables", and other values show `str()`'s first
       line.
     - Timed in R: `package:base` (1,283 objects) takes 0.14 s, ggplot2 (649) 0.19 s.
   - **Function source:** clicking a function, or its View Source button, in a package
     or the global environment opens its source in a read-only R editor tab, as
     RStudio's `View()` does. The tab is titled like RStudio's, for example `as_factor
     (namespace:forcats)`. The source comes from `deparse(..., control = "useSource")`,
     so functions defined in the session keep their comments; primitives show as
     `.Primitive("sum")`. Added after step 5, at the user's request, with a test that
     clicks a package function's row.
   - **Read-only:** a package's rows have no delete button, and Remove All is off. Rows
     don't expand. Only the last environment chosen is shown, if an earlier listing
     arrives late.
   - **Shared code:** `runtime::r_string` quotes R strings for the pane, the Packages
     pane and the console, which each had their own copy.
   - **Tests:** the search path's parsing, the listing code's quoting, choosing a
     package, the filter, Remove All staying off, detaching the package shown, a late
     listing, and an empty environment. The existing tests now answer the search path
     request that follows each refresh.
   - **Checked live:** the menu listed Global Environment and the seven default
     packages in search order, and choosing one listed its functions with their
     arguments. Not checked live, since the machine was in use: `library()` adding a
     package to the menu, and detaching the package shown. Unit tests cover both.
2. **Grouping and sorting.** *(Done.)* Top-level rows sit under Data, Values and Functions
   headings, in both the global environment and package environments.
   - **Groups:** Data holds what Ark calls a table (data frames and two-dimensional
     matrices), Functions its functions, and Values everything else. A group the filter
     empties is left out. Children keep R's order under their parent.
   - **Data frames:** Ark's `[32 rows x 11 columns] <data.frame>` reads "32 obs. of 11
     variables", as in RStudio. Matrices keep Ark's text, since RStudio doesn't describe
     them that way.
   - **Sorting:** clicking Name, Type, Size or Value sorts each group by that column, and
     clicking it again reverses the order. An arrow marks the column. Ties fall back to
     the name, and the order is kept across environments.
   - **Tests:** grouping, sorting by size both ways, the filter dropping empty groups, the
     data frame text (including one column and a count with commas), and clicking
     the headings in the pane.
   - **Not checked live:** the machine was in use. The headings and group rows reuse
     the pane's existing row layout.
3. **Memory and workspace.** *(Done.)* The pane's toolbar now has two rows, as in
   RStudio: Load, Save, the memory reading and Remove All above, and the environment
   menu and filter below.
   - **Memory:** every five seconds the pane sums the resident memory of the process R
     runs under (the shell supervising Ark), Ark, and any processes R started.
     `RSession::process_id` gives the shell's id. The toolbar shows the total, and its
     menu shows the session's and the system's memory and Free Unused R Memory, which
     runs `gc()` in the console. The reading is hidden while R isn't running.
   - **Workspace:** Save Workspace As asks for a file in R's working directory,
     suggesting `.RData`, and runs `save.image("…")` in the console. Load Workspace asks
     for a file and runs `load("…")`. Paths under the home folder are written with `~`,
     as in RStudio. Both are also actions, for the command palette.
   - **Tests:** the memory reader finds a real shell's child process and the system
     totals, nested and cyclic parent tables, the save and load code from simulated
     dialogs, cancelling a dialog, and path quoting.
   - **Not checked live:** the machine was in use.
4. **Import Dataset.** *(Done.)* An Import Dataset menu in the toolbar's top row offers
   From Text (readr), From Excel, From SPSS, From SAS and From Stata. Each opens a dialog
   that starts by asking for a file.
   - **Fields:** the file's path (or a URL, which readr reads too), with Browse; the
     name, suggested from the file's name with characters R doesn't allow in names
     replaced by `_`, until it's typed over; and Open Data Viewer. Text and Excel also
     have Skip, Missing Values and First Row as Names. Text has a Delimiter (comma,
     tab, semicolon, whitespace) and Excel a Sheet, listed from the file.
   - **Preview:** the first 50 rows, read with the chosen options by R code run with
     `evaluate_code` 300 ms after the last change. It returns each column's name,
     class and values formatted as text, and an Excel file's sheets. A newer change
     drops a preview still waiting. R's errors, such as a missing package, show in the
     preview's place.
   - **Code:** shown in a read-only editor, so it can be selected and copied, with a
     Copy button as in RStudio (added after step 5). It's written as RStudio writes it,
     with only the arguments that differ from the reader's defaults, for example `library(readr)`, `cars <- read_csv("~/cars.csv")`
     and `View(cars)`. Names R doesn't accept as they are go in backticks. Import runs
     it in the console and closes the dialog.
   - **Checked in R:** the preview and import code for a CSV with row names, a
     semicolon file with a skipped line and a custom missing value, readxl's example
     workbook with and without a sheet, and haven's SPSS, SAS and Stata examples.
   - **Tests:** the code for each format and option, the preview code, names, the
     preview's parsing (including R's one-value and empty vectors), and the dialog
     from choosing a file through a preview, a delimiter change and importing.
   - **Not checked live:** the machine was in use.
5. **Rendering.** *(Done.)* A new `render_panel` crate adds the Render tab, after
   Terminal in the bottom dock, and the actions behind the editor's Render button.
   - **Starting:** the Render button (an arrow in a `.qmd` or `.Rmd` editor's toolbar)
     and Cmd+Shift+K in those editors save the document if needed, then render it. A
     new render stops the one running, as only one runs at a time.
   - **Quarto:** `quarto preview <file> --no-browser --no-watch-inputs` in the
     document's folder. `QUARTO_R` points Quarto at the project's R. When it prints
     `Browse at <url>`, the Viewer shows the page. Rendering the same document again
     replaces the Viewer's entry rather than adding one, though the port changes. The
     server keeps running until stopped or replaced, and stopping kills its process
     group, since `quarto` starts `deno`, which outlives a plain kill. Quitting the app
     stops it too. A crash would leave it running, since it has no supervisor like
     Ark's.
   - **R Markdown:** `Rscript -e 'rmarkdown::render("<file>", encoding = "UTF-8")'`
     with the project's R. When it exits successfully, the file named by `Output
     created:` shows in the Viewer.
   - **Other formats:** the Viewer shows HTML and PDF. Word, PowerPoint and the like
     open in their own apps. The web view now serves PDFs as `application/pdf`.
   - **Environment:** the R session's environment (the project's shell environment),
     with `NO_COLOR=1`, since escape codes would show as text. Without pandoc on the
     path, R Markdown gets `RSTUDIO_PANDOC` pointing at the pandoc bundled with Quarto.
     Quarto is looked for on the path, then where its installers, RStudio and Positron
     put it.
   - **The Render tab:** the command and its output (up to 5,000 lines), a status line
     (rendering, previewing at the address, rendered, stopped, or failed with the exit
     code), a stop button, and Clear.
   - **Render on Save:** in the menu beside the Render button. While it's on, each save
     renders the document. Render's own save doesn't render twice.
   - **Tests:** finding Quarto and R, the commands and environments, quoting, pandoc
     from Quarto, reading output lines with escape codes, which results show in the
     Viewer, and the Render tab running scripts that act like Quarto (serving until
     stopped), R Markdown (finishing), a failing render, and a render replacing one
     still running. The Viewer replaces a re-rendered document's entry. Render on Save
     toggles for a Quarto file and isn't offered for Markdown. The app's action and
     keymap tests pass with the new actions and Cmd+Shift+K.
   - **Checked by hand:** `quarto preview` and `rmarkdown::render` on small documents,
     with Quarto 1.9.36, including a Word document.
   - **Not checked live:** the machine was in use.
6. **Tests and live checks.** *(Done.)* Unit tests for each step, a real-R test for package
   listings and imports, and live checks of rendering a Quarto and an R Markdown document.
   - **Shared Ark client:** the runtime crate's real-Ark test client is now
     `runtime::test_support` (behind `test-support`), with `Kernel::start`,
     `open_ui_comm`, `evaluate` (as `evaluate_code`), `run` and `shut_down`, so other
     crates' real-R tests drive Ark the same way.
   - **Real-R test for the Environment pane** (`cargo test -p variables_panel --
     --ignored`): the search path before and after `library(forcats)`, `package:stats`
     and `package:datasets` listings (a function, and a lazy dataset as `<Promise>`),
     function source for a package function, a session function (comments kept) and a
     primitive, the session's memory, previews and imports of a CSV, a semicolon file, an
     Excel sheet and a Stata file, and a workspace saved, cleared and loaded back.
   - **Real render test** (`cargo test -p render_panel -- --ignored`): the commands
     Render builds, run with real Quarto and `rmarkdown::render`. The Quarto preview
     serves a page with the chunk's output, and the R Markdown HTML holds it too.
   - **Checked live:** the environment menu on `package:forcats` (Values and Functions
     groups, a lazy dataset as `<Promise>`), function source tabs with R highlighting, the
     Global Environment's Data, Values and Functions groups with `3 obs. of 2 variables`,
     the memory indicator, the Import Dataset dialog's typed preview and code, and Render
     on a `.qmd` (Quarto preview in the Viewer, output in the Render tab) and an `.Rmd`
     (the HTML in the Viewer as its second entry).
   - **Found live and fixed:** the Render tab's rows are one line high, so a long line
     such as pandoc's command wrapped over the lines below it. Lines now stay on one row
     and the list scrolls sideways. The fix has a unit test but wasn't seen live, since
     the machine was in use.
   - **Not checked live:** sorting, workspace save and load, Render on Save, and
     clicking Import (the real-R test covers the import code).

Done when:
- The Environment menu switches between Global Environment and attached packages, and
  `library()` adds a package to it.
- Rows are grouped as in RStudio and sort by column.
- The memory indicator shows the session's memory, and saving and loading a workspace
  works.
- Import Dataset brings in a CSV, an Excel sheet and a Stata file.
- Render on a `.qmd` and an `.Rmd` file shows the HTML in the Viewer, with output in the
  Render tab.

### M9 plan

Dark mode for what R draws: plots in the Plots pane, and pages in the Viewer. R's plots
and most rendered documents have white backgrounds, which glare in a dark theme.
RStudio leaves them white; Positron offers a dark filter for plots.

Decisions:
- **Darken what's shown, never what's kept.** R draws as it always does, and RezStudio
  darkens the result on screen. Copy Plot, Save Plot (R draws again at the chosen size),
  the Viewer's Open in Browser, and rendered files on disk are always the originals. The
  other way, theming R itself with `par(bg = ...)` or a dark ggplot theme, would change
  the user's graphics settings and their exports, and packages override it (ggplot2
  ignores `par()`).
- **A toggle in each toolbar.** A moon button in the Plots toolbar and another in the
  Viewer toolbar, each with a tooltip saying what it does. Each pane has its own
  setting, `plots.dark_mode` and `viewer.dark_mode`, with the values `off`, `on` and
  `follow_theme` (on while the app's theme is dark). The button toggles its pane between
  on and off and saves that to the settings file; `follow_theme` is set there.
- **Off by default.** RStudio never changes a plot's colors, and darkened colors are not
  the true ones, so it's a choice. The toggle is one click away to check the real colors
  before exporting.
- **Plots:** each pixel's lightness is flipped while its hue and saturation stay, so red
  stays red and a dark line on white becomes a light line on dark. Lightness is scaled
  to run from the pane's background color to its text color, so a white plot background
  becomes the pane's own background. Known effects: a palette that runs light to dark,
  such as viridis, runs dark to light, and a photo drawn into a plot looks like a
  negative.
- **Viewer:** two layers under the one toggle.
  - Pages are told the app is dark, through the web view's appearance, so pages with a
    dark design of their own (`prefers-color-scheme: dark`), such as some Quarto themes,
    pkgdown sites and htmlwidgets, switch to it.
  - Pages that are still light once loaded get a filter: CSS that inverts the page and
    turns its hues back, the usual way to darken a page that has no dark design. A
    script checks the page's background after it loads, so a page that's already dark
    is left alone rather than turned light.
  - Under the filter, JPEG and WebP images and videos are turned back, so photos look
    right. PNG and SVG images stay darkened, since in R output they are mostly plots,
    which then match the Plots pane. Leaflet's map tiles are PNGs, so maps darken too.
  - The Help pane is already themed (M6) and is not part of this.

To check before starting:
- Whether setting a `WKWebView`'s own `appearance` to dark makes its pages see
  `prefers-color-scheme: dark` while the window and system are light, as when the app's
  theme is dark on a light Mac.
- How a RezStudio crate registers its own settings with Zed's settings system, keeping
  the change to Zed's settings crates small, and that a toolbar button can write the
  setting to the settings file, as Zed's own toggles do.
- Whether Ark's plots have a white or a transparent background, which decides what the
  flipped background becomes.

Steps:
1. **Settings.** `plots.dark_mode` and `viewer.dark_mode`, each `off`, `on` or
   `follow_theme`, defaulting to `off`, with the effective on or off worked out from the
   theme's appearance.
2. **Dark plots.** Darken the decoded image the pane draws, keeping the PNG that Copy
   uses. Toggling or a theme change redoes it from the kept PNG, without asking R to
   draw again. Everywhere the pane draws a plot is darkened: the pane, its history and
   the zoom window. The moon button goes in the Plots toolbar.
3. **Dark Viewer.** Set the web view's appearance from the setting, and add the filter
   script, which runs on each page as it loads and again when the toggle changes, with
   no reload. The moon button goes in the Viewer toolbar.
4. **Tests and live checks.**
   - Unit tests for the setting, the lightness flip (white to the pane's background,
     black to its text color, a pure red staying red), and the Viewer's script and CSS.
   - Live checks of a base plot and a ggplot2 plot, a rendered Quarto and R Markdown
     document, a plotly widget, a leaflet map and a Shiny app, with the theme dark and
     light.

Done when:
- With the toggle on, plots in the Plots pane and the zoom window match a dark theme,
  and Copy Plot and Save Plot still give the original light plot.
- A rendered document, an htmlwidget and a Shiny app in the Viewer are dark, a page with
  its own dark design uses it, and photos aren't inverted.
- `follow_theme` darkens both panes when the theme switches to dark and restores them
  when it switches back.
- With both settings `off`, nothing looks different from today.

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
