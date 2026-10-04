//! Update check: compares this build with the latest GitHub release of the
//! repository it was built from (`SORREL_UPDATE_REPO`, set by the release
//! workflow). Local builds have no repository and never check.
//! `SORREL_NO_UPDATE_CHECK=1` turns it off.

use proto::Update;
use tokio::sync::mpsc;

// ponytail: tells the user and opens the download page; installing in place waits for signed builds.
pub fn check(updates: mpsc::Sender<Update>) {
    let Some(repo) = option_env!("SORREL_UPDATE_REPO") else {
        return;
    };
    if std::env::var_os("SORREL_NO_UPDATE_CHECK").is_some() {
        return;
    }
    std::thread::spawn(move || {
        let mut cmd = std::process::Command::new("curl");
        cmd.args([
            "-fsSL",
            "--max-time",
            "20",
            "-H",
            "Accept: application/vnd.github+json",
            &format!("https://api.github.com/repos/{repo}/releases/latest"),
        ]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let Ok(output) = cmd.output() else { return };
        let Ok(release) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
            return;
        };
        let tag = release["tag_name"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches('v');
        let url = release["html_url"].as_str().unwrap_or_default();
        if !url.is_empty() && newer(tag, env!("CARGO_PKG_VERSION")) {
            let _ = updates.blocking_send(Update::UpdateAvailable {
                version: tag.to_owned(),
                url: url.to_owned(),
            });
        }
    });
}

fn version(v: &str) -> Vec<u64> {
    v.split(['.', '-'])
        .map_while(|part| part.parse().ok())
        .collect()
}

fn newer(candidate: &str, current: &str) -> bool {
    let candidate = version(candidate);
    !candidate.is_empty() && candidate > version(current)
}

#[cfg(test)]
mod tests {
    #[test]
    fn compares_versions_numerically() {
        assert!(super::newer("0.10.0", "0.9.3"));
        assert!(!super::newer("0.1.0", "0.1.0"));
        assert!(!super::newer("nightly", "0.1.0"));
    }
}
