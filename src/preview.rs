use std::path::Path;
use std::process::Command;

use crate::theme;

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

fn ansi_color_16(index: u8) -> u32 {
    match index {
        0 => 0x000000,
        1 => 0xcd3131,
        2 => 0x0dbc79,
        3 => 0xe5e510,
        4 => 0x2472c8,
        5 => 0xbc3fbc,
        6 => 0x11a8cd,
        7 => 0xe5e5e5,
        8 => 0x666666,
        9 => 0xf14c4c,
        10 => 0x23d18b,
        11 => 0xf5f543,
        12 => 0x3b8eea,
        13 => 0xd670d6,
        14 => 0x29b8db,
        _ => 0xffffff,
    }
}

fn color_256(index: u8) -> u32 {
    if index < 16 {
        return ansi_color_16(index);
    }
    if index >= 232 {
        let level = 8 + (index - 232) as u32 * 10;
        return (level << 16) | (level << 8) | level;
    }
    let index = index - 16;
    let r = index / 36;
    let g = (index / 6) % 6;
    let b = index % 6;
    let to_level = |v: u8| if v == 0 { 0 } else { 55 + v as u32 * 40 };
    (to_level(r) << 16) | (to_level(g) << 8) | to_level(b)
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

fn apply_sgr(
    seq: &str,
    color: &mut u32,
    italic: &mut bool,
    bold: &mut bool,
    underline: &mut bool,
    strikethrough: &mut bool,
    default_color: u32,
    default_italic: bool,
    default_bold: bool,
    default_underline: bool,
    default_strikethrough: bool,
) {
    if seq.is_empty() {
        *color = default_color;
        *italic = default_italic;
        *bold = default_bold;
        *underline = default_underline;
        *strikethrough = default_strikethrough;
        return;
    }
    let params: Vec<&str> = seq.split(';').collect();
    let mut i = 0;
    while i < params.len() {
        let code: u8 = params[i].parse().unwrap_or(0);
        match code {
            0 => {
                *color = default_color;
                *italic = default_italic;
                *bold = default_bold;
                *underline = default_underline;
                *strikethrough = default_strikethrough;
            }
            1 => *bold = true,
            3 => *italic = true,
            4 => *underline = true,
            9 => *strikethrough = true,
            22 => *bold = false,
            23 => *italic = false,
            24 => *underline = false,
            29 => *strikethrough = false,
            30..=37 => *color = ansi_color_16(code - 30),
            38 => {
                if params.get(i + 1) == Some(&"2") && i + 4 < params.len() {
                    let r: u8 = params[i + 2].parse().unwrap_or(0);
                    let g: u8 = params[i + 3].parse().unwrap_or(0);
                    let b: u8 = params[i + 4].parse().unwrap_or(0);
                    *color = (r as u32) << 16 | (g as u32) << 8 | b as u32;
                    i += 4;
                } else if params.get(i + 1) == Some(&"5") && i + 2 < params.len() {
                    let idx: u8 = params[i + 2].parse().unwrap_or(0);
                    *color = color_256(idx);
                    i += 2;
                }
            }
            39 => *color = default_color,
            90..=97 => *color = ansi_color_16(code - 90 + 8),
            _ => {}
        }
        i += 1;
    }
}

fn ansi_highlighted_lines(input: &str) -> Vec<HighlightedLine> {
    if input.is_empty() {
        return Vec::new();
    }

    let default_style = theme::syntax_render_style("text");
    let mut color = default_style.color;
    let mut italic = default_style.italic;
    let mut bold = default_style.bold;
    let mut underline = default_style.underline;
    let mut strikethrough = default_style.strikethrough;

    let mut lines: Vec<HighlightedLine> = Vec::new();
    let mut current = HighlightedLine { spans: Vec::new() };
    let mut buf = String::new();

    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if !buf.is_empty() {
                append_span(
                    &mut current,
                    color,
                    italic,
                    bold,
                    underline,
                    strikethrough,
                    &buf,
                );
                buf.clear();
            }
            if chars.next() == Some('[') {
                let mut seq = String::new();
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                    if c.is_ascii_digit() || c == ';' {
                        seq.push(c);
                    } else {
                        // Unknown CSI; stop parsing.
                        break;
                    }
                }
                apply_sgr(
                    &seq,
                    &mut color,
                    &mut italic,
                    &mut bold,
                    &mut underline,
                    &mut strikethrough,
                    default_style.color,
                    default_style.italic,
                    default_style.bold,
                    default_style.underline,
                    default_style.strikethrough,
                );
            }
        } else if ch == '\n' {
            if !buf.is_empty() {
                append_span(
                    &mut current,
                    color,
                    italic,
                    bold,
                    underline,
                    strikethrough,
                    &buf,
                );
                buf.clear();
            }
            lines.push(current);
            current = HighlightedLine { spans: Vec::new() };
        } else if ch == '\r' {
            // Ignore carriage returns.
        } else {
            buf.push(ch);
        }
    }

    if !buf.is_empty() {
        append_span(
            &mut current,
            color,
            italic,
            bold,
            underline,
            strikethrough,
            &buf,
        );
        buf.clear();
    }
    if !current.spans.is_empty() || !lines.is_empty() {
        lines.push(current);
    }

    lines
}

fn run_bat(path: &Path) -> Option<String> {
    let output = Command::new("bat")
        .arg("--color=always")
        .arg("--plain")
        .arg("--paging=never")
        .arg("--wrap=never")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn syntax_lines_for_path(path: &Path, content: &str) -> Vec<HighlightedLine> {
    if content.is_empty() {
        return Vec::new();
    }

    match run_bat(path) {
        Some(output) => ansi_highlighted_lines(&output),
        None => plain_text_lines(content),
    }
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
    fn clamp_range_handles_unicode() {
        assert!(clamp_range_to_char_boundaries("é", 1, 2).is_some());
    }
}
