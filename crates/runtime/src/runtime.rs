//! Runs R for each project window through Ark, the R kernel Posit develops
//! for Positron, and exposes the session to the panes that use it.

mod ark;
pub mod comms;
mod lsp;
mod session;

pub use comms::{ARK_VERSION, PromptStateParams};
pub use languages::r::R_LANGUAGE_NAME;
pub use lsp::{ARK_LANGUAGE_SERVER_NAME, LSP_BRIDGE_FLAG, run_lsp_bridge};
pub use session::{RSession, SessionEvent, SessionState};

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

/// The R session of each open workspace, keyed by the workspace's entity id.
#[derive(Default)]
pub struct SessionRegistry {
    sessions: HashMap<EntityId, Entity<RSession>>,
}

/// Emitted when a workspace's R session is created, which happens after the
/// workspace (and its panels) exist.
pub struct SessionAdded {
    pub workspace_id: EntityId,
    pub session: Entity<RSession>,
}

impl EventEmitter<SessionAdded> for SessionRegistry {}

impl SessionRegistry {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalSessionRegistry>()
            .map(|registry| registry.0.clone())
    }

    pub fn session(&self, workspace_id: EntityId) -> Option<&Entity<RSession>> {
        self.sessions.get(&workspace_id)
    }

    pub(crate) fn sessions(&self) -> impl Iterator<Item = &Entity<RSession>> {
        self.sessions.values()
    }
}

struct GlobalSessionRegistry(Entity<SessionRegistry>);

impl Global for GlobalSessionRegistry {}

/// Must run after `repl::init`, which connects the ZeroMQ sockets Ark uses to
/// GPUI's executor.
pub fn init(languages: Arc<LanguageRegistry>, cx: &mut App) {
    let registry = cx.new(|_| SessionRegistry::default());
    cx.set_global(GlobalSessionRegistry(registry));
    languages.register_lsp_adapter(
        LanguageName::new_static(R_LANGUAGE_NAME),
        Arc::new(lsp::ArkLspAdapter),
    );
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let workspace_id = cx.entity_id();
        cx.on_release(move |_, cx| {
            let Some(registry) = SessionRegistry::global(cx) else {
                return;
            };
            let session =
                registry.update(cx, |registry, _| registry.sessions.remove(&workspace_id));
            if let Some(session) = session {
                session.update(cx, |session, cx| session.stop(cx)).detach();
            }
        })
        .detach();

        let project = workspace.project().clone();
        if let Some(worktree_id) = first_local_worktree(&project, cx) {
            start_session(workspace_id, project, worktree_id, window, cx);
            return;
        }
        // Worktrees are added after the workspace is created when opening a folder.
        cx.subscribe_in(&project, window, move |_, project, event, window, cx| {
            if let project::Event::WorktreeAdded(_) = event
                && !has_session(workspace_id, cx)
                && let Some(worktree_id) = first_local_worktree(project, cx)
            {
                start_session(workspace_id, project.clone(), worktree_id, window, cx);
            }
        })
        .detach();
    })
    .detach();
}

/// The R session for `workspace`, once its project has a folder open.
pub fn session_for_workspace(workspace: &Entity<Workspace>, cx: &App) -> Option<Entity<RSession>> {
    SessionRegistry::global(cx)?
        .read(cx)
        .session(workspace.entity_id())
        .cloned()
}

fn has_session(workspace_id: EntityId, cx: &App) -> bool {
    SessionRegistry::global(cx)
        .is_some_and(|registry| registry.read(cx).sessions.contains_key(&workspace_id))
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

fn start_session(
    workspace_id: EntityId,
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
    let environment = project.read(cx).environment().clone();
    let environment = environment.update(cx, |environment, cx| {
        environment.local_directory_environment(
            &task::Shell::System,
            project_root.clone().into(),
            cx,
        )
    });
    let bundled_ark = cx.path_for_auxiliary_executable("ark").ok();

    window
        .spawn(cx, async move |cx| {
            let toolchain = match active_toolchain.await {
                Some(toolchain) => Some(toolchain),
                None => available_toolchains
                    .await
                    .and_then(|toolchains| toolchains.toolchains.default_toolchain()),
            };
            let r = toolchain.as_ref().and_then(RToolchainData::from_toolchain);
            let env = environment.await.unwrap_or_default();
            cx.update(|window, cx| {
                let Some(registry) = SessionRegistry::global(cx) else {
                    return;
                };
                if has_session(workspace_id, cx) {
                    return;
                }
                let session = cx.new(|cx| RSession::new(r, project_root, env, bundled_ark, cx));
                restart_language_server_on_new_port(&session, project.downgrade(), cx);
                start_when_trusted(&session, &project, worktree_id, window, cx);
                registry.update(cx, |registry, cx| {
                    registry.sessions.insert(workspace_id, session.clone());
                    cx.emit(SessionAdded {
                        workspace_id,
                        session,
                    });
                });
            })
            .ok();
        })
        .detach();
}

/// A restarted R listens on a new port, and the bridge to the old one has
/// exited, so restart Ark's language server for the project's open buffers.
fn restart_language_server_on_new_port(
    session: &Entity<RSession>,
    project: WeakEntity<Project>,
    cx: &mut App,
) {
    let mut first_port = true;
    cx.subscribe(session, move |_, event, cx| {
        let SessionEvent::LanguageServerStarted { .. } = event else {
            return;
        };
        if std::mem::replace(&mut first_port, false) {
            return;
        }
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

/// Starts R now if the worktree is trusted, otherwise once it becomes trusted.
fn start_when_trusted(
    session: &Entity<RSession>,
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
