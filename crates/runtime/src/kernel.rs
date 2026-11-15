//! The seam between a session and the kernel it runs. A session speaks the
//! Jupyter protocol and the comms on top of it, whichever kernel answers; what
//! differs per language is finding the interpreter and starting its kernel,
//! which a [`KernelLauncher`] does.

use crate::ark::ArkLauncher;
use anyhow::Result;
use collections::HashMap;
use languages::r::RToolchainData;
use runtimelib::{
    ClientControlConnection, ClientIoPubConnection, ClientShellConnection, ClientStdinConnection,
};
use std::path::PathBuf;
use util::ResultExt as _;

/// The language a session runs. A project window has one session per language.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionLanguage {
    R,
}

impl SessionLanguage {
    /// The language's name as messages to the user spell it.
    pub fn name(self) -> &'static str {
        match self {
            SessionLanguage::R => "R",
        }
    }
}

/// Finds a language's interpreter and starts its kernel.
#[derive(Clone, Debug)]
pub enum KernelLauncher {
    Ark(ArkLauncher),
}

impl KernelLauncher {
    /// Launches Ark for the R installation `r`, or for none when R wasn't
    /// found. `bundled_ark` is the copy of Ark inside RezStudio.app.
    pub fn ark(r: Option<RToolchainData>, bundled_ark: Option<PathBuf>) -> Self {
        KernelLauncher::Ark(ArkLauncher { r, bundled_ark })
    }

    pub fn language(&self) -> SessionLanguage {
        match self {
            KernelLauncher::Ark(_) => SessionLanguage::R,
        }
    }

    /// Why no kernel can be started, such as a missing interpreter.
    pub fn unavailable_reason(&self) -> Option<String> {
        match self {
            KernelLauncher::Ark(ark) => ark.unavailable_reason(),
        }
    }

    pub fn r_installation(&self) -> Option<&RToolchainData> {
        match self {
            KernelLauncher::Ark(ark) => ark.r.as_ref(),
        }
    }

    /// Starts the kernel in `working_directory` and connects to its sockets.
    pub(crate) async fn launch(
        self,
        working_directory: PathBuf,
        env: HashMap<String, String>,
    ) -> Result<(KernelProcess, KernelConnections)> {
        match self {
            KernelLauncher::Ark(ark) => ark.launch(&working_directory, env).await,
        }
    }
}

/// A running kernel process. Dropping it kills the process.
pub struct KernelProcess {
    process: util::process::Child,
    connection_path: PathBuf,
}

impl KernelProcess {
    pub(crate) fn new(process: util::process::Child, connection_path: PathBuf) -> Self {
        Self {
            process,
            connection_path,
        }
    }

    pub fn kill(&mut self) {
        self.process.kill().log_err();
    }

    /// The id of the process that was spawned. For Ark that's the shell
    /// supervising it, whose child Ark is.
    pub fn id(&self) -> u32 {
        self.process.id()
    }

    pub fn status(
        &mut self,
    ) -> impl Future<Output = std::io::Result<std::process::ExitStatus>> + use<> {
        self.process.status()
    }
}

impl Drop for KernelProcess {
    fn drop(&mut self) {
        self.kill();
        std::fs::remove_file(&self.connection_path).ok();
    }
}

/// A kernel's Jupyter sockets.
pub struct KernelConnections {
    pub iopub: ClientIoPubConnection,
    pub shell: ClientShellConnection,
    pub control: ClientControlConnection,
    pub stdin: ClientStdinConnection,
}
