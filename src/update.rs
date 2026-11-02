//! `ack-bot update` and `ack-bot uninstall`, the same release-branch scheme `sts` uses:
//! the release workflow pushes the built archives, their checksums, `install.sh` and a
//! `VERSION` file to the repo's `release` branch, so plain git access is enough to
//! install and update -- no token, no `gh`.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_RELEASE_REPOSITORY_URL: &str = "git@github.com:pankajyadav-blitz/slack-alert-ack.git";
const RELEASE_BRANCH: &str = "release";
const BINARY_NAME: &str = "ack-bot";

type UpdateResult<T> = Result<T, String>;

/// Replace the running binary with the build on the `release` branch when it is newer.
/// `ACK_BOT_REPO_URL` overrides the URL (e.g. an https remote).
pub fn self_update() -> UpdateResult<()> {
    let current_version = env!("CARGO_PKG_VERSION");
    let target = release_target()?;
    let archive_name = format!("{BINARY_NAME}-{target}.tar.gz");
    let staging_dir = std::env::temp_dir().join(format!("ack-bot-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging_dir);
    let outcome = clone_release_branch(&staging_dir).and_then(|()| {
        let released_tag = std::fs::read_to_string(staging_dir.join("VERSION"))
            .map_err(|_| "the release branch has no VERSION file".to_string())?;
        let released_version = released_tag.trim().trim_start_matches('v').to_string();
        if !is_newer(&released_version, current_version) {
            println!("ack-bot {current_version} is already the latest version");
            return Ok(());
        }
        println!("updating ack-bot {current_version} -> {released_version} ({target})");
        let installed_path = install_from_archive(&staging_dir, &archive_name)?;
        println!("updated {} to {released_version}", installed_path.display());
        println!("restart a running ack-bot to pick up the new version");
        Ok(())
    });
    let _ = std::fs::remove_dir_all(&staging_dir);
    outcome
}

fn clone_release_branch(destination: &Path) -> UpdateResult<()> {
    let repository_url = std::env::var("ACK_BOT_REPO_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_RELEASE_REPOSITORY_URL.to_string());
    let output = Command::new("git")
        .args(["clone", "-q", "--depth", "1", "-b", RELEASE_BRANCH, &repository_url])
        .arg(destination)
        .output()
        .map_err(|error| format!("running git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git clone of {repository_url} ({RELEASE_BRANCH} branch) failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Delete the running binary. Credentials, config and state in ~/.slack are kept.
pub fn uninstall(skip_confirmation: bool) -> UpdateResult<()> {
    let current_exe = running_binary_path()?;
    if !skip_confirmation {
        print!("Delete {}? [y/N] ", current_exe.display());
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("nothing deleted");
            return Ok(());
        }
    }
    std::fs::remove_file(&current_exe)
        .map_err(|error| format!("deleting {}: {error}", current_exe.display()))?;
    println!("deleted {}", current_exe.display());
    println!("your credentials, config and state in ~/.slack were kept");
    Ok(())
}

fn running_binary_path() -> UpdateResult<PathBuf> {
    std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| format!("locating the running ack-bot binary: {error}"))
}

fn release_target() -> UpdateResult<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        (os, arch) => return Err(format!("there is no ack-bot release for {os}/{arch}")),
    })
}

fn is_newer(candidate_version: &str, current_version: &str) -> bool {
    let numeric_parts = |version: &str| -> Vec<u64> {
        version.split('.').map(|part| part.parse().unwrap_or(0)).collect()
    };
    numeric_parts(candidate_version) > numeric_parts(current_version)
}

fn install_from_archive(staging_dir: &Path, archive_name: &str) -> UpdateResult<PathBuf> {
    let archive_path = staging_dir.join(archive_name);
    let checksum_path = staging_dir.join(format!("{archive_name}.sha256"));
    let expected_checksum = std::fs::read_to_string(&checksum_path)
        .map_err(|error| format!("reading {}: {error}", checksum_path.display()))?
        .split_whitespace()
        .next()
        .ok_or("the release's checksum file is empty")?
        .to_string();
    if sha256_of(&archive_path)? != expected_checksum {
        return Err(format!("checksum mismatch for {archive_name}; nothing was changed"));
    }
    let extracted = Command::new("tar")
        .arg("-xzf")
        .arg(&archive_path)
        .arg("-C")
        .arg(staging_dir)
        .status()
        .map_err(|error| format!("running tar: {error}"))?;
    if !extracted.success() {
        return Err(format!("tar could not extract {archive_name}"));
    }

    let current_exe = running_binary_path()?;
    let install_dir = current_exe.parent().ok_or("the ack-bot binary has no parent directory")?;
    let replacement_path = install_dir.join(".ack-bot-update");
    std::fs::copy(staging_dir.join(BINARY_NAME), &replacement_path).map_err(|error| {
        format!(
            "writing to {} ({error}; no write permission? rerun the install script instead)",
            install_dir.display()
        )
    })?;
    std::fs::set_permissions(&replacement_path, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("chmod {}: {error}", replacement_path.display()))?;
    std::fs::rename(&replacement_path, &current_exe)
        .map_err(|error| format!("replacing {}: {error}", current_exe.display()))?;
    Ok(current_exe)
}

fn sha256_of(path: &Path) -> UpdateResult<String> {
    for (program, leading_args) in [("sha256sum", &[][..]), ("shasum", &["-a", "256"][..])] {
        if let Ok(output) = Command::new(program).args(leading_args).arg(path).output()
            && output.status.success()
        {
            return Ok(String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string());
        }
    }
    Err("neither sha256sum nor shasum is available to verify the download".to_string())
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("1.1.0", "1.0.0"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("2.0.0", "1.99.0"));
        assert!(!is_newer("1.1.0", "1.1.0"));
        assert!(!is_newer("1.0.0", "1.1.0"));
    }
}
