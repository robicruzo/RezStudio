//! An offline index of R packages for search: every package on CRAN,
//! Bioconductor and R-universe with its title and description, downloaded at
//! most daily in the background and kept on disk, so search is instant and
//! works without a network.

mod fetch;
mod search;

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use http_client::HttpClient;
use search::Entry;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use util::ResultExt as _;

/// How old the index may get before it's downloaded again.
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// How often to check whether the index is due, and to retry after a failure.
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Bumped when the stored format changes, so an old file is fetched again.
const FORMAT: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct IndexedPackage {
    pub name: String,
    #[serde(default)]
    pub title: String,
    /// Cut to a few hundred characters.
    #[serde(default)]
    pub description: String,
    /// The r-universe it's published in, such as `cran`, `bioc` or `tidyverse`.
    pub universe: String,
    /// How many packages depend on it, which ranks results.
    #[serde(default)]
    pub used_by: u32,
    /// Its version on CRAN, when it's on CRAN.
    #[serde(default)]
    pub cran_version: Option<String>,
}

/// Where a package installs from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageSource {
    Cran,
    Bioconductor,
    /// Another r-universe, such as a developer's own, at `https://<name>.r-universe.dev`.
    Universe(String),
}

impl IndexedPackage {
    pub fn source(&self) -> PackageSource {
        if self.cran_version.is_some() {
            PackageSource::Cran
        } else if self.universe == "bioc" {
            PackageSource::Bioconductor
        } else {
            PackageSource::Universe(self.universe.clone())
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredIndex {
    format: u32,
    /// Seconds since the Unix epoch.
    fetched_at: u64,
    packages: Vec<IndexedPackage>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum IndexStatus {
    /// Reading the copy on disk.
    Loading,
    Ready,
    Downloading,
    /// The last download failed; search uses the last good copy, if any.
    Failed(String),
}

struct GlobalPackageIndex(Entity<PackageIndex>);

impl Global for GlobalPackageIndex {}

/// Starts the shared index, kept in RezStudio's data directory.
pub fn init(cx: &mut App) {
    let path = paths::data_dir().join("package-index.json");
    let http_client = cx.http_client();
    let index = cx.new(|cx| PackageIndex::new(path, http_client, cx));
    cx.set_global(GlobalPackageIndex(index));
}

pub struct PackageIndex {
    path: PathBuf,
    http_client: Arc<dyn HttpClient>,
    entries: Arc<Vec<Entry>>,
    fetched_at: Option<SystemTime>,
    status: IndexStatus,
    download: Option<Task<()>>,
    _schedule: Task<()>,
}

impl PackageIndex {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalPackageIndex>()
            .map(|global| global.0.clone())
    }

    pub fn new(path: PathBuf, http_client: Arc<dyn HttpClient>, cx: &mut Context<Self>) -> Self {
        let schedule = cx.spawn({
            let path = path.clone();
            async move |this, cx| {
                let loaded = cx.background_spawn(async move { load(&path) }).await;
                let Ok(()) = this.update(cx, |this, cx| {
                    match loaded {
                        Ok(Some((fetched_at, entries))) => {
                            this.entries = Arc::new(entries);
                            this.fetched_at = Some(fetched_at);
                        }
                        Ok(None) => {}
                        // A broken copy is as good as none; it's replaced by the next download.
                        Err(error) => log::warn!("ignoring the package index on disk: {error:#}"),
                    }
                    this.status = IndexStatus::Ready;
                    cx.notify();
                }) else {
                    return;
                };
                loop {
                    let due = this
                        .update(cx, |this, _| this.is_due() && this.download.is_none())
                        .unwrap_or(false);
                    if due {
                        let download = this.update(cx, |this, cx| this.start_download(cx));
                        if let Ok(Some(download)) = download {
                            download.await;
                        }
                    }
                    cx.background_executor().timer(CHECK_INTERVAL).await;
                }
            }
        });
        Self {
            path,
            http_client,
            entries: Arc::new(Vec::new()),
            fetched_at: None,
            status: IndexStatus::Loading,
            download: None,
            _schedule: schedule,
        }
    }

    pub fn status(&self) -> &IndexStatus {
        &self.status
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn fetched_at(&self) -> Option<SystemTime> {
        self.fetched_at
    }

    fn is_due(&self) -> bool {
        self.fetched_at.is_none_or(|fetched_at| {
            SystemTime::now()
                .duration_since(fetched_at)
                .is_ok_and(|age| age >= MAX_AGE)
        })
    }

    /// Downloads the index now, unless a download is already running.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.download.is_none()
            && let Some(download) = self.start_download(cx)
        {
            // Finishes on its own; `download` tracks it.
            download.detach();
        }
    }

    /// Returns a task that finishes when the download does.
    fn start_download(&mut self, cx: &mut Context<Self>) -> Option<Task<()>> {
        if self.download.is_some() {
            return None;
        }
        self.status = IndexStatus::Downloading;
        cx.notify();
        let http_client = self.http_client.clone();
        let path = self.path.clone();
        let (done_tx, done_rx) = futures::channel::oneshot::channel::<()>();
        self.download = Some(cx.spawn(async move |this, cx| {
            let downloaded = cx
                .background_spawn(async move {
                    let packages = fetch::fetch_index(http_client).await?;
                    let fetched_at = SystemTime::now();
                    store(&path, fetched_at, &packages)?;
                    anyhow::Ok((fetched_at, packages.into_iter().map(Entry::new).collect()))
                })
                .await;
            this.update(cx, |this, cx| {
                this.download = None;
                match downloaded {
                    Ok((fetched_at, entries)) => {
                        this.entries = Arc::new(entries);
                        this.fetched_at = Some(fetched_at);
                        this.status = IndexStatus::Ready;
                    }
                    Err(error) => {
                        log::warn!("couldn't download the package index: {error:#}");
                        this.status = IndexStatus::Failed(format!("{error:#}"));
                    }
                }
                cx.notify();
            })
            .log_err();
            done_tx.send(()).ok();
        }));
        Some(cx.background_spawn(async move {
            done_rx.await.ok();
        }))
    }

    /// The `limit` best matches for `query`, searched off the main thread.
    pub fn search(&self, query: &str, limit: usize, cx: &App) -> Task<Vec<IndexedPackage>> {
        let entries = self.entries.clone();
        let query = query.to_string();
        cx.background_spawn(async move {
            search::search(&entries, &query, limit)
                .into_iter()
                .filter_map(|index| entries.get(index).map(|entry| entry.package.clone()))
                .collect()
        })
    }
}

fn load(path: &PathBuf) -> Result<Option<(SystemTime, Vec<Entry>)>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let stored: StoredIndex =
        serde_json::from_slice(&bytes).context("parsing the package index")?;
    if stored.format != FORMAT {
        return Ok(None);
    }
    let fetched_at = SystemTime::UNIX_EPOCH + Duration::from_secs(stored.fetched_at);
    Ok(Some((
        fetched_at,
        stored.packages.into_iter().map(Entry::new).collect(),
    )))
}

/// Writes the index next to its final path and renames it into place, so a
/// crash mid-write never leaves a broken copy.
fn store(path: &PathBuf, fetched_at: SystemTime, packages: &[IndexedPackage]) -> Result<()> {
    let fetched_at = fetched_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|age| age.as_secs())
        .unwrap_or(0);
    let json = serde_json::to_vec(&StoredIndex {
        format: FORMAT,
        fetched_at,
        packages: packages.to_vec(),
    })?;
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let partial = path.with_extension("json.partial");
    std::fs::write(&partial, json).with_context(|| format!("writing {}", partial.display()))?;
    std::fs::rename(&partial, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncReadExt as _;
    use gpui::TestAppContext;
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fake r-universe with `count` packages, served in pages, and a CRAN
    /// that has the first half of them plus one r-universe hasn't indexed.
    fn fake_repositories(count: usize, requests: Arc<AtomicUsize>) -> Arc<dyn HttpClient> {
        FakeHttpClient::create(move |request| {
            let requests = requests.clone();
            async move {
                requests.fetch_add(1, Ordering::SeqCst);
                let uri = request.uri().to_string();
                if uri.ends_with("PACKAGES.gz") {
                    let mut text = String::new();
                    for index in 0..count / 2 {
                        text.push_str(&format!("Package: pkg{index}\nVersion: 1.{index}\n\n"));
                    }
                    text.push_str("Package: cranonly\nVersion: 0.1\n");
                    let mut gzipped = Vec::new();
                    async_compression::futures::bufread::GzipEncoder::new(
                        futures::io::BufReader::new(futures::io::Cursor::new(text.into_bytes())),
                    )
                    .read_to_end(&mut gzipped)
                    .await?;
                    return Ok(Response::builder()
                        .status(200)
                        .body(AsyncBody::from(gzipped))?);
                }
                let query: std::collections::HashMap<String, String> = request
                    .uri()
                    .query()
                    .unwrap_or_default()
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect();
                let skip: usize = query
                    .get("skip")
                    .and_then(|skip| skip.parse().ok())
                    .unwrap_or(0);
                let limit: usize = query
                    .get("limit")
                    .and_then(|limit| limit.parse().ok())
                    .unwrap_or(10);
                let results: Vec<serde_json::Value> = (skip..(skip + limit).min(count))
                    .map(|index| {
                        let universe = match index % 3 {
                            0 => "cran",
                            1 => "bioc",
                            _ => "someone",
                        };
                        serde_json::json!({
                            "Package": format!("pkg{index}"),
                            "Title": format!("Title\n    of {index}"),
                            "Description": "x ".repeat(400),
                            "_user": universe,
                            "_usedby": count - index,
                        })
                    })
                    .collect();
                let body = serde_json::json!({ "results": results, "total": count, "skip": skip, "limit": limit });
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(body.to_string()))?)
            }
        })
    }

    fn failing_repositories() -> Arc<dyn HttpClient> {
        FakeHttpClient::create(|_| async { anyhow::bail!("offline") })
    }

    #[gpui::test]
    async fn test_downloads_stores_and_reloads(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("package-index.json");
        let requests = Arc::new(AtomicUsize::new(0));
        let index = cx.new(|cx| {
            PackageIndex::new(
                path.clone(),
                fake_repositories(12_000, requests.clone()),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            requests.load(Ordering::SeqCst),
            4,
            "CRAN, then three pages of 5,000"
        );
        index.read_with(cx, |index, _| {
            assert_eq!(index.status(), &IndexStatus::Ready);
            assert_eq!(index.len(), 12_001);
            assert!(!index.is_due());
        });
        let found = index
            .read_with(cx, |index, cx| index.search("pkg1", 3, cx))
            .await;
        assert_eq!(found[0].name, "pkg1");
        assert_eq!(found[0].title, "Title of 1");
        assert!(found[0].description.chars().count() <= 500);
        let source = |name: &str, cx: &mut TestAppContext| {
            let found = index.read_with(cx, |index, cx| index.search(name, 1, cx));
            cx.foreground_executor().block_test(found)[0].source()
        };
        // On CRAN (the first half), on Bioconductor, or in another universe.
        assert_eq!(source("pkg1", cx), PackageSource::Cran);
        assert_eq!(source("pkg7000", cx), PackageSource::Bioconductor);
        assert_eq!(
            source("pkg7001", cx),
            PackageSource::Universe("someone".into())
        );
        assert_eq!(source("cranonly", cx), PackageSource::Cran);

        // A new start reads the copy on disk and doesn't download again.
        let requests = Arc::new(AtomicUsize::new(0));
        let reloaded = cx.new(|cx| {
            PackageIndex::new(
                path.clone(),
                fake_repositories(12_000, requests.clone()),
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        reloaded.read_with(cx, |index, _| assert_eq!(index.len(), 12_001));
        assert!(!directory.path().join("package-index.json.partial").exists());
    }

    #[gpui::test]
    async fn test_failures_keep_the_last_copy(cx: &mut TestAppContext) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("package-index.json");
        let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 24 * 60 * 60);
        store(
            &path,
            two_days_ago,
            &[IndexedPackage {
                name: "ggplot2".into(),
                title: "Create Elegant Data Visualisations".into(),
                description: String::new(),
                universe: "tidyverse".into(),
                used_by: 9000,
                cran_version: Some("3.5.2".into()),
            }],
        )
        .unwrap();

        // The copy is due, but the download fails: search keeps using it.
        let index = cx.new(|cx| PackageIndex::new(path.clone(), failing_repositories(), cx));
        cx.run_until_parked();
        index.read_with(cx, |index, _| {
            assert!(
                matches!(index.status(), IndexStatus::Failed(error) if error.contains("offline"))
            );
            assert_eq!(index.len(), 1);
        });
        let found = index
            .read_with(cx, |index, cx| index.search("ggpl", 5, cx))
            .await;
        assert_eq!(found.len(), 1);
        assert!(path.exists(), "a failed download doesn't touch the copy");

        // A listing that comes back nearly empty is a failure too.
        let small =
            cx.new(|cx| PackageIndex::new(path.clone(), fake_repositories(50, Arc::default()), cx));
        cx.run_until_parked();
        small.read_with(cx, |index, _| {
            assert!(matches!(index.status(), IndexStatus::Failed(error) if error.contains("only")));
            assert_eq!(index.len(), 1);
        });

        // A broken copy is ignored and replaced by the next download.
        std::fs::write(&path, b"{ not json").unwrap();
        let replaced = cx.new(|cx| {
            PackageIndex::new(path.clone(), fake_repositories(12_000, Arc::default()), cx)
        });
        cx.run_until_parked();
        replaced.read_with(cx, |index, _| {
            assert_eq!(index.status(), &IndexStatus::Ready);
            assert_eq!(index.len(), 12_001);
        });
    }

    /// Downloads the real index. Ignored by default, as it needs the network;
    /// run with `cargo test -p package_index -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn test_real_download() {
        let http_client: Arc<dyn HttpClient> = Arc::new(reqwest_client::ReqwestClient::new());
        let started = std::time::Instant::now();
        let packages = futures::executor::block_on(fetch::fetch_index(http_client)).unwrap();
        let json = serde_json::to_vec(&packages).unwrap();
        eprintln!(
            "downloaded {} packages in {:?}; {} MB stored",
            packages.len(),
            started.elapsed(),
            json.len() / 1_000_000
        );
        let find = |name: &str| {
            packages
                .iter()
                .find(|package| package.name == name)
                .unwrap_or_else(|| panic!("{name} is missing"))
        };
        assert!(packages.len() > 25_000);
        assert_eq!(find("ggplot2").source(), PackageSource::Cran);
        assert_eq!(find("DESeq2").source(), PackageSource::Bioconductor);
        assert!(find("dplyr").title.contains("Grammar"));
        let entries: Vec<Entry> = packages.into_iter().map(Entry::new).collect();
        for query in ["ggpl", "data table", "bayesian regression", "sf"] {
            let started = std::time::Instant::now();
            let found = search::search(&entries, query, 10);
            let names: Vec<&str> = found
                .iter()
                .map(|index| entries[*index].package.name.as_str())
                .collect();
            eprintln!("{query:?} in {:?}: {names:?}", started.elapsed());
        }
    }

    /// Search runs on every keystroke over the whole index.
    #[test]
    fn test_search_is_fast_on_a_full_size_index() {
        let words = [
            "data", "plot", "model", "spatial", "time", "series", "bayes", "text",
        ];
        let entries: Vec<Entry> = (0..31_500)
            .map(|index| {
                Entry::new(IndexedPackage {
                    name: format!("{}{}{index}", words[index % 8], words[(index / 8) % 8]),
                    title: format!(
                        "Tools for {} and {} Analysis",
                        words[index % 5],
                        words[index % 7]
                    ),
                    description: format!("{} ", words[index % 3]).repeat(60),
                    universe: "cran".into(),
                    used_by: (index % 997) as u32,
                    cran_version: None,
                })
            })
            .collect();
        for query in ["ggpl", "sp", "plot model", "bayesian time series", "zzzzzz"] {
            let started = std::time::Instant::now();
            let found = search::search(&entries, query, 50);
            let elapsed = started.elapsed();
            eprintln!("search {query:?}: {} results in {elapsed:?}", found.len());
            // Generous, as debug builds are several times slower than release.
            assert!(
                elapsed < Duration::from_millis(250),
                "{query:?} took {elapsed:?}"
            );
        }
    }
}
