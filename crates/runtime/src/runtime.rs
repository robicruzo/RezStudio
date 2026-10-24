//! Runs R for each project window through Ark, the R kernel Posit develops
//! for Positron, and exposes the session to the panes that use it.

mod ark;
mod comms;
mod session;

pub use comms::{ARK_VERSION, PromptStateParams};
pub use session::{RSession, SessionEvent, SessionState};

use collections::HashMap;
use gpui::{App, AppContext as _, Entity, EntityId, Global, Window};
use language::LanguageName;
use languages::r::{R_LANGUAGE_NAME, RToolchainData};
use project::{Project, ProjectPath, WorktreeId};
use std::sync::Arc;
use util::rel_path::RelPath;
use workspace::Workspace;

/// The R session of each open workspace, keyed by the workspace's entity id.
#[derive(Default)]
struct RSessions(HashMap<EntityId, Entity<RSession>>);

impl Global for RSessions {}

/// Must run after `repl::init`, which connects the ZeroMQ sockets Ark uses to
/// GPUI's executor.
pub fn init(cx: &mut App) {
    cx.set_global(RSessions::default());
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        let Some(window) = window else {
            return;
        };
        let workspace_id = cx.entity_id();
        cx.on_release(move |_, cx| {
            if let Some(session) = cx.global_mut::<RSessions>().0.remove(&workspace_id) {
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
                && !cx.global::<RSessions>().0.contains_key(&workspace_id)
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
    cx.try_global::<RSessions>()?
        .0
        .get(&workspace.entity_id())
        .cloned()
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
                if cx.global::<RSessions>().0.contains_key(&workspace_id) {
                    return;
                }
                let session = cx.new(|cx| RSession::new(r, project_root, env, bundled_ark, cx));
                session.update(cx, |session, cx| session.start(window, cx));
                cx.global_mut::<RSessions>().0.insert(workspace_id, session);
            })
            .ok();
        })
        .detach();
}
