//! Zed extension host for ansible-lens-lsp.
//!
//! Zed extensions run in a `wasm32-wasip1` sandbox, so this module only locates
//! and launches the native server binary. It prefers a copy already on PATH
//! (e.g. from `cargo install`), and otherwise downloads the matching prebuilt
//! release asset from GitHub so users need no Rust toolchain.

use std::fs;

use zed_extension_api::{
    self as zed, Architecture, Command, DownloadedFileType, GithubReleaseOptions,
    LanguageServerId, LanguageServerInstallationStatus, Os, Result, Worktree,
};

const SERVER_BIN: &str = "ansible-lens-lsp";
const REPO: &str = "jan-krueger/ansible-lens";

#[derive(Default)]
struct AnsibleLensExtension {
    cached_binary_path: Option<String>,
}

impl zed::Extension for AnsibleLensExtension {
    fn new() -> Self {
        Self::default()
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        Ok(Command {
            command: self.server_binary_path(language_server_id, worktree)?,
            args: vec![],
            env: worktree.shell_env(),
        })
    }
}

impl AnsibleLensExtension {
    fn server_binary_path(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<String> {
        // A local install on PATH wins, so `cargo install` and custom builds
        // keep working and override the downloaded copy.
        if let Some(path) = worktree.which(SERVER_BIN) {
            return Ok(path);
        }

        // A binary downloaded earlier this session, if it's still there.
        if let Some(path) = &self.cached_binary_path {
            if is_file(path) {
                return Ok(path.clone());
            }
        }

        let path = download_server(language_server_id)?;
        self.cached_binary_path = Some(path.clone());
        Ok(path)
    }
}

/// Fetch the latest release, download this platform's asset if not already
/// present, and return the path to the extracted binary. Falls back to the
/// newest previously-downloaded copy when GitHub is unreachable.
fn download_server(language_server_id: &LanguageServerId) -> Result<String> {
    let (os, arch) = zed::current_platform();
    let asset_name = asset_name(os, arch);
    let binary_name = binary_name(os);

    let release = match zed::latest_github_release(
        REPO,
        GithubReleaseOptions { require_assets: true, pre_release: false },
    ) {
        Ok(release) => release,
        Err(err) => {
            return newest_local_binary(&binary_name)
                .ok_or_else(|| format!("{err} (and no downloaded server to fall back on)"));
        }
    };

    let asset = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .ok_or_else(|| format!("no release asset `{asset_name}` in {REPO} {}", release.version))?;

    let version_dir = format!("{SERVER_BIN}-{}", release.version);
    let binary_path = format!("{version_dir}/{binary_name}");

    if !is_file(&binary_path) {
        zed::set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::Downloading,
        );
        let file_type = match os {
            Os::Windows => DownloadedFileType::Zip,
            _ => DownloadedFileType::GzipTar,
        };
        zed::download_file(&asset.download_url, &version_dir, file_type)
            .map_err(|e| format!("failed to download {asset_name}: {e}"))?;
        if !matches!(os, Os::Windows) {
            zed::make_file_executable(&binary_path)
                .map_err(|e| format!("failed to mark server executable: {e}"))?;
        }
        remove_other_versions(&version_dir);
    }

    Ok(binary_path)
}

/// Release asset name for a platform, e.g. `ansible-lens-lsp-aarch64-apple-darwin.tar.gz`.
/// The `<arch>-<os>` part is a Rust target triple, matching what CI builds.
fn asset_name(os: Os, arch: Architecture) -> String {
    let arch = match arch {
        Architecture::Aarch64 => "aarch64",
        Architecture::X8664 => "x86_64",
        Architecture::X86 => "x86",
    };
    let (os, ext) = match os {
        Os::Mac => ("apple-darwin", "tar.gz"),
        Os::Linux => ("unknown-linux-gnu", "tar.gz"),
        Os::Windows => ("pc-windows-msvc", "zip"),
    };
    format!("{SERVER_BIN}-{arch}-{os}.{ext}")
}

fn binary_name(os: Os) -> String {
    match os {
        Os::Windows => format!("{SERVER_BIN}.exe"),
        _ => SERVER_BIN.to_string(),
    }
}

/// Newest already-downloaded binary, used as an offline fallback.
fn newest_local_binary(binary_name: &str) -> Option<String> {
    version_dirs()
        .into_iter()
        .map(|d| format!("{d}/{binary_name}"))
        .filter(|p| is_file(p))
        .max()
}

fn remove_other_versions(keep: &str) {
    for dir in version_dirs() {
        if dir != keep {
            let _ = fs::remove_dir_all(&dir);
        }
    }
}

/// Extension-dir entries that look like a downloaded version (`<bin>-<version>`).
fn version_dirs() -> Vec<String> {
    let prefix = format!("{SERVER_BIN}-");
    fs::read_dir(".")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(&prefix))
        .collect()
}

fn is_file(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file())
}

zed::register_extension!(AnsibleLensExtension);
