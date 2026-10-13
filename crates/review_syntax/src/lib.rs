//! Tree-sitter based syntax highlighting, split per line so the UI can render any
//! single line of a file (e.g. a diff line) with its correct highlighting.

use std::ops::Range;
use std::sync::LazyLock;

use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// Spaces used to render a tab character.
pub const TAB_WIDTH: usize = 4;

/// Coarse token categories; the UI maps each one to a theme color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Token {
    Attribute,
    Comment,
    Constant,
    Function,
    Keyword,
    Number,
    Operator,
    Property,
    Punctuation,
    String,
    Tag,
    Type,
    Variable,
}

/// Capture names we recognize, paired with the token they map to. Tree-sitter picks
/// the longest matching prefix, so `function.method` maps via `function`.
const CAPTURES: &[(&str, Token)] = &[
    ("attribute", Token::Attribute),
    ("comment", Token::Comment),
    ("constant", Token::Constant),
    ("boolean", Token::Constant),
    ("constructor", Token::Type),
    ("escape", Token::String),
    ("function", Token::Function),
    ("keyword", Token::Keyword),
    ("label", Token::Tag),
    ("module", Token::Type),
    ("number", Token::Number),
    ("operator", Token::Operator),
    ("property", Token::Property),
    ("punctuation", Token::Punctuation),
    ("string", Token::String),
    ("tag", Token::Tag),
    ("type", Token::Type),
    ("variable.builtin", Token::Keyword),
    ("variable.parameter", Token::Variable),
];

#[derive(Debug, Clone, Default)]
pub struct HighlightedLine {
    /// Line text with tabs expanded and without the trailing newline.
    pub text: String,
    /// Byte ranges into `text`, sorted and non-overlapping.
    pub spans: Vec<(Range<usize>, Token)>,
}

/// Expands tabs and strips the trailing carriage return, so diff lines and highlighted
/// lines always have identical text and byte offsets.
pub fn normalize_line(line: &str) -> String {
    line.trim_end_matches('\r')
        .replace('\t', &" ".repeat(TAB_WIDTH))
}

/// Highlights a whole file, returning one entry per line. Returns `None` when the
/// language isn't supported (from the file extension) or parsing fails.
pub fn highlight_file(path: &str, text: &str) -> Option<Vec<HighlightedLine>> {
    let config = config_for_path(path)?;
    let text = text
        .lines()
        .map(normalize_line)
        .collect::<Vec<_>>()
        .join("\n");

    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(config, text.as_bytes(), None, None, |_| None)
        .ok()?;
    let mut stack: Vec<Token> = Vec::new();
    let mut spans: Vec<(Range<usize>, Token)> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::HighlightStart(highlight) => stack.push(CAPTURES[highlight.0].1),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
            HighlightEvent::Source { start, end } => {
                if let Some(&token) = stack.last() {
                    spans.push((start..end, token));
                }
            }
        }
    }

    Some(split_lines(&text, &spans))
}

/// Distributes file-level spans onto the lines they cover, rebasing offsets so
/// they're relative to each line.
fn split_lines(text: &str, spans: &[(Range<usize>, Token)]) -> Vec<HighlightedLine> {
    let mut lines = Vec::new();
    let mut span_index = 0;
    let mut line_start = 0;
    for line in text.split('\n') {
        let line_end = line_start + line.len();
        let mut line_spans = Vec::new();
        // Skip spans that ended before this line.
        while span_index < spans.len() && spans[span_index].0.end <= line_start {
            span_index += 1;
        }
        let mut index = span_index;
        while index < spans.len() && spans[index].0.start < line_end {
            let (range, token) = &spans[index];
            let start = range.start.max(line_start) - line_start;
            let end = range.end.min(line_end) - line_start;
            if start < end {
                line_spans.push((start..end, *token));
            }
            index += 1;
        }
        lines.push(HighlightedLine {
            text: line.to_string(),
            spans: line_spans,
        });
        line_start = line_end + 1;
    }
    lines
}

fn config_for_path(path: &str) -> Option<&'static HighlightConfiguration> {
    let extension = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())?;
    let config = match extension.as_str() {
        "rs" => &*RUST,
        "js" | "jsx" | "mjs" | "cjs" => &*JAVASCRIPT,
        "ts" | "mts" | "cts" => &*TYPESCRIPT,
        "tsx" => &*TSX,
        "py" | "pyi" => &*PYTHON,
        "go" => &*GO,
        "json" | "jsonc" => &*JSON,
        "cs" => &*C_SHARP,
        "java" => &*JAVA,
        "html" | "htm" => &*HTML,
        "css" => &*CSS,
        _ => return None,
    };
    config.as_ref()
}

type Config = LazyLock<Option<HighlightConfiguration>>;

fn build(
    language: tree_sitter_language::LanguageFn,
    name: &str,
    highlights: &str,
) -> Option<HighlightConfiguration> {
    let mut config = HighlightConfiguration::new(language.into(), name, highlights, "", "").ok()?;
    config.configure(&CAPTURES.iter().map(|(name, _)| *name).collect::<Vec<_>>());
    Some(config)
}

static RUST: Config = LazyLock::new(|| {
    build(
        tree_sitter_rust::LANGUAGE,
        "rust",
        tree_sitter_rust::HIGHLIGHTS_QUERY,
    )
});
static JAVASCRIPT: Config = LazyLock::new(|| {
    let query = format!(
        "{}\n{}",
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
    );
    build(tree_sitter_javascript::LANGUAGE, "javascript", &query)
});
// The TypeScript queries only cover TS-specific syntax and extend the JavaScript ones.
static TYPESCRIPT: Config = LazyLock::new(|| {
    let query = format!(
        "{}\n{}",
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
        tree_sitter_javascript::HIGHLIGHT_QUERY
    );
    build(
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
        "typescript",
        &query,
    )
});
static TSX: Config = LazyLock::new(|| {
    let query = format!(
        "{}\n{}\n{}",
        tree_sitter_typescript::HIGHLIGHTS_QUERY,
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
    );
    build(tree_sitter_typescript::LANGUAGE_TSX, "tsx", &query)
});
static PYTHON: Config = LazyLock::new(|| {
    build(
        tree_sitter_python::LANGUAGE,
        "python",
        tree_sitter_python::HIGHLIGHTS_QUERY,
    )
});
static GO: Config = LazyLock::new(|| {
    build(
        tree_sitter_go::LANGUAGE,
        "go",
        tree_sitter_go::HIGHLIGHTS_QUERY,
    )
});
static JSON: Config = LazyLock::new(|| {
    build(
        tree_sitter_json::LANGUAGE,
        "json",
        tree_sitter_json::HIGHLIGHTS_QUERY,
    )
});
static C_SHARP: Config = LazyLock::new(|| {
    build(
        tree_sitter_c_sharp::LANGUAGE,
        "c_sharp",
        tree_sitter_c_sharp::HIGHLIGHTS_QUERY,
    )
});
static JAVA: Config = LazyLock::new(|| {
    build(
        tree_sitter_java::LANGUAGE,
        "java",
        tree_sitter_java::HIGHLIGHTS_QUERY,
    )
});
static HTML: Config = LazyLock::new(|| {
    build(
        tree_sitter_html::LANGUAGE,
        "html",
        tree_sitter_html::HIGHLIGHTS_QUERY,
    )
});
static CSS: Config = LazyLock::new(|| {
    build(
        tree_sitter_css::LANGUAGE,
        "css",
        tree_sitter_css::HIGHLIGHTS_QUERY,
    )
});

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(line: &HighlightedLine) -> Vec<(&str, Token)> {
        line.spans
            .iter()
            .map(|(range, token)| (&line.text[range.clone()], *token))
            .collect()
    }

    #[test]
    fn highlights_rust_per_line() {
        let lines = highlight_file("main.rs", "fn main() {\n\tlet s = \"hi\"; // c\n}\n").unwrap();
        assert_eq!(lines[1].text, "    let s = \"hi\"; // c");
        let line = tokens(&lines[1]);
        assert!(line.contains(&("let", Token::Keyword)), "{line:?}");
        assert!(line.contains(&("\"hi\"", Token::String)), "{line:?}");
        assert!(line.contains(&("// c", Token::Comment)), "{line:?}");
        assert!(tokens(&lines[0]).contains(&("main", Token::Function)));
    }

    #[test]
    fn splits_multiline_spans() {
        let lines = highlight_file("a.rs", "/* one\ntwo */ fn x() {}").unwrap();
        assert_eq!(tokens(&lines[0]), [("/* one", Token::Comment)]);
        assert_eq!(tokens(&lines[1])[0], ("two */", Token::Comment));
    }

    #[test]
    fn every_language_query_compiles() {
        for path in [
            "a.rs", "a.js", "a.ts", "a.tsx", "a.py", "a.go", "a.json", "a.cs", "a.java", "a.html",
            "a.css",
        ] {
            assert!(config_for_path(path).is_some(), "query failed for {path}");
        }
        assert!(highlight_file("README", "text").is_none());
    }
}
