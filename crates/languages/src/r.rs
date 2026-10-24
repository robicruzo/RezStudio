use anyhow::{Context as _, Result};
use async_trait::async_trait;
use collections::HashMap;
use futures::future::BoxFuture;
use gpui::{App, SharedString};
use language::{LanguageName, Toolchain, ToolchainList, ToolchainLister, ToolchainMetadata};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use task::ShellKind;
use util::ResultExt as _;
use util::rel_path::RelPath;

pub const R_LANGUAGE_NAME: &str = "R";

/// Where an R installation was found. Only used for display and ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RInstallationSource {
    /// `/Library/Frameworks/R.framework`, used by CRAN installers and rig on macOS.
    Framework,
    Homebrew,
    /// `/opt/R/<version>`, used by rig on Linux.
    Rig,
    System,
    /// Found only through the `R` on `PATH`.
    Path,
    /// Found only through the `R_HOME` environment variable.
    Environment,
    /// Added by the user through the toolchain picker.
    User,
}

/// The R-specific data stored in a [`Toolchain`]'s `as_json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RToolchainData {
    pub r_home: PathBuf,
    pub version: Option<String>,
    pub source: RInstallationSource,
}

impl RToolchainData {
    pub fn from_toolchain(toolchain: &Toolchain) -> Option<Self> {
        serde_json::from_value(toolchain.as_json.clone()).ok()
    }

    fn to_toolchain(&self) -> Result<Toolchain> {
        let name = match &self.version {
            Some(version) => format!("R {version}"),
            None => "R".to_string(),
        };
        Ok(Toolchain {
            name: name.into(),
            path: self.r_home.to_string_lossy().into_owned().into(),
            language_name: LanguageName::new_static(R_LANGUAGE_NAME),
            as_json: serde_json::to_value(self)?,
        })
    }
}

pub struct RToolchainLister;

#[async_trait]
impl ToolchainLister for RToolchainLister {
    async fn list(
        &self,
        _worktree_root: PathBuf,
        _subroot_relative_path: Arc<RelPath>,
        project_env: Option<HashMap<String, String>>,
    ) -> ToolchainList {
        let env = project_env.unwrap_or_default();
        let (installations, default) =
            discover_r_installations(&RSearchLocations::for_current_platform(), &env);
        let toolchains = installations
            .iter()
            .filter_map(|installation| installation.to_toolchain().log_err())
            .collect();
        ToolchainList {
            toolchains,
            default,
            groups: Box::default(),
        }
    }

    async fn resolve(
        &self,
        path: PathBuf,
        _project_env: Option<HashMap<String, String>>,
    ) -> Result<Toolchain> {
        let r_home = r_home_for_user_path(&path)
            .with_context(|| format!("No R installation found at {}", path.display()))?;
        RToolchainData {
            version: r_version(&r_home),
            r_home,
            source: RInstallationSource::User,
        }
        .to_toolchain()
    }

    fn activation_script(
        &self,
        _toolchain: &Toolchain,
        _shell: ShellKind,
        _cx: &App,
    ) -> BoxFuture<'static, Vec<String>> {
        // R needs no shell activation; the runtime passes `R_HOME` to Ark directly.
        Box::pin(async { Vec::new() })
    }

    fn meta(&self) -> ToolchainMetadata {
        ToolchainMetadata {
            term: SharedString::new_static("R Installation"),
            new_toolchain_placeholder: SharedString::new_static(
                "Path to an R installation (R_HOME) or to its R executable",
            ),
            // No manifest provider is registered for DESCRIPTION, so every R
            // file in a worktree shares the worktree root's installation.
            manifest_name: SharedString::new_static("DESCRIPTION").into(),
        }
    }
}

/// The places searched for R installations, besides `PATH` and `R_HOME`.
#[derive(Debug, Default)]
struct RSearchLocations {
    /// Directories whose subdirectories are framework versions holding `Resources`.
    framework_versions_dirs: Vec<PathBuf>,
    /// Directories whose subdirectories are rig installs holding `lib/R`.
    rig_dirs: Vec<PathBuf>,
    fixed_homes: Vec<(PathBuf, RInstallationSource)>,
}

impl RSearchLocations {
    fn for_current_platform() -> Self {
        if cfg!(target_os = "macos") {
            Self {
                framework_versions_dirs: vec![PathBuf::from(
                    "/Library/Frameworks/R.framework/Versions",
                )],
                rig_dirs: Vec::new(),
                fixed_homes: vec![
                    (
                        PathBuf::from("/opt/homebrew/opt/r/lib/R"),
                        RInstallationSource::Homebrew,
                    ),
                    (
                        PathBuf::from("/usr/local/opt/r/lib/R"),
                        RInstallationSource::Homebrew,
                    ),
                ],
            }
        } else {
            Self {
                framework_versions_dirs: Vec::new(),
                rig_dirs: vec![PathBuf::from("/opt/R")],
                fixed_homes: vec![
                    (PathBuf::from("/usr/lib/R"), RInstallationSource::System),
                    (
                        PathBuf::from("/usr/local/lib/R"),
                        RInstallationSource::System,
                    ),
                ],
            }
        }
    }
}

/// Returns the R installations found, newest version first, and the index of
/// the one to use by default: the `R` on `PATH`, matching what a terminal runs.
fn discover_r_installations(
    locations: &RSearchLocations,
    env: &HashMap<String, String>,
) -> (Vec<RToolchainData>, Option<usize>) {
    let mut candidates: Vec<(PathBuf, RInstallationSource)> = Vec::new();
    for versions_dir in &locations.framework_versions_dirs {
        for version_dir in subdirectories(versions_dir) {
            if version_dir
                .file_name()
                .is_some_and(|name| name == "Current")
            {
                continue;
            }
            candidates.push((
                version_dir.join("Resources"),
                RInstallationSource::Framework,
            ));
        }
    }
    for rig_dir in &locations.rig_dirs {
        for version_dir in subdirectories(rig_dir) {
            candidates.push((version_dir.join("lib").join("R"), RInstallationSource::Rig));
        }
    }
    candidates.extend(locations.fixed_homes.iter().cloned());

    let path_r_home = env
        .get("PATH")
        .and_then(|path_var| find_in_path("R", path_var))
        .and_then(|launcher| r_home_for_launcher(&launcher));
    if let Some(r_home) = &path_r_home {
        candidates.push((r_home.clone(), RInstallationSource::Path));
    }
    if let Some(r_home) = env.get("R_HOME") {
        candidates.push((PathBuf::from(r_home), RInstallationSource::Environment));
    }

    let mut installations: Vec<RToolchainData> = Vec::new();
    for (r_home, source) in candidates {
        let Some(r_home) = valid_r_home(&r_home) else {
            continue;
        };
        if installations
            .iter()
            .any(|installation| installation.r_home == r_home)
        {
            continue;
        }
        installations.push(RToolchainData {
            version: r_version(&r_home),
            r_home,
            source,
        });
    }

    installations.sort_by(|a, b| compare_versions(b.version.as_deref(), a.version.as_deref()));

    let default_r_home = path_r_home.as_deref().and_then(valid_r_home);
    let default = default_r_home
        .and_then(|r_home| {
            installations
                .iter()
                .position(|installation| installation.r_home == r_home)
        })
        .or_else(|| (!installations.is_empty()).then_some(0));
    (installations, default)
}

/// Canonicalizes `r_home` if it looks like an R installation.
fn valid_r_home(r_home: &Path) -> Option<PathBuf> {
    let r_home = r_home.canonicalize().ok()?;
    let has_launcher = r_home.join("bin").join("R").is_file();
    let has_base_package = base_description_path(&r_home).is_file();
    (has_launcher && has_base_package).then_some(r_home)
}

fn base_description_path(r_home: &Path) -> PathBuf {
    r_home.join("library").join("base").join("DESCRIPTION")
}

/// Reads the version from the base package, which every R installation ships.
fn r_version(r_home: &Path) -> Option<String> {
    let description = std::fs::read_to_string(base_description_path(r_home)).ok()?;
    description.lines().find_map(|line| {
        line.strip_prefix("Version:")
            .map(|version| version.trim().to_string())
            .filter(|version| !version.is_empty())
    })
}

/// The `R` launcher is a shell script that hard-codes its installation as
/// `R_HOME_DIR=...`, which is cheaper and safer to read than running R.
fn r_home_for_launcher(launcher: &Path) -> Option<PathBuf> {
    let launcher = launcher.canonicalize().ok()?;
    let script = std::fs::read_to_string(&launcher).ok()?;
    let r_home = script.lines().find_map(|line| {
        let value = line.trim().strip_prefix("R_HOME_DIR=")?;
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        (!value.is_empty()).then(|| PathBuf::from(value))
    })?;
    valid_r_home(&r_home)
}

fn r_home_for_user_path(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return r_home_for_launcher(path);
    }
    valid_r_home(path).or_else(|| {
        let parent = path.parent()?;
        (path.file_name()? == "bin")
            .then(|| valid_r_home(parent))
            .flatten()
    })
}

fn find_in_path(program: &str, path_var: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut directories: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    directories.sort();
    directories
}

/// Orders dotted version strings numerically, with unknown versions last.
fn compare_versions(a: Option<&str>, b: Option<&str>) -> Ordering {
    let parse = |version: &str| -> Vec<u64> {
        version
            .split(['.', '-'])
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    };
    match (a, b) {
        (Some(a), Some(b)) => parse(a).cmp(&parse(b)),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_r_home(r_home: &Path, version: &str) {
        fs::create_dir_all(r_home.join("bin")).unwrap();
        fs::write(r_home.join("bin").join("R"), "#!/bin/sh\n").unwrap();
        fs::create_dir_all(r_home.join("library").join("base")).unwrap();
        fs::write(
            base_description_path(r_home),
            format!("Package: base\nVersion: {version}\nPriority: base\n"),
        )
        .unwrap();
    }

    fn make_launcher(dir: &Path, r_home: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let launcher = dir.join("R");
        fs::write(
            &launcher,
            format!(
                "#!/bin/bash\n# Shell wrapper for R executable.\n\nR_HOME_DIR={}\nexport R_HOME=\"${{R_HOME_DIR}}\"\n",
                r_home.display()
            ),
        )
        .unwrap();
        launcher
    }

    #[test]
    fn test_discovers_framework_versions_and_defaults_to_path() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let versions = root.join("R.framework").join("Versions");
        make_r_home(&versions.join("4.4-arm64").join("Resources"), "4.4.2");
        make_r_home(&versions.join("4.5-arm64").join("Resources"), "4.5.3");
        #[cfg(unix)]
        std::os::unix::fs::symlink(versions.join("4.4-arm64"), versions.join("Current")).unwrap();

        let homebrew = root.join("homebrew").join("lib").join("R");
        make_r_home(&homebrew, "4.5.1");

        // The launcher on PATH points at the framework through `Current`, the way
        // `/usr/local/bin/R` does after a CRAN install.
        let bin = root.join("bin");
        make_launcher(&bin, &versions.join("Current").join("Resources"));

        let locations = RSearchLocations {
            framework_versions_dirs: vec![versions],
            rig_dirs: Vec::new(),
            fixed_homes: vec![
                (homebrew, RInstallationSource::Homebrew),
                (root.join("missing"), RInstallationSource::System),
            ],
        };
        let env = HashMap::from_iter([("PATH".to_string(), bin.to_string_lossy().into_owned())]);

        let (installations, default) = discover_r_installations(&locations, &env);
        let found = installations
            .iter()
            .map(|installation| (installation.version.as_deref(), installation.source))
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            [
                (Some("4.5.3"), RInstallationSource::Framework),
                (Some("4.5.1"), RInstallationSource::Homebrew),
                (Some("4.4.2"), RInstallationSource::Framework),
            ]
        );
        assert_eq!(
            default.and_then(|index| installations[index].version.as_deref()),
            Some("4.4.2")
        );
    }

    #[test]
    fn test_path_only_installation_and_r_home_env() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let custom = root.join("custom").join("lib").join("R");
        make_r_home(&custom, "4.3.0");
        let bin = make_launcher(&root.join("bin"), &custom);
        let from_env = root.join("env-r");
        make_r_home(&from_env, "4.2.1");

        let env = HashMap::from_iter([
            (
                "PATH".to_string(),
                bin.parent().unwrap().to_string_lossy().into_owned(),
            ),
            (
                "R_HOME".to_string(),
                from_env.to_string_lossy().into_owned(),
            ),
        ]);
        let (installations, default) = discover_r_installations(&RSearchLocations::default(), &env);
        let found = installations
            .iter()
            .map(|installation| (installation.r_home.clone(), installation.source))
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            [
                (custom, RInstallationSource::Path),
                (from_env, RInstallationSource::Environment),
            ]
        );
        assert_eq!(default, Some(0));
    }

    #[test]
    fn test_no_installations() {
        let (installations, default) =
            discover_r_installations(&RSearchLocations::default(), &HashMap::default());
        assert!(installations.is_empty());
        assert_eq!(default, None);
    }

    #[test]
    fn test_resolve_user_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let r_home = root.join("R");
        make_r_home(&r_home, "4.5.0");
        let launcher = make_launcher(&root.join("bin"), &r_home);

        assert_eq!(r_home_for_user_path(&r_home), Some(r_home.clone()));
        assert_eq!(
            r_home_for_user_path(&r_home.join("bin")),
            Some(r_home.clone())
        );
        assert_eq!(r_home_for_user_path(&launcher), Some(r_home));
        assert_eq!(r_home_for_user_path(&root.join("nothing")), None);
    }

    #[test]
    fn test_toolchain_round_trip() {
        let data = RToolchainData {
            r_home: PathBuf::from("/opt/R/4.5.3/lib/R"),
            version: Some("4.5.3".to_string()),
            source: RInstallationSource::Rig,
        };
        let toolchain = data.to_toolchain().unwrap();
        assert_eq!(toolchain.name.as_ref(), "R 4.5.3");
        assert_eq!(toolchain.path.as_ref(), "/opt/R/4.5.3/lib/R");
        assert_eq!(RToolchainData::from_toolchain(&toolchain), Some(data));
    }

    #[test]
    fn test_compare_versions() {
        assert_eq!(
            compare_versions(Some("4.10.0"), Some("4.9.1")),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions(Some("4.5.3"), Some("4.5.3")),
            Ordering::Equal
        );
        assert_eq!(compare_versions(None, Some("3.6.0")), Ordering::Less);
    }
}
