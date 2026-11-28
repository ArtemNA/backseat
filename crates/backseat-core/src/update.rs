//! Updates from the project's GitHub releases: the latest release names a version, an
//! installer `backseat-setup-X.Y.Z.exe` and `SHA256SUMS.txt`. With betas on, the newest of
//! the recent releases counts, pre-releases ("0.2.27-beta.1") included. Installing downloads the
//! installer, checks its SHA-256 against the sums, runs it silently and starts the app
//! again when it is done; the running app quits right away so its files can be replaced.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Read;

const LATEST: &str = "https://api.github.com/repos/ArtemNA/backseat/releases/latest";
/// The recent releases, pre-releases included (for betas).
const RECENT: &str = "https://api.github.com/repos/ArtemNA/backseat/releases?per_page=30";
/// Every release with its notes ("what's new"), for the settings' link.
pub const RELEASES_PAGE: &str = "https://github.com/ArtemNA/backseat/releases";
const AGENT: &str = concat!("Backseat/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, Default, Serialize)]
pub struct Release {
    pub current: String,
    pub latest: String,
    pub newer: bool,
    pub notes: String,
    pub page: String,
    #[serde(skip)]
    installer: Option<(String, String)>,
    #[serde(skip)]
    sums: Option<String>,
}

impl Release {
    /// Tests only: a newer release with nothing to download.
    pub fn for_test(latest: &str) -> Self {
        Self { latest: latest.into(), newer: true, ..Default::default() }
    }
}

/// A version for ordering, as semantic versions order: "v0.1.10" -> [0, 1, 10], no
/// pre-release; "0.2.27-beta.1" -> [0, 2, 27] and ["beta", 1]. A release comes after its
/// pre-releases (0.2.27 is newer than 0.2.27-beta.3).
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Version {
    core: Vec<u64>,
    /// None (a release) orders after Some (a pre-release).
    pre: PreRelease,
}

#[derive(PartialEq, Eq, Debug)]
struct PreRelease(Option<Vec<Ident>>);

impl PartialOrd for PreRelease {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PreRelease {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match (&self.0, &other.0) {
            (None, None) => Equal,
            (None, Some(_)) => Greater,
            (Some(_), None) => Less,
            (Some(a), Some(b)) => a.cmp(b),
        }
    }
}

/// A pre-release part: numbers order before words, numbers by value, words by text.
#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Ident {
    Num(u64),
    Word(String),
}

fn version(v: &str) -> Version {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let v = v.split('+').next().unwrap_or(v);
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let mut core: Vec<u64> = core.split('.').map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)).collect();
    // "0.2" and "0.2.0" are the same version.
    while core.len() < 3 {
        core.push(0);
    }
    let pre = pre.map(|p| p.split('.').map(|i| i.parse().map(Ident::Num).unwrap_or_else(|_| Ident::Word(i.to_string()))).collect());
    Version { core, pre: PreRelease(pre) }
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    version(latest) > version(current)
}

/// A pre-release version ("0.2.27-beta.1").
pub fn is_beta(v: &str) -> bool {
    version(v).pre.0.is_some()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(60))).build().into()
}

fn get_text(url: &str) -> Result<String, String> {
    agent()
        .get(url)
        .header("User-Agent", AGENT)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("{url}: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())
}

/// The latest release; with `betas`, the newest of the recent releases with an installer,
/// pre-releases included. STREAMER_UPDATE_URL / STREAMER_UPDATE_LIST_URL point at other JSON
/// in the same shapes (tests).
pub fn check(betas: bool) -> Result<Release, String> {
    let json = if betas {
        let url = std::env::var("STREAMER_UPDATE_LIST_URL").unwrap_or_else(|_| RECENT.to_string());
        let list: serde_json::Value = serde_json::from_str(&get_text(&url)?).map_err(|e| e.to_string())?;
        newest(&list).ok_or("no release with an installer")?
    } else {
        let url = std::env::var("STREAMER_UPDATE_URL").unwrap_or_else(|_| LATEST.to_string());
        serde_json::from_str(&get_text(&url)?).map_err(|e| e.to_string())?
    };
    from_json(&json)
}

/// Whether a release has an installer for us.
fn has_installer(r: &serde_json::Value) -> bool {
    r["assets"].as_array().into_iter().flatten().any(|a| a["name"].as_str().is_some_and(|n| {
        let n = n.to_ascii_lowercase();
        n.starts_with("backseat-setup-") && n.ends_with(".exe")
    }))
}

/// The newest published release in a list (GitHub's /releases), drafts and releases without
/// an installer left out.
fn newest(list: &serde_json::Value) -> Option<serde_json::Value> {
    list.as_array()?
        .iter()
        .filter(|r| !r["draft"].as_bool().unwrap_or(false) && has_installer(r))
        .filter_map(|r| r["tag_name"].as_str().map(|t| (version(t), r)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, r)| r.clone())
}

fn from_json(json: &serde_json::Value) -> Result<Release, String> {
    let latest = json["tag_name"].as_str().ok_or("no tag_name in the release")?.trim_start_matches(['v', 'V']).to_string();
    let current = env!("CARGO_PKG_VERSION").to_string();
    let mut installer = None;
    let mut sums = None;
    for a in json["assets"].as_array().into_iter().flatten() {
        let (Some(name), Some(link)) = (a["name"].as_str(), a["browser_download_url"].as_str()) else { continue };
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("backseat-setup-") && lower.ends_with(".exe") {
            installer = Some((name.to_string(), link.to_string()));
        } else if lower == "sha256sums.txt" {
            sums = Some(link.to_string());
        }
    }
    Ok(Release {
        newer: is_newer(&latest, &current) && installer.is_some(),
        current,
        latest,
        notes: json["body"].as_str().unwrap_or("").chars().take(4000).collect(),
        page: json["html_url"].as_str().unwrap_or("").to_string(),
        installer,
        sums,
    })
}

/// Downloads and checks the installer; returns its path.
pub fn download(r: &Release) -> Result<std::path::PathBuf, String> {
    let (name, link) = r.installer.clone().ok_or("the release has no installer")?;
    let sums_url = r.sums.clone().ok_or("the release has no SHA256SUMS.txt")?;
    let sums = get_text(&sums_url)?;
    let want = sums
        .lines()
        .find_map(|l| {
            let mut it = l.split_whitespace();
            let (hash, file) = (it.next()?, it.next()?.trim_start_matches('*'));
            file.eq_ignore_ascii_case(&name).then(|| hash.to_ascii_lowercase())
        })
        .ok_or_else(|| format!("{name} is not in SHA256SUMS.txt"))?;
    let mut body = agent().get(&link).header("User-Agent", AGENT).call().map_err(|e| format!("{link}: {e}"))?;
    let mut bytes = Vec::new();
    body.body_mut().with_config().limit(200 * 1024 * 1024).reader().read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    let got = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>();
    if got != want {
        return Err(format!("{name}: SHA-256 {got} does not match {want}"));
    }
    let dir = std::env::temp_dir().join("backseat-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(&name);
    std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Runs the installer silently in the background, then starts the app from where it runs
/// now. The caller quits right after. STREAMER_UPDATE_DRY prints the command instead.
pub fn run_installer(installer: &std::path::Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // cmd waits for the installer (start /wait), then starts the app again.
    let line = format!("start \"\" /wait \"{}\" /S & start \"\" \"{}\"", installer.display(), exe.display());
    if std::env::var_os("STREAMER_UPDATE_DRY").is_some() {
        eprintln!("update: would run: cmd /c {line}");
        return Ok(());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        std::process::Command::new("cmd")
            .arg("/c")
            .raw_arg(&line)
            .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(is_newer("0.2.0", "0.1.99"));
        assert!(!is_newer("v0.1.2", "0.1.2"));
        assert!(!is_newer("0.1.1", "0.1.2"));
        // Pre-releases: a beta is newer than the release before, older than its own release.
        assert!(is_newer("0.2.27-beta.1", "0.2.26"));
        assert!(is_newer("0.2.27", "0.2.27-beta.3"));
        assert!(!is_newer("0.2.27-beta.1", "0.2.27"));
        assert!(is_newer("0.2.27-beta.2", "0.2.27-beta.1"));
        assert!(is_newer("0.2.27-beta.10", "0.2.27-beta.9"));
        assert!(!is_newer("0.2.27-beta.1", "0.2.27-beta.1"));
        assert!(is_newer("0.2.27-rc.1", "0.2.27-beta.5"));
        assert!(!is_newer("0.2.26", "0.2.27-beta.1"));
        assert!(!is_newer("0.2", "0.2.0") && !is_newer("0.2.0", "0.2"));
        assert!(is_beta("v0.2.27-beta.1") && !is_beta("0.2.27"));
    }

    #[test]
    fn the_newest_release_is_picked() {
        let rel = |tag: &str, draft: bool, installer: bool| {
            let assets = if installer { serde_json::json!([{ "name": format!("backseat-setup-{}.exe", tag.trim_start_matches('v')), "browser_download_url": "x" }]) } else { serde_json::json!([]) };
            serde_json::json!({ "tag_name": tag, "draft": draft, "prerelease": tag.contains('-'), "assets": assets })
        };
        let list = serde_json::json!([rel("v0.2.26", false, true), rel("v0.2.27-beta.2", false, true), rel("v0.2.27-beta.3", true, true), rel("v0.2.27-beta.4", false, false), rel("v0.2.25", false, true)]);
        assert_eq!(newest(&list).unwrap()["tag_name"], "v0.2.27-beta.2");
        let list = serde_json::json!([rel("v0.2.27-beta.2", false, true), rel("v0.2.27", false, true)]);
        assert_eq!(newest(&list).unwrap()["tag_name"], "v0.2.27");
        assert!(newest(&serde_json::json!([])).is_none());
    }
}
