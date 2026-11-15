//! Runs a kernel per language for each project window and exposes its session
//! to the panes that use it. R runs through Ark, the R kernel Posit develops
//! for Positron.

mod ark;
#[cfg(any(test, feature = "test-support"))]
pub use ark::test_support;
pub mod comms;
mod kernel;
mod lsp;
mod session;

pub use ark::ArkLauncher;
pub use comms::{ARK_VERSION, PromptStateParams};
pub use kernel::{KernelConnections, KernelLauncher, KernelProcess, SessionLanguage};
pub use languages::r::R_LANGUAGE_NAME;
pub use lsp::{ARK_LANGUAGE_SERVER_NAME, LSP_BRIDGE_FLAG, run_lsp_bridge};
pub use session::{KernelSession, SessionEvent, SessionState};

use ::lsp::LanguageServerSelector;
use collections::{HashMap, HashSet};
use gpui::{App, AppContext as _, Entity, EntityId, EventEmitter, Global, WeakEntity, Window};
use language::{LanguageName, LanguageRegistry};
use languages::r::RToolchainData;
use project::trusted_worktrees::{PathTrust, TrustedWorktrees, TrustedWorktreesEvent};
use project::{Project, ProjectPath, WorktreeId};
use std::sync::Arc;
use util::ResultExt as _;
use util::rel_path::RelPath;
use workspace::Workspace;

/// The sessions of each open workspace, one per language, keyed by the
/// workspace's entity id and the language.
#[derive(Default)]
pub struct SessionRegistry {
    sessions: HashMap<(EntityId, SessionLanguage), Entity<KernelSession>>,
}

/// Emitted when a workspace's session for a language is created, which
/// happens after the workspace (and its panels) exist.
pub struct SessionAdded {
    pub workspace_id: EntityId,
    pub language: SessionLanguage,
    pub session: Entity<KernelSession>,
}

impl EventEmitter<SessionAdded> for SessionRegistry {}

impl SessionRegistry {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSessionRegistry>()
            .map(|registry| registry.0.clone())
    }

    pub fn session(
        &self,
        workspace_id: EntityId,
        language: SessionLanguage,
    ) -> Option<&Entity<KernelSession>> {
        self.sessions.get(&(workspace_id, language))
    }

    pub(crate) fn sessions(&self) -> impl Iterator<Item = &Entity<KernelSession>> {
        self.sessions.values()
    }

    fn remove_workspace(&mut self, workspace_id: EntityId) -> Vec<Entity<KernelSession>> {
        let languages = self
            .sessions
            .keys()
            .filter(|(id, _)| *id == workspace_id)
            .map(|(_, language)| *language)
            .collect::<Vec<_>>();
        languages
            .into_iter()
            .filter_map(|language| self.sessions.remove(&(workspace_id, language)))
            .collect()
    }
}

struct GlobalSessionRegistry(Entity<SessionRegistry>);

impl Global for GlobalSessionRegistry {}

/// Must run after `repl::init`, which connects the ZeroMQ sockets Ark uses to
/// GPUI's executor.
pub fn init(languages: Arc<LanguageRegistry>, cx: &mut App) {
    let registry = cx.new(|_| SessionRegistry::default());
    cx.set_global(GlobalSessionRegistry(registry));
    let adapter = Arc::new(lsp::ArkLspAdapter);
    languages.register_lsp_adapter(LanguageName::new_static(R_LANGUAGE_NAME), adapter.clone());
    // So Ark's server runs while only R Markdown or Quarto files are open, for
    // their chunks' completions. Ark reads every document as R; see
    // `ArkLspAdapter::process_diagnostics`.
    for language in lsp::R_DOCUMENT_LANGUAGES {
        languages.register_lsp_adapter(LanguageName::new_static(language), adapter.clone());
    }
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let workspace_id = cx.entity_id();
        cx.on_release(move |_, cx| {
            let Some(registry) = SessionRegistry::global(cx) else {
                return;
            };
            let sessions =
                registry.update(cx, |registry, _| registry.remove_workspace(workspace_id));
            for session in sessions {
                session.update(cx, |session, cx| session.stop(cx)).detach();
            }
        })
        .detach();

        let project = workspace.project().clone();
        if let Some(worktree_id) = first_local_worktree(&project, cx) {
            start_session(
                workspace_id,
                SessionLanguage::R,
                project,
                worktree_id,
                window,
                cx,
            );
            return;
        }
        // Worktrees are added after the workspace is created when opening a folder.
        cx.subscribe_in(&project, window, move |_, project, event, window, cx| {
            if let project::Event::WorktreeAdded(_) = event
                && !has_session(workspace_id, SessionLanguage::R, cx)
                && let Some(worktree_id) = first_local_worktree(project, cx)
            {
                start_session(
                    workspace_id,
                    SessionLanguage::R,
                    project.clone(),
                    worktree_id,
                    window,
                    cx,
                );
            }
        })
        .detach();
    })
    .detach();
}

/// The `language` session for `workspace`, once its project has a folder open.
pub fn session_for_workspace(
    workspace: &Entity<Workspace>,
    language: SessionLanguage,
    cx: &App,
) -> Option<Entity<KernelSession>> {
    SessionRegistry::global(cx)?
        .read(cx)
        .session(workspace.entity_id(), language)
        .cloned()
}

/// `text` as an R string literal, for building code to run in R.
pub fn r_string(text: &str) -> String {
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('"');
    for character in text.chars() {
        match character {
            '"' => literal.push_str("\\\""),
            '\\' => literal.push_str("\\\\"),
            '\n' => literal.push_str("\\n"),
            character => literal.push(character),
        }
    }
    literal.push('"');
    literal
}

fn has_session(workspace_id: EntityId, language: SessionLanguage, cx: &App) -> bool {
    SessionRegistry::global(cx).is_some_and(|registry| {
        registry
            .read(cx)
            .sessions
            .contains_key(&(workspace_id, language))
    })
}

fn first_local_worktree(project: &Entity<Project>, cx: &App) -> Option<WorktreeId> {
    let project = project.read(cx);
    if !project.is_local() {
        return None;
    }
    project
        .visible_worktrees(cx)
        .next()
        .map(|worktree| worktree.read(cx).id())
}

/// Finds the project's R, its active toolchain or else the default one, for
/// Ark to run.
fn ark_launcher(
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    cx: &mut App,
) -> impl Future<Output = KernelLauncher> + use<> {
    let project_path = ProjectPath {
        worktree_id,
        path: Arc::from(RelPath::empty()),
    };
    let language = LanguageName::new_static(R_LANGUAGE_NAME);
    let active_toolchain =
        project
            .read(cx)
            .active_toolchain(project_path.clone(), language.clone(), cx);
    let available_toolchains = project
        .read(cx)
        .available_toolchains(project_path, language, cx);
    let bundled_ark = cx.path_for_auxiliary_executable("ark").ok();
    async move {
        let toolchain = match active_toolchain.await {
            Some(toolchain) => Some(toolchain),
            None => available_toolchains
                .await
                .and_then(|toolchains| toolchains.toolchains.default_toolchain()),
        };
        let r = toolchain.as_ref().and_then(RToolchainData::from_toolchain);
        KernelLauncher::ark(r, bundled_ark)
    }
}

fn start_session(
    workspace_id: EntityId,
    language: SessionLanguage,
    project: Entity<Project>,
    worktree_id: WorktreeId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(project_root) = project
        .read(cx)
        .worktree_for_id(worktree_id, cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    else {
        return;
    };
    let launcher = match language {
        SessionLanguage::R => ark_launcher(&project, worktree_id, cx),
    };
    let environment = project.read(cx).environment().clone();
    let environment = environment.update(cx, |environment, cx| {
        environment.local_directory_environment(
            &task::Shell::System,
            project_root.clone().into(),
            cx,
        )
    });

    window
        .spawn(cx, async move |cx| {
            let launcher = launcher.await;
            let env = environment.await.unwrap_or_default();
            cx.update(|window, cx| {
                let Some(registry) = SessionRegistry::global(cx) else {
                    return;
                };
                if has_session(workspace_id, language, cx) {
                    return;
                }
                let session = cx.new(|cx| KernelSession::new(launcher, project_root, env, cx));
                match language {
                    SessionLanguage::R => {
                        restart_language_server_on_new_port(&session, project.downgrade(), cx)
                    }
                }
                start_when_trusted(&session, &project, worktree_id, window, cx);
                registry.update(cx, |registry, cx| {
                    registry
                        .sessions
                        .insert((workspace_id, language), session.clone());
                    cx.emit(SessionAdded {
                        workspace_id,
                        language,
                        session,
                    });
                });
            })
            .ok();
        })
        .detach();
}

/// A restarted R listens on a new port, and the bridges to the old one have
/// exited, so restart Ark's language server for the project's open buffers.
fn restart_language_server_on_new_port(
    session: &Entity<KernelSession>,
    project: WeakEntity<Project>,
    cx: &mut App,
) {
    cx.subscribe(session, move |_, event, cx| {
        let SessionEvent::LanguageServerStarted {
            restart_servers: true,
            ..
        } = event
        else {
            return;
        };
        project
            .update(cx, |project, cx| {
                let buffers = project.opened_buffers(cx);
                project.restart_language_servers_for_buffers(
                    buffers,
                    HashSet::from_iter([LanguageServerSelector::Name(ARK_LANGUAGE_SERVER_NAME)]),
                    cx,
                );
            })
            .log_err();
    })
    .detach();
}

/// Starts the kernel now if the worktree is trusted, otherwise once it becomes
/// trusted.
fn start_when_trusted(
    session: &Entity<KernelSession>,
    project: &Entity<Project>,
    worktree_id: WorktreeId,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(trusted_worktrees) = TrustedWorktrees::try_get_global(cx) else {
        session.update(cx, |session, cx| session.start(window, cx));
        return;
    };
    let worktree_store = project.read(cx).worktree_store();
    let trusted = trusted_worktrees.update(cx, |trusted_worktrees, cx| {
        trusted_worktrees.can_trust(&worktree_store, worktree_id, cx)
    });
    if trusted {
        session.update(cx, |session, cx| session.start(window, cx));
        return;
    }

    let window_handle = window.window_handle();
    let weak_session = session.downgrade();
    let subscription = cx.subscribe(&trusted_worktrees, move |_, event, cx| {
        let TrustedWorktreesEvent::Trusted(_, trusted_paths) = event else {
            return;
        };
        if !trusted_paths.contains(&PathTrust::Worktree(worktree_id)) {
            return;
        }
        let Some(session) = weak_session.upgrade() else {
            return;
        };
        window_handle
            .update(cx, |_, window, cx| {
                session.update(cx, |session, cx| session.start(window, cx));
            })
            .log_err();
    });
    session.update(cx, |session, cx| session.wait_for_trust(subscription, cx));
}
