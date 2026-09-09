//! Self-update: poll GitHub releases, swap the executable, relaunch.
//!
//! Best-effort everywhere: a failed check only records itself in
//! [`UpdateState`] (surfaced in the window title + tray menu), never breaks
//! boot or capture. No forced updates, and no signature verification beyond
//! HTTPS — the asset comes straight from our own release over TLS.
//!
//! Asset names mirror `.github/workflows/release.yml` exactly:
//! `jlocal-<tag>-<target>.(tar.gz|zip)`, where `<tag>` is the release tag
//! (e.g. `v0.2.0`).

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;

/// Where we look for a newer release.
///
/// `…/releases/latest` answers 302 to the newest `…/releases/tag/vX.Y.Z`.
/// We read the tag off that redirect's `location` without following it, so
/// the check never touches the GitHub REST API (which 404s anonymously
/// here even though the repo is public).
pub const RELEASES_URL: &str = "https://github.com/giulianoo0/jlocal/releases/latest";

/// Boot + this often. Six hours: fresh enough to matter, quiet enough to
/// never look like phoning home.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// One check never hangs boot or the tray: slow network fails fast.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// What the window title + tray menu read. Shared behind a lock in
/// [`crate::status::AppState`].
#[derive(Clone, Debug, Default)]
pub struct UpdateState {
    /// Newest tag seen that is newer than our own build. `None` means
    /// up-to-date (or never successfully checked — see `last_error`).
    pub latest_tag: Option<String>,
    /// Last completed check, unix seconds (`0` = never).
    pub last_checked_unix: u64,
    /// Last check failure, if any. Cleared by the next success.
    pub last_error: Option<String>,
    /// A check or an install is in flight; the window says so.
    pub busy: bool,
}

/// `true` when `latest` is a newer release than `current`.
///
/// Both are tags like `v0.2.0` (the leading `v` is optional, missing parts
/// read as `0`). Anything unparseable compares as *not* newer — a weird tag
/// must never trigger a swap.
pub fn update_available(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(a), Some(b)) => b > a,
        _ => false,
    }
}

fn parse_version(tag: &str) -> Option<[u64; 3]> {
    let core = tag.trim().strip_prefix('v').unwrap_or(tag.trim());
    let mut out = [0u64; 3];
    let mut count = 0;
    for part in core.split('.') {
        if count >= 3 {
            return None;
        }
        // Reject empty / non-numeric / suffixed parts ("1rc2", "") outright.
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        out[count] = part.parse::<u64>().ok()?;
        count += 1;
    }
    if count == 0 {
        return None;
    }
    Some(out)
}

fn release_target() -> Option<(&'static str, &'static str)> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("aarch64-apple-darwin", "tar.gz"),
        ("linux", "x86_64") => ("x86_64-unknown-linux-gnu", "tar.gz"),
        ("windows", "x86_64") => ("x86_64-pc-windows-msvc", "zip"),
        _ => return None,
    })
}

/// Exact release asset file name for `tag`, or `None` where we ship nothing.
pub fn asset_filename(tag: &str) -> Option<String> {
    let (target, ext) = release_target()?;
    Some(format!("jlocal-{tag}-{target}.{ext}"))
}

/// Direct download URL for `tag`'s asset (same name `release.yml` uploads).
pub fn asset_url(tag: &str) -> Option<String> {
    asset_filename(tag)
        .map(|name| format!("https://github.com/giulianoo0/jlocal/releases/download/{tag}/{name}"))
}

/// Short-timeout client for the version check only. Redirects are disabled:
/// [`latest_tag`] reads the tag off the `…/releases/latest` 302 itself, so
/// following it would download a web page instead of a tag.
pub fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("jlocal/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(anyhow::Error::from)
}

/// Short-timeout client for asset downloads, which redirect (releases to a
/// CDN) and so need the default redirect-following policy.
pub fn download_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("jlocal/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(anyhow::Error::from)
}

/// Newest release tag (`v…`), or an error the caller records and moves on.
///
/// `GET`s [`RELEASES_URL`] without following redirects and reads the tag off
/// the 302's `location` (`…/releases/tag/vX.Y.Z`). Anything else — a non-302
/// status, a missing or odd `location`, a network error — is a best-effort
/// miss, exactly like a failed API check used to be.
pub async fn latest_tag(client: &reqwest::Client) -> anyhow::Result<String> {
    latest_tag_from(client, RELEASES_URL).await
}

/// [`latest_tag`] against an arbitrary URL, so tests can point it at a local
/// stub instead of the live releases page.
async fn latest_tag_from(client: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    let response = client.get(url).send().await?;
    if response.status() != reqwest::StatusCode::FOUND {
        anyhow::bail!("expected 302, got {}", response.status());
    }
    let location = response
        .headers()
        .get(reqwest::header::LOCATION)
        .ok_or_else(|| anyhow::anyhow!("redirect has no location header"))?
        .to_str()
        .map_err(|_| anyhow::anyhow!("redirect location is not readable"))?;
    let tag = location
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("redirect location has no tag: {location}"))?;
    if parse_version(tag).is_none() {
        anyhow::bail!("redirect location has no parseable tag: {location}");
    }
    Ok(tag.to_string())
}

/// Download + extract the executable for `tag`. Returns raw bytes, ready to
/// [`install_and_relaunch`].
pub async fn fetch_binary(client: &reqwest::Client, tag: &str) -> anyhow::Result<Vec<u8>> {
    let url =
        asset_url(tag).ok_or_else(|| anyhow::anyhow!("no release asset for this platform"))?;
    let bytes = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    extract_binary(&bytes)
}

/// Pull the `jlocal` binary out of a release archive (tar.gz on unix,
/// zip on Windows — matching what `release.yml` uploads per OS).
fn extract_binary(archive: &[u8]) -> anyhow::Result<Vec<u8>> {
    #[cfg(unix)]
    {
        let gz = flate2::read::GzDecoder::new(archive);
        let mut tar = tar::Archive::new(gz);
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_path_buf();
            if path.file_name().is_some_and(|n| n == "jlocal") {
                let mut out = Vec::with_capacity(entry.size().min(64 << 20) as usize);
                std::io::Read::read_to_end(&mut entry, &mut out)?;
                return Ok(out);
            }
        }
        anyhow::bail!("archive has no jlocal binary")
    }
    #[cfg(windows)]
    {
        let cursor = std::io::Cursor::new(archive);
        let mut zip = zip::ZipArchive::new(cursor)?;
        for i in 0..zip.len() {
            let mut file = zip.by_index(i)?;
            if Path::new(file.name())
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("jlocal.exe"))
            {
                let mut out = Vec::with_capacity(file.size().min(64 << 20) as usize);
                std::io::Read::read_to_end(&mut file, &mut out)?;
                return Ok(out);
            }
        }
        anyhow::bail!("archive has no jlocal.exe")
    }
}

/// One check: record the newest tag (when newer than us) or the failure.
/// Never throws — the scheduler calls this in a loop, the tray calls it
/// on demand. The network runs lock-free; the lock is only held for the
/// final record so the future stays `Send`.
pub async fn check_once(state: &crate::status::AppState, client: &reqwest::Client) {
    let result = latest_tag(client).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut update = state.update.lock();
    match result {
        Ok(tag) => {
            update.last_checked_unix = now;
            update.last_error = None;
            update.latest_tag = update_available(crate::status::VERSION, &tag).then_some(tag);
        }
        Err(e) => {
            update.last_checked_unix = now;
            update.last_error = Some(e.to_string());
        }
    }
}

/// Boot + every [`CHECK_INTERVAL`], forever. Calls `notify` after each
/// check so the window/tray can refresh. Best-effort: a missing client
/// (TLSless build, clock skew) just ends the task.
pub async fn run_poller(state: crate::status::AppState, notify: impl Fn() + Send + Sync + 'static) {
    let Ok(client) = client() else { return };
    loop {
        check_once(&state, &client).await;
        notify();
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

/// Swap the running executable for `binary` and relaunch in place.
///
/// - Inside `JLocal.app` (macOS bundle): replaces `Contents/MacOS/jlocal`
///   and re-opens the bundle (`open -n`), so the app keeps its identity.
/// - Plain binary (dev runs, Linux, `--no-ui`): atomically replaces the
///   current file and spawns it with the same args.
/// - Windows: renames the running exe aside (locked files can't be
///   overwritten) and writes the new one in its place; the `.old` copy is
///   cleaned on the next boot by [`cleanup_backup`].
///
/// Exits this process on success — returns only on failure.
pub fn install_and_relaunch(binary: &[u8]) -> anyhow::Result<()> {
    let current = std::env::current_exe()?;
    let parent = current
        .parent()
        .ok_or_else(|| anyhow::anyhow!("current exe has no parent dir"))?;

    #[cfg(windows)]
    {
        let backup = parent.join("jlocal.old");
        let _ = std::fs::remove_file(&backup);
        std::fs::rename(&current, &backup)?;
        std::fs::write(&current, binary)?;
        relaunch_plain(&current)?;
    }
    #[cfg(unix)]
    {
        // Write-temp + rename: atomic, and never truncates the running
        // image in place (the old inode stays mapped until we exit).
        let staged = parent.join(".jlocal-new");
        std::fs::write(&staged, binary)?;
        set_executable(&staged)?;
        std::fs::rename(&staged, &current)?;
        if let Some(bundle) = app_bundle_of(&current) {
            std::process::Command::new("open")
                .arg("-n")
                .arg(&bundle)
                .spawn()?;
        } else {
            relaunch_plain(&current)?;
        }
    }
    std::process::exit(0);
}

#[cfg(unix)]
fn set_executable(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(unix)]
fn relaunch_plain(exe: &Path) -> anyhow::Result<()> {
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .spawn()?;
    Ok(())
}

#[cfg(windows)]
fn relaunch_plain(exe: &Path) -> anyhow::Result<()> {
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .spawn()?;
    Ok(())
}

/// `<bundle>/Contents/MacOS/jlocal` → `<bundle>`, else `None`.
#[cfg(unix)]
fn app_bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()?.to_str()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()?.to_str()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?.to_path_buf();
    if bundle.extension()?.to_str()? != "app" {
        return None;
    }
    Some(bundle)
}

/// Best-effort removal of the previous Windows executable after an update.
pub fn cleanup_backup() {
    #[cfg(windows)]
    {
        if let Ok(current) = std::env::current_exe() {
            if let Some(parent) = current.parent() {
                let _ = std::fs::remove_file(parent.join("jlocal.old"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_tag_detection() {
        assert!(update_available("v0.1.0", "v0.2.0"));
        assert!(update_available("0.1.0", "v0.1.1"));
        assert!(update_available("v0.1.0", "v1.0.0"));
        assert!(!update_available("v0.2.0", "v0.2.0"));
        assert!(!update_available("v0.2.0", "v0.1.9"));
        assert!(!update_available("v1.0.0", "v0.9.9"));
        // Short tags read missing parts as zero.
        assert!(update_available("v0.1", "v0.1.1"));
        assert!(!update_available("v0.1.1", "v0.1"));
    }

    #[test]
    fn garbage_tags_never_trigger() {
        for bad in ["", "latest", "v1.2.x", "v1.2.3.4", "v-1.0.0", "v1.0.0-rc1"] {
            assert!(!update_available("v0.1.0", bad), "{bad} must not trigger");
            assert!(
                !update_available(bad, "v9.9.9"),
                "current {bad} must not trigger"
            );
        }
    }

    /// One-shot stub origin: serves a single canned response, then drops.
    async fn stub_origin(response: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/releases/latest", listener.local_addr().unwrap());
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
            }
        });
        url
    }

    #[tokio::test]
    async fn redirect_tag_is_parsed() {
        let url = stub_origin(
            "HTTP/1.1 302 Found\r\nlocation: /giulianoo0/jlocal/releases/tag/v9.9.9\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )
        .await;
        let tag = latest_tag_from(&client().unwrap(), &url).await.unwrap();
        assert_eq!(tag, "v9.9.9");
    }

    #[tokio::test]
    async fn non_redirect_is_a_miss() {
        let url = stub_origin(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
        )
        .await;
        assert!(latest_tag_from(&client().unwrap(), &url).await.is_err());
    }

    #[tokio::test]
    async fn garbage_location_is_a_miss() {
        for response in [
            "HTTP/1.1 302 Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            "HTTP/1.1 302 Found\r\nlocation: /giulianoo0/jlocal/releases\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            "HTTP/1.1 302 Found\r\nlocation: /giulianoo0/jlocal/releases/tag/latest\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        ] {
            let url = stub_origin(response).await;
            assert!(
                latest_tag_from(&client().unwrap(), &url).await.is_err(),
                "{response} must miss"
            );
        }
    }

    #[test]
    fn asset_name_matches_release_workflow() {
        // release.yml uploads dist/jlocal-<TAG>-<target>.(tar.gz|zip).
        if let Some(name) = asset_filename("v9.9.9") {
            assert!(name.starts_with("jlocal-v9.9.9-"), "{name}");
            assert!(
                name.ends_with(".tar.gz") || name.ends_with(".zip"),
                "{name}"
            );
            let url = asset_url("v9.9.9").unwrap();
            assert!(url.contains(&name), "{url}");
        }
    }

    #[test]
    fn bundle_path_detection() {
        #[cfg(unix)]
        {
            assert_eq!(
                app_bundle_of(Path::new("/Applications/JLocal.app/Contents/MacOS/jlocal")),
                Some(PathBuf::from("/Applications/JLocal.app"))
            );
            assert_eq!(app_bundle_of(Path::new("/usr/local/bin/jlocal")), None);
            assert_eq!(
                app_bundle_of(Path::new(
                    "/Applications/JLocal.app/Contents/Resources/jlocal"
                )),
                None
            );
        }
    }
}
