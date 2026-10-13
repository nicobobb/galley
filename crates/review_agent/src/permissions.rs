//! Commands the agent may run without asking, per repository. Entries are command
//! prefixes (`cargo test` allows `cargo test -p foo`) or raw Claude Code permission
//! rules such as `WebFetch(domain:docs.rs)`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// The `--allowedTools` rule for an allowlist entry.
pub fn rule_for(entry: &str) -> String {
    let entry = entry.trim();
    let is_raw_rule = entry.ends_with(')')
        && entry
            .split_once('(')
            .is_some_and(|(tool, _)| !tool.is_empty() && tool.chars().all(char::is_alphanumeric));
    if is_raw_rule {
        entry.to_string()
    } else {
        format!("Bash({entry}:*)")
    }
}

/// A reasonable prefix to allow for a denied command, e.g. `npm run build -- --x`
/// becomes `npm run build` and `cargo test -p foo` becomes `cargo test`.
pub fn suggest_prefix(command: &str) -> String {
    // Only the first command of a chain (`a && b`, `a | b`, `a; b`).
    let first = command
        .split(['|', ';', '&', '\n'])
        .next()
        .unwrap_or_default();
    let words = first
        .split_whitespace()
        // Skip leading `VAR=value` assignments.
        .skip_while(|word| word.contains('=') && !word.starts_with('-'))
        .collect::<Vec<_>>();
    let Some(program) = words.first() else {
        return command.trim().to_string();
    };
    let is_argument =
        |word: &&&str| !word.starts_with('-') && !word.contains(['/', '\\', '.', '"', '\'', '$']);
    let mut prefix = vec![*program];
    if let Some(subcommand) = words.get(1).filter(is_argument) {
        prefix.push(subcommand);
        // `npm run build`, `dotnet ef migrations`, `python -m pytest` style launchers.
        if matches!(*subcommand, "run" | "exec" | "x" | "ef" | "tool")
            && let Some(target) = words.get(2).filter(is_argument)
        {
            prefix.push(target);
        }
    } else if words.get(1) == Some(&"-m")
        && let Some(module) = words.get(2)
    {
        prefix.extend(["-m", module]);
    }
    prefix.join(" ")
}

/// Allowlists for every repository, stored in the user's config directory so
/// nothing is written into the repositories themselves.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CommandSettings {
    #[serde(default)]
    allowed_commands: BTreeMap<String, Vec<String>>,
    /// Settings owned by other parts of Galley (e.g. `check_for_updates`), kept as is
    /// so saving the allowlist doesn't drop them.
    #[serde(flatten)]
    other: serde_json::Map<String, serde_json::Value>,
}

impl CommandSettings {
    /// `<config dir>/galley/settings.json`.
    pub fn default_path() -> Option<PathBuf> {
        Some(config_dir()?.join("galley").join("settings.json"))
    }

    /// The file to load from: [`Self::default_path`], or the one kept back when Galley
    /// was called Lupa if only that one exists. [`Self::save`] then writes the new path.
    pub fn existing_path() -> Option<PathBuf> {
        let path = Self::default_path()?;
        let legacy = config_dir()?.join("lupa").join("settings.json");
        Some(if !path.exists() && legacy.exists() {
            legacy
        } else {
            path
        })
    }

    /// Loads settings; a missing file means no commands are allowed yet.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("invalid settings in {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("couldn't read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("couldn't save {}", path.display()))
    }

    pub fn allowed(&self, repo_root: &Path) -> &[String] {
        self.allowed_commands
            .get(&repo_key(repo_root))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Adds `entry` unless it's empty or already present. Returns whether it changed.
    pub fn allow(&mut self, repo_root: &Path, entry: &str) -> bool {
        let entry = entry.trim();
        let list = self
            .allowed_commands
            .entry(repo_key(repo_root))
            .or_default();
        if entry.is_empty() || list.iter().any(|existing| existing == entry) {
            return false;
        }
        list.push(entry.to_string());
        true
    }

    pub fn disallow(&mut self, repo_root: &Path, entry: &str) {
        let key = repo_key(repo_root);
        if let Some(list) = self.allowed_commands.get_mut(&key) {
            list.retain(|existing| existing != entry);
            if list.is_empty() {
                self.allowed_commands.remove(&key);
            }
        }
    }
}

fn config_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }
}

/// The same repository can be spelled with either slash and drive letter case.
fn repo_key(repo_root: &Path) -> String {
    let key = repo_root.to_string_lossy().replace('\\', "/");
    let key = key.trim_end_matches('/');
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_wrap_prefixes_and_keep_raw_rules() {
        assert_eq!(rule_for("cargo test"), "Bash(cargo test:*)");
        assert_eq!(rule_for("  npm run lint "), "Bash(npm run lint:*)");
        assert_eq!(
            rule_for("WebFetch(domain:docs.rs)"),
            "WebFetch(domain:docs.rs)"
        );
        assert_eq!(rule_for("Bash(git log)"), "Bash(git log)");
        // Parentheses inside a command aren't a raw rule.
        assert_eq!(rule_for("echo (x)"), "Bash(echo (x):*)");
    }

    #[test]
    fn suggests_useful_prefixes() {
        assert_eq!(suggest_prefix("cargo test -p review_git"), "cargo test");
        assert_eq!(suggest_prefix("npm run build -- --watch"), "npm run build");
        assert_eq!(
            suggest_prefix("dotnet build Facturador.sln"),
            "dotnet build"
        );
        assert_eq!(
            suggest_prefix("python -m pytest tests/"),
            "python -m pytest"
        );
        assert_eq!(
            suggest_prefix("RUST_LOG=debug cargo run && echo ok"),
            "cargo run"
        );
        assert_eq!(suggest_prefix("node -e \"console.log(1)\""), "node");
        assert_eq!(
            suggest_prefix("./gradlew assembleDebug"),
            "./gradlew assembleDebug"
        );
        assert_eq!(suggest_prefix("ls"), "ls");
    }

    #[test]
    fn stores_allowlists_per_repo() {
        let path = std::env::temp_dir()
            .join(format!("galley_settings_{}", std::process::id()))
            .join("settings.json");
        let repo = Path::new("C:/Repos/App/");
        let mut settings = CommandSettings::load(&path).unwrap();
        assert!(settings.allowed(repo).is_empty());
        assert!(settings.allow(repo, "cargo test"));
        assert!(!settings.allow(repo, "cargo test"));
        assert!(settings.allow(repo, "cargo check"));
        settings.save(&path).unwrap();

        let mut settings = CommandSettings::load(&path).unwrap();
        let same_repo = if cfg!(windows) {
            Path::new("c:\\repos\\app")
        } else {
            repo
        };
        assert_eq!(settings.allowed(same_repo), ["cargo test", "cargo check"]);
        assert!(settings.allowed(Path::new("C:/Repos/Other")).is_empty());
        settings.disallow(repo, "cargo test");
        assert_eq!(settings.allowed(repo), ["cargo check"]);

        // Keys this struct doesn't know survive a load/save round trip.
        std::fs::write(
            &path,
            r#"{"allowed_commands": {}, "check_for_updates": false}"#,
        )
        .unwrap();
        CommandSettings::load(&path).unwrap().save(&path).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains(r#""check_for_updates": false"#)
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
