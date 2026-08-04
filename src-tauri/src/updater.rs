use serde::Deserialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const RELEASE_API_URL: &str = "https://api.github.com/repos/kevinagnes/voicekeyboard/releases/latest";
pub const APP_NAME: &str = "VoiceKeyboard";

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub latest_version: String,
    pub current_version: String,
    pub notes: String,
    pub url: String,
    pub dmg_url: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    body: Option<String>,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let mut parts = v.split('.');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

/// Compare two versions: returns true when `a` is newer than `b`.
pub fn is_newer(a: &str, b: &str) -> bool {
    match (parse_version(a), parse_version(b)) {
        (Some(va), Some(vb)) => va > vb,
        _ => false,
    }
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .user_agent(format!("{APP_NAME}/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("failed to build http client")
}

/// Fetch the latest release from GitHub. Returns None when the running app is
/// already up to date, Err on network/parse failures.
pub fn check() -> Result<Option<UpdateInfo>, String> {
    let resp = client()
        .get(RELEASE_API_URL)
        .send()
        .map_err(|e| format!("update check failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("update check failed: HTTP {}", resp.status()));
    }
    let body = resp.text().map_err(|e| format!("update check: read error: {e}"))?;
    let release: GithubRelease =
        serde_json::from_str(&body).map_err(|e| format!("update check: bad response: {e}"))?;

    let current = env!("CARGO_PKG_VERSION");
    if !is_newer(&release.tag_name, current) {
        return Ok(None);
    }

    let dmg_url = release
        .assets
        .iter()
        .find(|a| a.name.ends_with(".dmg"))
        .map(|a| a.browser_download_url.clone())
        .unwrap_or_default();

    Ok(Some(UpdateInfo {
        latest_version: release.tag_name.trim_start_matches('v').to_string(),
        current_version: current.to_string(),
        notes: release.body.unwrap_or_default(),
        url: release.html_url,
        dmg_url,
    }))
}

pub fn update_dir() -> PathBuf {
    crate::app_data_dir().join("updates")
}

/// Download the DMG to `update_dir()` with progress callbacks.
pub fn download(info: &UpdateInfo, progress: impl Fn(u64, u64) + Send + 'static) -> Result<PathBuf, String> {
    if info.dmg_url.is_empty() {
        return Err("release has no DMG asset".to_string());
    }
    let dir = update_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = dir.join(format!("{APP_NAME}-{}.dmg", info.latest_version));

    if dest.exists() {
        progress(1, 1);
        return Ok(dest);
    }

    let mut resp = client()
        .get(&info.dmg_url)
        .send()
        .map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);

    let part: PathBuf = {
        let mut p = dest.as_os_str().to_os_string();
        p.push(".part");
        p.into()
    };

    {
        let mut file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0u64;
        loop {
            let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            done += n as u64;
            if total > 0 {
                progress(done, total);
            }
        }
    }

    std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
    progress(1, 1);
    Ok(dest)
}

/// Path of the currently running .app bundle (three levels above the binary).
pub fn current_app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(
        exe.parent()?
            .parent()?
            .parent()?
            .to_path_buf(),
    )
}

pub fn is_running_from_mounted_dmg() -> bool {
    current_app_bundle()
        .map(|p| {
            let s = p.to_string_lossy().to_lowercase();
            s.contains("/volumes/")
        })
        .unwrap_or(false)
}

/// Mount the DMG and copy the new app bundle next to the running one, then
/// hand over to a detached shell script that swaps bundles and relaunches.
/// Returns the shell script content, or an error.
pub fn stage_and_install(dmg: &Path) -> Result<(), String> {
    use std::process::Command;

    let mount_point = update_dir().join("mnt");
    std::fs::create_dir_all(&mount_point).map_err(|e| e.to_string())?;

    let attach = Command::new("hdiutil")
        .args([
            "attach",
            "-nobrowse",
            "-readonly",
            "-mountpoint",
        ])
        .arg(&mount_point)
        .arg(dmg)
        .output()
        .map_err(|e| format!("failed to mount DMG: {e}"))?;
    if !attach.status.success() {
        return Err(format!(
            "failed to mount DMG: {}",
            String::from_utf8_lossy(&attach.stderr).trim()
        ));
    }

    let new_app = mount_point.join(format!("{APP_NAME}.app"));
    if !new_app.exists() {
        let _ = Command::new("hdiutil").args(["detach"]).arg(&mount_point).status();
        return Err(format!("mounted DMG does not contain {APP_NAME}.app"));
    }

    let current = current_app_bundle()
        .ok_or("cannot determine current app bundle path")?;
    let _install_dir = current
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or("cannot determine install directory")?;

    // Stage a fresh copy (ditto preserves signatures/symlinks).
    let staged = update_dir().join(format!("{APP_NAME}.app"));
    let _ = std::fs::remove_dir_all(&staged);
    let ditto = Command::new("ditto")
        .arg(&new_app)
        .arg(&staged)
        .status()
        .map_err(|e| format!("failed to stage app: {e}"))?;
    if !ditto.success() {
        return Err("failed to stage app bundle".to_string());
    }

    // Detach the DMG now that we have a copy.
    let _ = Command::new("hdiutil").args(["detach"]).arg(&mount_point).status();

    // Detached installer: wait for us to exit, swap bundles, relaunch.
    let installer = update_dir().join("install.sh");
    let script = format!(
        "#!/bin/sh\n\
         sleep 2\n\
         rm -rf \"{cur}\"\n\
         ditto \"{staged}\" \"{cur}\"\n\
         rm -rf \"{staged}\"\n\
         open \"{cur}\"\n",
        cur = current.display(),
        staged = staged.display(),
    );
    std::fs::write(&installer, script).map_err(|e| e.to_string())?;
    let _ = std::process::Command::new("chmod").arg("+x").arg(&installer).status();
    let _ = std::process::Command::new("nohup")
        .arg(&installer)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_and_compare() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert!(parse_version("abc").is_none());
        assert!(is_newer("v1.0.1", "1.0.0"));
        assert!(!is_newer("v1.0.0", "1.0.0"));
        assert!(is_newer("v1.1.0", "1.0.9"));
        assert!(!is_newer("v0.9.0", "1.0.0"));
    }

    #[test]
    #[ignore]
    fn live_github_check() {
        match check() {
            Ok(Some(i)) => println!("UPDATE: {} -> {} ({})", i.current_version, i.latest_version, i.dmg_url),
            Ok(None) => println!("UP TO DATE"),
            Err(e) => println!("ERR: {e}"),
        }
    }
}
