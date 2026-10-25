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
| M1 | `runtime` + `console` for R *(done)* | Ark bundled; R discovered; Cmd+Enter / Cmd+Shift+S run code; interrupt and restart work; rich output in the console; Ark LSP completions in the editor and console. |
| M2 | Environment + data viewer *(done)* | Live variables; `View(df)` opens the paged grid on a 10M-row frame without lag. |
| M3 | Plots *(done)* | Plot history, resize re-render, export. |
| M4 | Packages + search | Installed/loaded list; instant offline search across CRAN, Bioconductor, R-universe. |
| M5 | Rmd/Quarto inline chunks | Chunk execution with inline output. |
| M6 | Help + Viewer | WKWebView embedded; help, htmlwidgets, Shiny. |
| M7 | Python parity | ipykernel add-on implements the comms; all panes work for Python. |
| M8 | Typst native preview | Live preview with source ↔ preview jumping. |
| M9 | Debugger | Ark DAP through Zed's `debugger_ui`: breakpoints, `browser()`, stepping. |

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
   if RezStudio dies, since Ark doesn't watch its parent. An execution is finished only once both
   its `execute_reply` (shell) and iopub `idle` have arrived, because output can trail the reply.
   Ark's `rpc_request`s on stdin are always answered: questions and dialogs use native prompts,
   the workspace folder is the project root, and unsupported ones return a JSON-RPC error. Links
   (including help) open in the system browser until M6; `positron.plot` comms are dropped until
   M3. Comm types are ported from Ark 0.1.252's MIT-licensed generated Rust.
3. **LSP bridge.** *(Done.)* The hidden `rezstudio --lsp-bridge <port>` mode copies stdio to Ark's
   LSP TCP port, like Zed's `--askpass` netcat mode. The `ark` language server adapter (registered
   by `runtime` for R) waits for the window's session to report its port, then launches the
   bridge; when R restarts on a new port, `ark` is restarted for the project's open buffers.
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
