# RezStudio

RezStudio is a data-science IDE for macOS built on [Zed](https://github.com/zed-industries/zed): RStudio's workflow and panes (console, environment, plots, packages, help) with Zed's speed and editor.

R is the first-class language (including R Markdown and Quarto), followed by Python and Typst. RezStudio is a fork of Zed with the AI, collaboration, and zed.dev account features removed; see [`rezstudio/DESIGN.md`](./rezstudio/DESIGN.md) for the architecture and roadmap.

> RezStudio is an independent project and is not affiliated with or endorsed by Zed Industries.

### Building

RezStudio ships for macOS only. Follow Zed's [macOS build guide](./docs/src/development/macos.md) to install the toolchain, then:

```sh
cargo run -p zed --bin rezstudio      # run a debug build
script/bundle-rezstudio -d -o         # build and open a debug RezStudio.app
script/bundle-rezstudio -m            # release RezStudio.app plus a .dmg
```

The code also compiles on Linux so it can be checked and tested in CI, but Linux builds are not supported.

RezStudio keeps its settings in `~/.config/rezstudio` and its data in `~/Library/Application Support/RezStudio`, so it can run side by side with Zed.

### Licensing

RezStudio, like Zed, is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked. Zed is developed by Zed Industries, Inc.

License information for third party dependencies must be correctly provided. We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to comply with open source licenses; `script/generate-licenses` produces the bundled notices, configured by `script/licenses/zed-licenses.toml`.
