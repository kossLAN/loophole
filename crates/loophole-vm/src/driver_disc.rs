use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;

use crate::dirs;

const REPO: &str = "kossLAN/loophole-guest-driver";
const ASSET: &str = "loophole-guest.iso";
const PREFIX: &str = "loophole-guest-";
const KEEP: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disc {
    pub build: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub build: String,
    url: String,
    size: u64,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    size: u64,
    browser_download_url: String,
}

pub fn repo() -> String {
    std::env::var("LOOPHOLE_GUEST_REPO")
        .ok()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| REPO.to_owned())
}

pub fn pinned() -> bool {
    std::env::var_os("LOOPHOLE_DRIVER_DISC").is_some_and(|v| !v.is_empty())
}

pub fn cached() -> Option<Disc> {
    cached_in(&dirs::driver_disc_dir()).into_iter().next()
}

fn cached_in(dir: &Path) -> Vec<Disc> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut found: Vec<(SystemTime, Disc)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let build = name.strip_prefix(PREFIX)?.strip_suffix(".iso")?.to_owned();
            let modified = e.metadata().ok()?.modified().ok()?;

            Some((
                modified,
                Disc {
                    build,
                    path: e.path(),
                },
            ))
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, d)| d).collect()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(concat!("loophole/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_per_read(Some(Duration::from_secs(60)))
        .build()
        .into()
}

fn failed(e: ureq::Error, repo: &str) -> anyhow::Error {
    match e {
        ureq::Error::StatusCode(404) => anyhow!("{repo} has no release"),
        ureq::Error::StatusCode(403 | 429) => anyhow!("GitHub's rate limit, try again later"),
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Timeout(_) => {
            anyhow!("GitHub is not reachable")
        },
        e => anyhow!(e),
    }
}

pub fn latest() -> Result<Release> {
    let repo = repo();
    let release: GithubRelease = agent()
        .get(format!(
            "https://api.github.com/repos/{repo}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .and_then(|mut r| r.body_mut().read_json())
        .map_err(|e| failed(e, &repo))?;
    let build = release.tag_name;

    if build.is_empty()
        || !build
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("{repo}'s latest release has an unusable tag, {build:?}");
    }

    let asset = release
        .assets
        .into_iter()
        .find(|a| a.name == ASSET)
        .with_context(|| format!("{repo} {build} has no {ASSET}"))?;

    Ok(Release {
        build,
        url: asset.browser_download_url,
        size: asset.size,
    })
}

pub fn download(release: &Release, mut progress: impl FnMut(u64, u64)) -> Result<Disc> {
    let dir = dirs::driver_disc_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{PREFIX}{}.iso", release.build));
    // Each process writes its own part, and their renames agree.
    let part = dir.join(format!(
        ".{PREFIX}{}.{}.part",
        release.build,
        std::process::id()
    ));
    let done = fetch(release, &part, &mut progress)
        .and_then(|()| fs::rename(&part, &path).context("keeping the driver disc"));

    if done.is_err() {
        let _ = fs::remove_file(&part);
    }

    done?;
    prune(&dir);

    Ok(Disc {
        build: release.build.clone(),
        path,
    })
}

fn fetch(release: &Release, to: &Path, progress: &mut impl FnMut(u64, u64)) -> Result<()> {
    let mut response = agent()
        .get(&release.url)
        .call()
        .map_err(|e| failed(e, &repo()))?;
    let mut body = response.body_mut().as_reader();
    let mut file = File::create(to).with_context(|| format!("creating {}", to.display()))?;
    let mut buf = vec![0; 1 << 16];
    let mut received = 0;

    loop {
        let n = body.read(&mut buf).context("downloading the driver disc")?;

        if n == 0 {
            break;
        }

        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", to.display()))?;
        received += n as u64;
        progress(received, release.size);
    }

    if received != release.size {
        bail!("downloaded {received} bytes instead of {}", release.size);
    }

    file.sync_all()?;
    Ok(())
}

fn prune(dir: &Path) {
    for old in cached_in(dir).iter().skip(KEEP) {
        let _ = fs::remove_file(&old.path);
    }

    let day = Duration::from_hours(24);

    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > day));

        if stale && e.file_name().to_string_lossy().ends_with(".part") {
            let _ = fs::remove_file(e.path());
        }
    }
}
