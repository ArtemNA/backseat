//! Updates from the project's GitHub releases: the latest release names a version, an
//! installer `backseat-setup-X.Y.Z.exe` and `SHA256SUMS.txt`. Installing downloads the
//! installer, checks its SHA-256 against the sums, runs it silently and starts the app
//! again when it is done; the running app quits right away so its files can be replaced.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Read;

const LATEST: &str = "https://api.github.com/repos/ArtemNA/backseat/releases/latest";
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

/// "v0.1.10" -> [0, 1, 10]
fn parts(v: &str) -> Vec<u64> {
    v.trim().trim_start_matches(['v', 'V']).split('.').map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0)).collect()
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    parts(latest) > parts(current)
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

/// The latest release. STREAMER_UPDATE_URL points at another JSON in the same shape (tests).
pub fn check() -> Result<Release, String> {
    let url = std::env::var("STREAMER_UPDATE_URL").unwrap_or_else(|_| LATEST.to_string());
    let json: serde_json::Value = serde_json::from_str(&get_text(&url)?).map_err(|e| e.to_string())?;
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
    }
}
