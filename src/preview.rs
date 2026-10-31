use std::path::Path;

use crate::theme;

use tree_sitter::Language;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

pub const MAX_PREVIEW_LINES: usize = 500;
pub const MATCH_CONTEXT_BEFORE: usize = 8;

#[derive(Clone)]
pub struct HighlightedSpan {
    pub color: u32,
    pub bg: Option<u32>,
    pub italic: bool,
    pub bold: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub text: String,
}

#[derive(Clone)]
pub struct HighlightedLine {
    pub spans: Vec<HighlightedSpan>,
}

struct TreeSitterLanguageSpec {
    language: Language,
    name: &'static str,
    highlights_query: &'static str,
    injections_query: &'static str,
    locals_query: &'static str,
}

fn syntax_set_for_path(path: &Path) -> Option<TreeSitterLanguageSpec> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();

    match extension.as_str() {
        "rs" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_rust::LANGUAGE.into(),
            name: "rust",
            highlights_query: tree_sitter_rust::HIGHLIGHTS_QUERY,
            injections_query: tree_sitter_rust::INJECTIONS_QUERY,
            locals_query: "",
        }),
        "swift" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_swift::LANGUAGE.into(),
            name: "swift",
            highlights_query: tree_sitter_swift::HIGHLIGHTS_QUERY,
            injections_query: tree_sitter_swift::INJECTIONS_QUERY,
            locals_query: tree_sitter_swift::LOCALS_QUERY,
        }),
        "js" | "mjs" | "cjs" | "jsx" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_javascript::LANGUAGE.into(),
            name: "javascript",
            highlights_query: if extension == "jsx" {
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY
            } else {
                tree_sitter_javascript::HIGHLIGHT_QUERY
            },
            injections_query: tree_sitter_javascript::INJECTIONS_QUERY,
            locals_query: tree_sitter_javascript::LOCALS_QUERY,
        }),
        "ts" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            name: "typescript",
            highlights_query: tree_sitter_typescript::HIGHLIGHTS_QUERY,
            injections_query: "",
            locals_query: tree_sitter_typescript::LOCALS_QUERY,
        }),
        "tsx" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_typescript::LANGUAGE_TSX.into(),
            name: "tsx",
            highlights_query: tree_sitter_typescript::HIGHLIGHTS_QUERY,
            injections_query: "",
            locals_query: tree_sitter_typescript::LOCALS_QUERY,
        }),
        "go" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_go::LANGUAGE.into(),
            name: "go",
            highlights_query: tree_sitter_go::HIGHLIGHTS_QUERY,
            injections_query: "",
            locals_query: "",
        }),
        "py" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_python::LANGUAGE.into(),
            name: "python",
            highlights_query: tree_sitter_python::HIGHLIGHTS_QUERY,
            injections_query: "",
            locals_query: "",
        }),
        "json" | "jsonc" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_json::LANGUAGE.into(),
            name: "json",
            highlights_query: tree_sitter_json::HIGHLIGHTS_QUERY,
            injections_query: "",
            locals_query: "",
        }),
        "yaml" | "yml" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_yaml::LANGUAGE.into(),
            name: "yaml",
            highlights_query: tree_sitter_yaml::HIGHLIGHTS_QUERY,
            injections_query: "",
            locals_query: "",
        }),
        "sh" | "bash" | "zsh" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_bash::LANGUAGE.into(),
            name: "bash",
            highlights_query: tree_sitter_bash::HIGHLIGHT_QUERY,
            injections_query: "",
            locals_query: "",
        }),
        "c" | "h" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_c::LANGUAGE.into(),
            name: "c",
            highlights_query: tree_sitter_c::HIGHLIGHT_QUERY,
            injections_query: "",
            locals_query: "",
        }),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_cpp::LANGUAGE.into(),
            name: "cpp",
            highlights_query: tree_sitter_cpp::HIGHLIGHT_QUERY,
            injections_query: "",
            locals_query: "",
        }),
        "css" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_css::LANGUAGE.into(),
            name: "css",
            highlights_query: tree_sitter_css::HIGHLIGHTS_QUERY,
            injections_query: "",
            locals_query: "",
        }),
        "html" | "htm" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_html::LANGUAGE.into(),
            name: "html",
            highlights_query: tree_sitter_html::HIGHLIGHTS_QUERY,
            injections_query: tree_sitter_html::INJECTIONS_QUERY,
            locals_query: "",
        }),
        "md" | "markdown" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_md::LANGUAGE.into(),
            name: "markdown",
            highlights_query: tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            injections_query: tree_sitter_md::INJECTION_QUERY_BLOCK,
            locals_query: "",
        }),
        "regex" | "re" => Some(TreeSitterLanguageSpec {
            language: tree_sitter_regex::LANGUAGE.into(),
            name: "regex",
            highlights_query: tree_sitter_regex::HIGHLIGHTS_QUERY,
            injections_query: "",
            locals_query: "",
        }),
        _ => None,
    }
}

fn build_highlight_config(spec: &TreeSitterLanguageSpec) -> Option<HighlightConfiguration> {
    let mut config = HighlightConfiguration::new(
        spec.language.clone(),
        spec.name,
        spec.highlights_query,
        spec.injections_query,
        spec.locals_query,
    )
    .ok()?;
    let capture_names: Vec<String> = config
        .query
        .capture_names()
        .into_iter()
        .map(|name| name.to_string())
        .collect();
    config.configure(&capture_names);
    Some(config)
}

fn append_span(
    line: &mut HighlightedLine,
    color: u32,
    italic: bool,
    bold: bool,
    underline: bool,
    strikethrough: bool,
    text: &str,
) {
    if text.is_empty() {
        return;
    }

    if let Some(last) = line.spans.last_mut()
        && last.color == color
        && last.italic == italic
        && last.bold == bold
        && last.underline == underline
        && last.strikethrough == strikethrough
    {
        last.text.push_str(text);
        return;
    }

    line.spans.push(HighlightedSpan {
        color,
        bg: None,
        italic,
        bold,
        underline,
        strikethrough,
        text: text.to_string(),
    });
}

fn push_text(lines: &mut Vec<HighlightedLine>, mut text: &str, style: theme::SyntaxRenderStyle) {
    while !text.is_empty() {
        let newline = text.find('\n');
        match newline {
            Some(idx) => {
                let (head, tail) = text.split_at(idx);
                let head = head.strip_suffix('\r').unwrap_or(head);
                append_span(
                    lines.last_mut().expect("at least one line exists"),
                    style.color,
                    style.italic,
                    style.bold,
                    style.underline,
                    style.strikethrough,
                    head,
                );
                lines.push(HighlightedLine { spans: Vec::new() });
                text = &tail[1..];
            }
            None => {
                let text = text.strip_suffix('\r').unwrap_or(text);
                append_span(
                    lines.last_mut().expect("at least one line exists"),
                    style.color,
                    style.italic,
                    style.bold,
                    style.underline,
                    style.strikethrough,
                    text,
                );
                return;
            }
        }
    }
}

fn highlighted_lines(content: &str, spec: &TreeSitterLanguageSpec) -> Option<Vec<HighlightedLine>> {
    if content.is_empty() {
        return Some(Vec::new());
    }

    let mut highlighter = Highlighter::new();
    let config = build_highlight_config(spec)?;
    let capture_names = config.query.capture_names();
    let mut lines = vec![HighlightedLine { spans: Vec::new() }];
    let default_style = theme::syntax_render_style("text");
    let mut style_stack = vec![default_style];

    let events = highlighter
        .highlight(&config, content.as_bytes(), None, |_| None)
        .ok()?;

    for event in events {
        match event.ok()? {
            HighlightEvent::Source { start, end } => {
                let style = style_stack.last().copied().unwrap_or(default_style);
                push_text(&mut lines, &content[start..end], style);
            }
            HighlightEvent::HighlightStart(highlight) => {
                let capture_name = capture_names.get(highlight.0).copied().unwrap_or("text");
                style_stack.push(theme::syntax_render_style(capture_name));
            }
            HighlightEvent::HighlightEnd => {
                if style_stack.len() > 1 {
                    style_stack.pop();
                }
            }
        }
    }

    Some(lines)
}

fn plain_text_lines(content: &str) -> Vec<HighlightedLine> {
    let mut lines = Vec::new();
    let mut current = HighlightedLine { spans: Vec::new() };

    for chunk in content.split_inclusive('\n') {
        let chunk = chunk.strip_suffix('\n').unwrap_or(chunk);
        let chunk = chunk.strip_suffix('\r').unwrap_or(chunk);
        if !chunk.is_empty() {
            let style = theme::syntax_render_style("text");
            current.spans.push(HighlightedSpan {
                color: style.color,
                bg: None,
                italic: style.italic,
                bold: style.bold,
                underline: style.underline,
                strikethrough: style.strikethrough,
                text: chunk.to_string(),
            });
        }
        lines.push(current);
        current = HighlightedLine { spans: Vec::new() };
    }

    if lines.is_empty() && !content.is_empty() {
        lines.push(current);
    }

    lines
}

fn slice_preview_lines(
    lines: Vec<HighlightedLine>,
    start_line: usize,
) -> (usize, Vec<HighlightedLine>) {
    let start = start_line.saturating_sub(1);
    let preview = lines
        .into_iter()
        .skip(start)
        .take(MAX_PREVIEW_LINES)
        .collect();
    (start_line, preview)
}

fn syntax_lines_for_path(path: &Path, content: &str) -> Vec<HighlightedLine> {
    if content.is_empty() {
        return Vec::new();
    }

    let Some(spec) = syntax_set_for_path(path) else {
        return plain_text_lines(content);
    };

    highlighted_lines(content, &spec).unwrap_or_else(|| plain_text_lines(content))
}

// Warm the syntax and theme caches before the first preview render.
pub fn warm_highlighter() {
    let _ = theme::palette();
    let _ = theme::syntax_color("keyword");
}

// Overlay grep match ranges onto already-highlighted spans.
pub fn overlay_match_ranges(
    spans: &[HighlightedSpan],
    byte_ranges: &[(u32, u32)],
    _match_color: u32,
    match_bg: Option<u32>,
) -> Vec<HighlightedSpan> {
    if byte_ranges.is_empty() {
        return spans.to_vec();
    }

    let mut sorted_ranges = byte_ranges.to_vec();
    sorted_ranges.sort_unstable_by_key(|&(s, _)| s);

    let mut result = Vec::new();
    let mut byte_pos: u32 = 0;

    for span in spans {
        let span_start = byte_pos;
        let span_end = span_start + span.text.len() as u32;
        let mut chunk_start = span_start;

        for &(range_start, range_end) in &sorted_ranges {
            let overlap_start = range_start.max(chunk_start);
            let overlap_end = range_end.min(span_end);

            if overlap_start >= overlap_end {
                continue;
            }

            let pre_s = (chunk_start - span_start) as usize;
            let pre_e = (overlap_start - span_start) as usize;
            if let Some((pre_s, pre_e)) = clamp_range_to_char_boundaries(&span.text, pre_s, pre_e) {
                result.push(HighlightedSpan {
                    color: span.color,
                    bg: span.bg,
                    italic: span.italic,
                    bold: span.bold,
                    underline: span.underline,
                    strikethrough: span.strikethrough,
                    text: span.text[pre_s..pre_e].to_string(),
                });
            }

            let hi_s = (overlap_start - span_start) as usize;
            let hi_e = (overlap_end - span_start) as usize;
            if let Some((hi_s, hi_e)) = clamp_range_to_char_boundaries(&span.text, hi_s, hi_e) {
                result.push(HighlightedSpan {
                    color: span.color,
                    bg: match_bg.or(span.bg),
                    italic: span.italic,
                    bold: span.bold,
                    underline: span.underline,
                    strikethrough: span.strikethrough,
                    text: span.text[hi_s..hi_e].to_string(),
                });
            }

            chunk_start = overlap_end;
        }

        let tail_s = (chunk_start - span_start) as usize;
        if let Some((tail_s, tail_e)) =
            clamp_range_to_char_boundaries(&span.text, tail_s, span.text.len())
        {
            result.push(HighlightedSpan {
                color: span.color,
                bg: span.bg,
                italic: span.italic,
                bold: span.bold,
                underline: span.underline,
                strikethrough: span.strikethrough,
                text: span.text[tail_s..tail_e].to_string(),
            });
        }

        byte_pos = span_end;
    }

    result
}

fn clamp_range_to_char_boundaries(text: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let mut start = start.min(text.len());
    let mut end = end.min(text.len());

    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }

    (start < end).then_some((start, end))
}

// Read a file and return a syntax-highlighted preview window.
pub fn highlight_file_window(
    path: &Path,
    center_line: Option<usize>,
) -> (usize, Vec<HighlightedLine>) {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return (1, vec![]),
    };

    let lines = syntax_lines_for_path(path, &content);
    let start_line = center_line
        .map(|line| line.saturating_sub(MATCH_CONTEXT_BEFORE).max(1))
        .unwrap_or(1);

    slice_preview_lines(lines, start_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_sitter_capture_name_aliases_resolve_to_zed_tokens() {
        assert_eq!(
            theme::syntax_color("comment.documentation"),
            theme::syntax_color("comment")
        );
    }

    #[test]
    fn tree_sitter_highlight_map_detects_typescript_files() {
        let spec =
            syntax_set_for_path(Path::new("component.ts")).expect("typescript should be supported");

        assert_eq!(spec.name, "typescript");
    }
}
