//! Everything the engine keeps on disk besides the index: where data lives,
//! settings, memory, project instruction files, and reading a thread's folder
//! for the file pane.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
};

use proto::{FileContent, FileEntry, McpServer, Provider, SettingsView};
use serde::{Deserialize, Serialize};

/// `$SORREL_DATA_DIR`, else the platform's per-user data folder.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SORREL_DATA_DIR") {
        return dir.into();
    }
    let home = || {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
    };
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home)
            .join("Sorrel")
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support/Sorrel")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"))
            .join("sorrel")
    }
}

/// Where a provider's CLI lives: `$SORREL_<NAME>_BIN`, else found on PATH.
pub fn resolve_bin(provider: Provider) -> PathBuf {
    let var = format!("SORREL_{}_BIN", provider.key().to_ascii_uppercase());
    if let Some(bin) = std::env::var_os(var) {
        return bin.into();
    }
    let name = match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
        other => drivers::acp::launch(other).bin,
    };
    find_on_path(name).unwrap_or_else(|| name.into())
}

/// PATH lookup that also finds npm's `.cmd` shims on Windows, which a plain
/// spawn of the bare name does not.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["exe", "cmd", "bat"]
    } else {
        &[""]
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    // Native installers put CLIs here without always fixing PATH for GUI apps.
    if let Some(home) = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        dirs.push(PathBuf::from(&home).join(".local/bin"));
    }
    dirs.iter().find_map(|dir| {
        exts.iter().find_map(|ext| {
            let candidate = if ext.is_empty() {
                dir.join(name)
            } else {
                dir.join(format!("{name}.{ext}"))
            };
            candidate.is_file().then_some(candidate)
        })
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// User-entered API keys by provider key. Never sent to clients.
    // ponytail: plain file in the user's data dir (0600 on Unix), like the CLIs' own key files; OS keychain later.
    pub api_keys: BTreeMap<String, String>,
    pub use_api_key: BTreeSet<String>,
    pub mcp_servers: Vec<McpServer>,
    pub max_sessions: usize,
}

impl Settings {
    pub fn load(dir: &Path) -> Settings {
        let mut settings: Settings = fs::read_to_string(dir.join("settings.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        if settings.max_sessions == 0 {
            settings.max_sessions = 4;
        }
        settings
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let path = dir.join("settings.json");
        fs::write(
            &path,
            serde_json::to_string_pretty(self).expect("settings serialize"),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// The key to hand a session, only when API-key mode is on for it.
    pub fn key_for(&self, provider: Provider) -> Option<String> {
        if !self.use_api_key.contains(provider.key()) {
            return None;
        }
        self.api_keys.get(provider.key()).cloned()
    }

    pub fn view(&self, data_dir: &Path) -> SettingsView {
        let pick = |keys: Vec<&String>| -> Vec<Provider> {
            keys.into_iter()
                .filter_map(|key| Provider::from_key(key))
                .collect()
        };
        SettingsView {
            api_key_set: pick(self.api_keys.keys().collect()),
            use_api_key: pick(self.use_api_key.iter().collect()),
            mcp_servers: self.mcp_servers.clone(),
            max_sessions: self.max_sessions,
            data_dir: data_dir.to_owned(),
        }
    }
}

const BEGIN: &str = "<!-- sorrel:begin -->";
const END: &str = "<!-- sorrel:end -->";

/// Splits a file into the user's own text and Sorrel's managed block.
fn split_managed(text: &str) -> (String, Option<String>) {
    match (text.find(BEGIN), text.find(END)) {
        (Some(begin), Some(end)) if begin < end => {
            let outside = format!("{}{}", &text[..begin], &text[end + END.len()..]);
            let inside = text[begin + BEGIN.len()..end].trim_matches('\n').to_owned();
            (outside, Some(inside))
        }
        _ => (text.to_owned(), None),
    }
}

/// Replaces Sorrel's block in `path`, keeping whatever else the user wrote.
fn write_managed(path: &Path, block: &str) -> io::Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let (outside, _) = split_managed(&existing);
    let outside = outside.trim();
    let text = match (outside.is_empty(), block.trim().is_empty()) {
        (true, true) => {
            if path.exists() {
                fs::remove_file(path)?;
            }
            return Ok(());
        }
        (false, true) => format!("{outside}\n"),
        (true, false) => format!("{BEGIN}\n{block}\n{END}\n"),
        (false, false) => format!("{outside}\n\n{BEGIN}\n{block}\n{END}\n"),
    };
    if text != existing {
        fs::write(path, text)?;
    }
    Ok(())
}

/// Writes a folder's instructions where both CLIs look: `CLAUDE.md` imports
/// the memory file, and `AGENTS.md` (which has no imports) carries a copy.
pub fn write_instructions(
    folder: &Path,
    instructions: &str,
    memory_file: &Path,
    memory: &str,
) -> io::Result<()> {
    let instructions = instructions.trim();
    let memory = memory.trim();
    let import = if memory.is_empty() {
        String::new()
    } else {
        format!("@{}", memory_file.display())
    };
    let claude = [instructions, import.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let memory_section = if memory.is_empty() {
        String::new()
    } else {
        format!("## Memory\n\n{memory}")
    };
    let agents = [instructions, memory_section.as_str()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    write_managed(&folder.join("CLAUDE.md"), &claude)?;
    write_managed(&folder.join("AGENTS.md"), &agents)
}

/// The project instructions Sorrel last wrote, read back from `CLAUDE.md`.
pub fn read_instructions(folder: &Path) -> String {
    let text = fs::read_to_string(folder.join("CLAUDE.md")).unwrap_or_default();
    let (_, block) = split_managed(&text);
    let block = block.unwrap_or_default();
    let lines: Vec<&str> = block.lines().collect();
    match lines.last() {
        // Drop the memory import line Sorrel adds.
        Some(last) if last.starts_with('@') => {
            lines[..lines.len() - 1].join("\n").trim().to_owned()
        }
        _ => block.trim().to_owned(),
    }
}

const SKIP_DIRS: [&str; 5] = [".git", "node_modules", "target", ".venv", "__pycache__"];
const MAX_FILES: usize = 2000;
const MAX_DEPTH: usize = 8;
const MAX_READ: u64 = 2 * 1024 * 1024;

/// The folder's files for the file pane, depth- and count-limited.
pub fn list_files(root: &Path) -> Vec<FileEntry> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_owned(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if files.len() >= MAX_FILES {
                return files;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(meta) = entry.metadata() else { continue };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if meta.is_dir() {
                if SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                files.push(FileEntry {
                    path: rel,
                    is_dir: true,
                    size: 0,
                });
                if depth < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
            } else {
                files.push(FileEntry {
                    path: rel,
                    is_dir: false,
                    size: meta.len(),
                });
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// Reads one file for the file pane. `rel` comes from a client, so it may
/// only name something inside `root`.
pub fn read_file(root: &Path, rel: &str) -> io::Result<FileContent> {
    let rel_path = Path::new(rel);
    if !rel_path
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must stay inside the folder",
        ));
    }
    let path = root.join(rel_path);
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => {
            return Ok(FileContent::Image(path));
        }
        "html" | "htm" => return Ok(FileContent::Html(path)),
        _ => {}
    }
    if fs::metadata(&path)?.len() > MAX_READ {
        return Ok(FileContent::Binary(path));
    }
    let bytes = fs::read(&path)?;
    match String::from_utf8(bytes) {
        Ok(text) if ext == "md" || ext == "markdown" => Ok(FileContent::Markdown(text)),
        Ok(text) => Ok(FileContent::Text { text, ext }),
        Err(_) => Ok(FileContent::Binary(path)),
    }
}

/// A new folder under `parent` named after `name`.
pub fn unique_dir(parent: &Path, name: &str) -> PathBuf {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    let slug = if slug.is_empty() {
        "project".to_owned()
    } else {
        slug
    };
    (1..)
        .map(|n| {
            if n == 1 {
                parent.join(&slug)
            } else {
                parent.join(format!("{slug}-{n}"))
            }
        })
        .find(|dir| !dir.exists())
        .expect("some name is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_round_trip_and_keep_the_users_own_text() {
        let dir = std::env::temp_dir().join(format!("sorrel-instr-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("CLAUDE.md"), "# Mine\n\nKeep this.\n").unwrap();
        let memory = dir.join("memory.md");

        write_instructions(&dir, "Be brief.", &memory, "Likes tea.").unwrap();
        let claude = fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert!(claude.starts_with("# Mine\n\nKeep this."));
        assert!(claude.contains(&format!("@{}", memory.display())));
        assert!(
            fs::read_to_string(dir.join("AGENTS.md"))
                .unwrap()
                .contains("Likes tea.")
        );
        assert_eq!(read_instructions(&dir), "Be brief.");

        write_instructions(&dir, "Be thorough.", &memory, "").unwrap();
        assert_eq!(read_instructions(&dir), "Be thorough.");
        assert_eq!(
            fs::read_to_string(dir.join("CLAUDE.md"))
                .unwrap()
                .matches(BEGIN)
                .count(),
            1
        );
    }

    #[test]
    fn file_reads_stay_inside_the_folder() {
        let dir = std::env::temp_dir();
        assert!(read_file(&dir, "../secret").is_err());
        assert!(read_file(&dir, "/etc/passwd").is_err());
    }
}
