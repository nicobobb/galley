//! Repository-wide search: listing files (respecting `.gitignore`), fuzzy matching
//! file paths, and searching file contents, ripgrep style.

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use grep_matcher::Matcher as _;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::UTF8};
use ignore::WalkBuilder;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Lists the repository's files relative to `root`, with `/` separators, skipping
/// ignored files and `.git`. Sorted by path.
pub fn list_files(root: &Path, cancel: &AtomicBool) -> Vec<String> {
    let mut files = Vec::new();
    for entry in walker(root).build() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && let Ok(relative) = entry.path().strip_prefix(root)
        {
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    files.sort();
    files
}

fn walker(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        // Respect .gitignore even outside a git repo, but never descend into .git.
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git");
    builder
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyMatch {
    /// Index into the candidate list.
    pub index: usize,
    pub score: u32,
    /// Byte ranges of the matched characters, for highlighting.
    pub ranges: Vec<Range<usize>>,
}

/// Fuzzy matches `query` against file paths, best matches first. An empty query
/// matches everything in the original order.
pub fn fuzzy_match_paths(query: &str, paths: &[String], limit: usize) -> Vec<FuzzyMatch> {
    if query.trim().is_empty() {
        return (0..paths.len().min(limit))
            .map(|index| FuzzyMatch {
                index,
                score: 0,
                ranges: Vec::new(),
            })
            .collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut buffer = Vec::new();
    let mut matches = paths
        .iter()
        .enumerate()
        .filter_map(|(index, path)| {
            let score = pattern.score(Utf32Str::new(path, &mut buffer), &mut matcher)?;
            Some((index, score))
        })
        .collect::<Vec<_>>();
    // Higher score first; shorter paths win ties.
    matches.sort_by(|(a_index, a_score), (b_index, b_score)| {
        b_score
            .cmp(a_score)
            .then_with(|| paths[*a_index].len().cmp(&paths[*b_index].len()))
    });
    matches.truncate(limit);

    let mut indices = Vec::new();
    matches
        .into_iter()
        .map(|(index, score)| {
            let path = &paths[index];
            indices.clear();
            pattern.indices(Utf32Str::new(path, &mut buffer), &mut matcher, &mut indices);
            indices.sort_unstable();
            indices.dedup();
            FuzzyMatch {
                index,
                score,
                ranges: char_indices_to_byte_ranges(path, &indices),
            }
        })
        .collect()
}

/// Converts sorted char indices into merged byte ranges.
fn char_indices_to_byte_ranges(text: &str, indices: &[u32]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut wanted = indices.iter().peekable();
    for (char_index, (byte_index, ch)) in text.char_indices().enumerate() {
        if wanted
            .peek()
            .is_some_and(|&&wanted| wanted as usize == char_index)
        {
            wanted.next();
            let end = byte_index + ch.len_utf8();
            match ranges.last_mut() {
                Some(last) if last.end == byte_index => last.end = end,
                _ => ranges.push(byte_index..end),
            }
        }
    }
    ranges
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TextSearchOptions {
    /// Treat the query as a regular expression instead of literal text.
    pub regex: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextMatch {
    pub path: String,
    /// 1-based line number.
    pub line_number: u32,
    /// The line without its trailing newline.
    pub line: String,
    /// Byte ranges of the matches within `line`.
    pub ranges: Vec<Range<usize>>,
}

pub struct TextSearchResults {
    pub matches: Vec<TextMatch>,
    /// Whether the search stopped at `limit`.
    pub truncated: bool,
}

/// Searches every non-ignored text file under `root`. Case-insensitive unless the
/// query has uppercase letters. Stops early when `cancel` is set.
pub fn search_text(
    root: &Path,
    query: &str,
    options: TextSearchOptions,
    cancel: &AtomicBool,
) -> Result<TextSearchResults> {
    let matcher = RegexMatcherBuilder::new()
        .case_smart(true)
        .fixed_strings(!options.regex)
        .build(query)?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();

    let mut matches = Vec::new();
    let mut truncated = false;
    'files: for path in list_files(root, cancel) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut file_matches = Vec::new();
        let result = searcher.search_path(
            &matcher,
            root.join(&path),
            UTF8(|line_number, line| {
                let line = line.trim_end_matches(['\n', '\r']);
                let mut ranges = Vec::new();
                matcher.find_iter(line.as_bytes(), |found| {
                    ranges.push(found.start()..found.end());
                    true
                })?;
                file_matches.push(TextMatch {
                    path: path.clone(),
                    line_number: line_number as u32,
                    line: line.to_string(),
                    ranges,
                });
                Ok(true)
            }),
        );
        if result.is_err() {
            // Unreadable files (permissions, races with deletes) are skipped.
            continue;
        }
        for found in file_matches {
            if matches.len() >= options.limit {
                truncated = true;
                break 'files;
            }
            matches.push(found);
        }
    }
    Ok(TextSearchResults { matches, truncated })
}

#[cfg(test)]
// Single-range match lists are the expected values, not a mistaken range.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    fn temp_repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("review_search_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            "fn main() {\n    println!(\"Hello\");\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();
        std::fs::write(dir.join("target/out.rs"), "hello").unwrap();
        std::fs::write(dir.join(".git/config"), "hello").unwrap();
        std::fs::write(dir.join("logo.bin"), b"hello\0\x01\x02").unwrap();
        dir
    }

    #[test]
    fn lists_files_respecting_gitignore() {
        let dir = temp_repo("list");
        let files = list_files(&dir, &AtomicBool::new(false));
        assert_eq!(
            files,
            [".gitignore", "logo.bin", "src/lib.rs", "src/main.rs"]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuzzy_matches_paths_with_highlight_ranges() {
        let paths = ["src/review_view.rs", "src/main.rs", "README.md"].map(String::from);
        let matches = fuzzy_match_paths("rvw", &paths, 10);
        assert_eq!(matches[0].index, 0);
        let path = &paths[0];
        let matched = matches[0]
            .ranges
            .iter()
            .map(|range| &path[range.clone()])
            .collect::<String>();
        assert_eq!(matched.to_lowercase(), "rvw");
        assert_eq!(fuzzy_match_paths("", &paths, 2).len(), 2);
        assert!(fuzzy_match_paths("zzz", &paths, 10).is_empty());
    }

    #[test]
    fn searches_text_smart_case_skipping_binary_and_ignored() {
        let dir = temp_repo("text");
        let cancel = AtomicBool::new(false);
        let options = TextSearchOptions {
            regex: false,
            limit: 100,
        };
        let results = search_text(&dir, "hello", options, &cancel).unwrap();
        let found = results
            .matches
            .iter()
            .map(|m| (m.path.as_str(), m.line_number, m.ranges.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            found,
            [
                ("src/lib.rs", 1, vec![7..12]),
                ("src/main.rs", 2, vec![14..19])
            ]
        );

        // Uppercase makes it case-sensitive.
        let results = search_text(&dir, "Hello", options, &cancel).unwrap();
        assert_eq!(results.matches.len(), 1);

        let options = TextSearchOptions {
            regex: true,
            limit: 1,
        };
        let results = search_text(&dir, r"fn \w+", options, &cancel).unwrap();
        assert_eq!(results.matches.len(), 1);
        assert!(results.truncated);
        std::fs::remove_dir_all(&dir).ok();
    }
}
