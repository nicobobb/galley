//! Line comments written for a pull request but not submitted yet. They're saved as
//! they change, so closing Galley halfway through a review loses nothing.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::{PrRef, data_dir};

/// The side of the diff a comment is on, as GitHub names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DiffSide {
    /// The old version: removed lines.
    Left,
    /// The new version: added and unchanged lines.
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftComment {
    pub path: String,
    pub side: DiffSide,
    /// First line of the range; equal to `line` for a single line.
    pub start_line: u32,
    pub line: u32,
    pub body: String,
    /// The commented lines with `+`, `-` and ` ` prefixes, for display only.
    #[serde(default)]
    pub excerpt: String,
}

fn drafts_path(pr: &PrRef) -> Result<PathBuf> {
    Ok(data_dir()?
        .join("drafts")
        .join(format!("{}.json", pr.file_stem())))
}

/// The drafts saved for `pr`, oldest first.
pub fn load_drafts(pr: &PrRef) -> Result<Vec<DraftComment>> {
    load_from(&drafts_path(pr)?)
}

/// Saves `drafts` for `pr`; an empty list removes the file.
pub fn save_drafts(pr: &PrRef, drafts: &[DraftComment]) -> Result<()> {
    save_to(&drafts_path(pr)?, drafts)
}

fn load_from(path: &Path) -> Result<Vec<DraftComment>> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("invalid draft comments in {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).with_context(|| format!("couldn't read {}", path.display())),
    }
}

fn save_to(path: &Path, drafts: &[DraftComment]) -> Result<()> {
    if drafts.is_empty() {
        return match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("couldn't remove {}", path.display()))
            }
            _ => Ok(()),
        };
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(drafts)?)
        .with_context(|| format!("couldn't save {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_clears_drafts() {
        let path = std::env::temp_dir()
            .join(format!("galley_drafts_{}", std::process::id()))
            .join("pr.json");
        assert!(load_from(&path).unwrap().is_empty());
        let drafts = vec![DraftComment {
            path: "src/main.rs".into(),
            side: DiffSide::Left,
            start_line: 1,
            line: 2,
            body: "Keep this".into(),
            excerpt: "-a\n-b\n".into(),
        }];
        save_to(&path, &drafts).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains(r#""side": "LEFT""#)
        );
        assert_eq!(load_from(&path).unwrap(), drafts);
        save_to(&path, &[]).unwrap();
        assert!(!path.exists());
        save_to(&path, &[]).unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
