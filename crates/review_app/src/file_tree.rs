//! Folder tree of the changed files for the sidebar.

use std::collections::{BTreeMap, HashSet};

/// A visible row of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeEntry {
    Dir {
        /// Full directory path, used as the collapse key.
        path: String,
        /// Displayed name; chains of single-child directories are joined, e.g. `src/app`.
        name: String,
        depth: usize,
        collapsed: bool,
    },
    File {
        /// Index into the review's files.
        index: usize,
        name: String,
        depth: usize,
    },
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    /// (file name, index), kept sorted by name.
    files: Vec<(String, usize)>,
}

pub struct FileTree {
    root: Node,
}

impl FileTree {
    pub fn new<'a>(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let mut root = Node::default();
        for (index, path) in paths.into_iter().enumerate() {
            let mut node = &mut root;
            let mut parts = path.split('/').peekable();
            while let Some(part) = parts.next() {
                if parts.peek().is_some() {
                    node = node.dirs.entry(part.to_string()).or_default();
                } else {
                    node.files.push((part.to_string(), index));
                }
            }
        }
        sort_files(&mut root);
        Self { root }
    }

    /// File indices in tree order (directories first), ignoring collapsed state.
    pub fn file_order(&self) -> Vec<usize> {
        let mut order = Vec::new();
        collect_files(&self.root, &mut order);
        order
    }

    /// Rows to show given the collapsed directories.
    pub fn entries(&self, collapsed: &HashSet<String>) -> Vec<TreeEntry> {
        let mut entries = Vec::new();
        push_entries(&self.root, "", 0, collapsed, &mut entries);
        entries
    }
}

fn sort_files(node: &mut Node) {
    node.files.sort();
    for child in node.dirs.values_mut() {
        sort_files(child);
    }
}

fn collect_files(node: &Node, order: &mut Vec<usize>) {
    for child in node.dirs.values() {
        collect_files(child, order);
    }
    order.extend(node.files.iter().map(|(_, index)| *index));
}

fn push_entries(
    node: &Node,
    parent_path: &str,
    depth: usize,
    collapsed: &HashSet<String>,
    entries: &mut Vec<TreeEntry>,
) {
    for (name, mut child) in &node.dirs {
        let mut name = name.clone();
        let mut path = join(parent_path, &name);
        // Compact `a/b/c` while a directory only contains another directory.
        while child.files.is_empty() && child.dirs.len() == 1 {
            let (grandchild_name, grandchild) = child.dirs.iter().next().unwrap();
            name = format!("{name}/{grandchild_name}");
            path = join(&path, grandchild_name);
            child = grandchild;
        }
        let is_collapsed = collapsed.contains(&path);
        entries.push(TreeEntry::Dir {
            path: path.clone(),
            name,
            depth,
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            push_entries(child, &path, depth + 1, collapsed, entries);
        }
    }
    for (name, index) in &node.files {
        entries.push(TreeEntry::File {
            index: *index,
            name: name.clone(),
            depth,
        });
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn describe(entries: &[TreeEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                TreeEntry::Dir {
                    name,
                    depth,
                    collapsed,
                    ..
                } => format!(
                    "{}{name}/{}",
                    "  ".repeat(*depth),
                    if *collapsed { " +" } else { "" }
                ),
                TreeEntry::File { name, depth, index } => {
                    format!("{}{name} #{index}", "  ".repeat(*depth))
                }
            })
            .collect()
    }

    #[test]
    fn builds_compacted_tree_with_dirs_first() {
        let paths = [
            "README.md",
            "crates/app/src/main.rs",
            "crates/app/src/view.rs",
            "crates/git/src/lib.rs",
            "crates/app/Cargo.toml",
            "Cargo.toml",
        ];
        let tree = FileTree::new(paths);
        assert_eq!(
            describe(&tree.entries(&HashSet::new())),
            [
                "crates/",
                "  app/",
                "    src/",
                "      main.rs #1",
                "      view.rs #2",
                "    Cargo.toml #4",
                "  git/src/",
                "    lib.rs #3",
                "Cargo.toml #5",
                "README.md #0",
            ]
        );
        assert_eq!(tree.file_order(), [1, 2, 4, 3, 5, 0]);
    }

    #[test]
    fn collapsed_directories_hide_their_children() {
        let tree = FileTree::new(["a/x.rs", "a/b/y.rs", "z.rs"]);
        let collapsed = HashSet::from(["a".to_string()]);
        assert_eq!(describe(&tree.entries(&collapsed)), ["a/ +", "z.rs #2"]);
        // Navigation order still includes hidden files.
        assert_eq!(tree.file_order(), [1, 0, 2]);
    }
}
