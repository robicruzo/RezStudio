//! Downloading the index: every package r-universe has indexed (CRAN,
//! Bioconductor and other universes), and CRAN's list of packages and
//! versions, which says what installs from CRAN.

use crate::IndexedPackage;
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Request};
use serde::Deserialize;
use std::sync::Arc;

/// r-universe's search with an empty query lists every indexed package, most
/// used first.
const UNIVERSE_SEARCH_URL: &str = "https://r-universe.dev/api/search?q=";
const CRAN_PACKAGES_URL: &str = "https://cloud.r-project.org/src/contrib/PACKAGES.gz";
/// Records per request; r-universe serves up to this many in about 6 MB.
const PAGE_SIZE: usize = 5000;
/// Fewer packages than this means a listing went wrong, and the last good
/// index should be kept instead.
const MIN_PACKAGES: usize = 10_000;
/// Long descriptions are cut, to keep the index small; search reads this much.
const MAX_DESCRIPTION: usize = 500;

#[derive(Deserialize)]
struct SearchPage {
    #[serde(default)]
    results: Vec<SearchResult>,
    #[serde(default)]
    total: usize,
}

#[derive(Deserialize)]
struct SearchResult {
    #[serde(rename = "Package")]
    package: String,
    #[serde(rename = "Title", default)]
    title: String,
    #[serde(rename = "Description", default)]
    description: String,
    #[serde(rename = "_user", default)]
    universe: String,
    #[serde(rename = "_usedby", default)]
    used_by: u32,
}

pub(crate) async fn fetch_index(http_client: Arc<dyn HttpClient>) -> Result<Vec<IndexedPackage>> {
    let cran_versions = parse_cran_packages(&String::from_utf8_lossy(
        &gunzip(get(&http_client, CRAN_PACKAGES_URL).await?).await?,
    ));
    let mut packages = Vec::new();
    let mut seen = HashSet::default();
    let mut skip = 0;
    loop {
        let url = format!("{UNIVERSE_SEARCH_URL}&limit={PAGE_SIZE}&skip={skip}");
        let page: SearchPage = serde_json::from_slice(&get(&http_client, &url).await?)
            .with_context(|| format!("unexpected reply from {url}"))?;
        let count = page.results.len();
        for result in page.results {
            // A package published in several universes is listed once per universe; the
            // first is the most used.
            if seen.insert(result.package.clone()) {
                packages.push(IndexedPackage {
                    cran_version: cran_versions.get(&result.package).cloned(),
                    name: result.package,
                    title: one_line(&result.title, usize::MAX),
                    description: one_line(&result.description, MAX_DESCRIPTION),
                    universe: result.universe,
                    used_by: result.used_by,
                });
            }
        }
        skip += count;
        if count == 0 || skip >= page.total {
            break;
        }
    }
    // CRAN packages r-universe hasn't indexed yet can still be found by name.
    let mut cran_only: Vec<_> = cran_versions
        .into_iter()
        .filter(|(name, _)| !seen.contains(name))
        .collect();
    cran_only.sort();
    packages.extend(cran_only.into_iter().map(|(name, version)| IndexedPackage {
        name,
        title: String::new(),
        description: String::new(),
        universe: "cran".to_string(),
        used_by: 0,
        cran_version: Some(version),
    }));
    anyhow::ensure!(
        packages.len() >= MIN_PACKAGES,
        "the package listing had only {} packages",
        packages.len()
    );
    Ok(packages)
}

async fn get(http_client: &Arc<dyn HttpClient>, url: &str) -> Result<Vec<u8>> {
    let request = Request::get(url)
        .header("Accept-Encoding", "gzip")
        .body(AsyncBody::default())?;
    let mut response = http_client
        .send(request)
        .await
        .with_context(|| format!("couldn't reach {url}"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "{url} answered {}",
        response.status()
    );
    let gzipped = response
        .headers()
        .get("content-encoding")
        .is_some_and(|encoding| encoding.as_bytes().eq_ignore_ascii_case(b"gzip"));
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    if gzipped {
        gunzip(body).await
    } else {
        Ok(body)
    }
}

async fn gunzip(bytes: Vec<u8>) -> Result<Vec<u8>> {
    let mut decoder = async_compression::futures::bufread::GzipDecoder::new(
        futures::io::BufReader::new(futures::io::Cursor::new(bytes)),
    );
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .await
        .context("couldn't decompress a download")?;
    Ok(decompressed)
}

/// Package names and versions from a CRAN `PACKAGES` file, in Debian control
/// format: blank-line separated records of `Field: value` lines.
pub(crate) fn parse_cran_packages(text: &str) -> HashMap<String, String> {
    let mut versions = HashMap::default();
    for record in text.split("\n\n") {
        let mut name = None;
        let mut version = None;
        for line in record.lines() {
            if let Some(value) = line.strip_prefix("Package:") {
                name = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("Version:") {
                version = Some(value.trim().to_string());
            }
        }
        if let (Some(name), Some(version)) = (name, version) {
            versions.insert(name, version);
        }
    }
    versions
}

/// Collapses DESCRIPTION line wrapping into one line, cut at `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.chars().count() + word.chars().count() + 1 > max {
            line.push('…');
            break;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parses_cran_packages() {
        let versions = parse_cran_packages(
            "Package: A3\nVersion: 1.0.0\nDepends: R (>= 2.15.0), xtable,\n        pbapply\n\nPackage: abc\nVersion: 2.2.1\nLicense: GPL (>= 3)\n\nLicense: no package line\n",
        );
        assert_eq!(versions.len(), 2);
        assert_eq!(versions.get("A3").map(String::as_str), Some("1.0.0"));
        assert_eq!(versions.get("abc").map(String::as_str), Some("2.2.1"));
    }

    #[test]
    fn test_one_line() {
        assert_eq!(
            one_line("A Grammar of Data\n    Manipulation", 100),
            "A Grammar of Data Manipulation"
        );
        assert_eq!(one_line("one two three four", 9), "one two…");
    }
}
