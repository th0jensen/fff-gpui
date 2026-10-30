use std::collections::BTreeSet;
use std::io::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use fff_query_parser::{FFFQuery, FileSearchConfig, FuzzyQuery, QueryParser};
use fff_search::{
    ContentCacheBudget, FFFMode, FilePickerOptions, FuzzySearchOptions, GrepMode,
    GrepSearchOptions, PaginationArgs, SharedFilePicker, SharedFrecency, SharedQueryTracker,
    file_picker::FilePicker, frecency::FrecencyTracker, git::format_git_status_opt,
    query_tracker::QueryTracker,
};
use gpui::prelude::*;
use gpui::*;
use tracing::{debug, error, info, trace, warn};

use crate::editor;
use crate::log;
use crate::path_shortening::PathShortenStrategy;
use crate::preview::{self, HighlightedLine};
use crate::service::{ClientStream, PickEntry, PickResponse};
use crate::text_field::TextField;
use crate::theme::{self, AppTheme, FileIconPath};

pub type ResponderArc = Arc<Mutex<Option<ClientStream>>>;

// Keep live grep snappy by returning partial results quickly; newer keystrokes
// will preempt any still-running search.
const GREP_TIME_BUDGET_MS: u64 = 150;

// Write a PickResponse to the client and shut the stream so the client unblocks.
// `responder` is consumed; passing None or a stream that's already been taken is a no-op.
#[cfg(unix)]
fn send_pick_response(responder: Option<ResponderArc>, entries: &[PickEntry]) {
    let Some(arc) = responder else { return };
    let Ok(mut guard) = arc.lock() else { return };
    let Some(mut stream) = guard.take() else {
        return;
    };
    let payload = match serde_json::to_string(&PickResponse {
        paths: entries.to_vec(),
    }) {
        Ok(s) => s,
        Err(err) => {
            warn!(error = %err, "failed to serialize pick response");
            return;
        }
    };
    if let Err(err) = writeln!(stream, "{payload}") {
        warn!(error = %err, "failed to write pick response to client");
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

#[cfg(not(unix))]
fn send_pick_response(_responder: Option<ResponderArc>, _entries: &[PickEntry]) {}

actions!(
    fff_picker,
    [
        Quit,
        OpenSelected,
        SelectNext,
        SelectPrev,
        ToggleSelected,
        ToggleSelectAll,
        ShiftTab,
        CycleGrepMode,
        CyclePreviousQuery,
        PreviewScrollUp,
        PreviewScrollDown,
        SwitchFiles,
        SwitchGrep,
    ]
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchView {
    Files,
    Grep,
}

#[derive(Clone, Debug, Default)]
pub struct PickerSharedState {
    pub shared_picker: SharedFilePicker,
    pub shared_frecency: SharedFrecency,
    pub shared_query_tracker: SharedQueryTracker,
}

// Return a sensible worker count for fff searches on the current machine.
fn search_threads() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
}

// A single grep-matched line within a file, with byte ranges for that line.
#[derive(Clone)]
pub struct GrepMatchLine {
    pub line_number: u64,
    pub line_content: String,
    pub byte_ranges: Vec<(u32, u32)>,
}

// A file path snapshot captured from a FileItem for render and preview work.
#[derive(Clone)]
pub struct FileItemSnapshot {
    pub file_name: String,
    pub dir: String,
    pub absolute_path: PathBuf,
    pub git_status: Option<String>,
    pub frecency_score: i16,
    pub match_ranges: Vec<Range<usize>>,
    pub grep_matches: Vec<GrepMatchLine>,
}

pub struct FffPicker {
    shared_picker: SharedFilePicker,
    shared_frecency: SharedFrecency,
    shared_query_tracker: SharedQueryTracker,
    base_path: PathBuf,
    view: SearchView,
    excluded_dirs: Vec<PathBuf>,
    print_stdout: bool,
    grep_mode: GrepMode,
    query: String,
    results: Arc<Vec<FileItemSnapshot>>,
    total_files: usize,
    total_matched: usize,
    indexed_count: usize,
    selected: usize,
    selected_paths: Arc<BTreeSet<PathBuf>>,
    scan_done: bool,
    search_epoch: u64,
    search_in_flight: bool,
    search_abort: Option<Arc<AtomicBool>>,
    preview_epoch: u64,
    preview_loading: bool,
    preview_loading_visible: bool,
    preview_scroll_row: usize,
    preview_start_line: usize,
    theme_version: u64,
    focus_handle: FocusHandle,
    list_scroll: UniformListScrollHandle,
    preview_scroll: UniformListScrollHandle,
    preview_lines: Arc<Vec<HighlightedLine>>,
    status_message: Option<String>,
    text_field: Entity<TextField>,
    editor: String,
    dismiss_on_blur: Option<Subscription>,
    dismiss_on_window_blur: Option<Subscription>,
    responder: Option<ResponderArc>,
}

// Find byte ranges where query characters appear in order.
fn find_match_ranges(query: &str, text: &str) -> Vec<Range<usize>> {
    let query = query.trim();
    if query.is_empty() {
        return vec![];
    }

    let fuzzy_chars: Vec<char> = query.to_lowercase().chars().collect();
    let mut qi = 0;
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut run_start: Option<usize> = None;
    let mut run_end: usize = 0;

    for (byte_idx, orig_ch) in text.char_indices() {
        if qi >= fuzzy_chars.len() {
            break;
        }
        let ch_lower = orig_ch.to_lowercase().next().unwrap_or(orig_ch);
        if ch_lower == fuzzy_chars[qi] {
            if run_start.is_none() {
                run_start = Some(byte_idx);
            }
            run_end = byte_idx + orig_ch.len_utf8();
            qi += 1;
        } else if let Some(start) = run_start.take() {
            ranges.push(start..run_end);
        }
    }
    if let Some(start) = run_start {
        ranges.push(start..run_end);
    }

    if qi >= fuzzy_chars.len() {
        ranges
    } else {
        vec![]
    }
}

// Render text with character ranges highlighted in the match color.
fn render_highlighted(text: &str, ranges: &[Range<usize>], theme: &AppTheme) -> Div {
    fn clamp_range_to_char_boundaries(
        text: &str,
        start: usize,
        end: usize,
    ) -> Option<Range<usize>> {
        let mut start = start.min(text.len());
        let mut end = end.min(text.len());

        while start > 0 && !text.is_char_boundary(start) {
            start -= 1;
        }
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }

        (start < end).then_some(start..end)
    }

    let mut ranges: Vec<Range<usize>> = ranges
        .iter()
        .filter_map(|range| clamp_range_to_char_boundaries(text, range.start, range.end))
        .collect();
    ranges.sort_by_key(|range| (range.start, range.end));

    if ranges.is_empty() {
        return div().flex().items_center().child(text.to_string());
    }

    let mut parts: Vec<Div> = Vec::new();
    let mut last = 0;

    for range in ranges {
        if range.start < last {
            continue;
        }
        if range.start > last {
            parts.push(
                div()
                    .text_color(rgb(theme.text_primary))
                    .child(text[last..range.start].to_string()),
            );
        }
        parts.push(
            div()
                .text_color(rgb(theme.match_highlight))
                .child(text[range.clone()].to_string()),
        );
        last = range.end;
    }
    if last < text.len() {
        parts.push(
            div()
                .text_color(rgb(theme.text_primary))
                .child(text[last..].to_string()),
        );
    }

    div().flex().items_center().children(parts)
}

// Shorten the directory segment shown in each result row.
fn shorten_dir_for_row(dir: &str, max_chars: usize) -> String {
    let trimmed = dir.trim_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }

    PathShortenStrategy::MiddleNumber.shorten_path(std::path::Path::new(trimmed), max_chars)
}

// Keep file search on the fast fuzzy path unless the query looks like a
// filename or path filter.
//
// This avoids parsing code-shaped queries like `struct Data {` as glob-like
// constraints, while preserving filename/path searches such as `main.rs`,
// `src/foo`, `*.toml`, and `type:rust`.
fn should_parse_file_constraints(query: &str) -> bool {
    query.split_whitespace().any(|token| {
        token.contains('/') || token.contains(':') || token.starts_with('.') || token.contains('.')
    })
}

// Keep only tokens that can actually help fuzzy file-name matching.
//
// Punctuation-only crumbs like `{`, `}`, `(`, `)` are common in code-shaped
// queries but are useless for file-name fuzziness and can make the search
// require impossible extra matches.
fn is_useful_fuzzy_token(token: &str) -> bool {
    token.chars().any(|c| c.is_ascii_alphanumeric())
}

fn build_file_query<'a>(query: &'a str) -> FFFQuery<'a> {
    let query = query.trim();
    if query.is_empty() {
        return FFFQuery {
            raw_query: query,
            constraints: Vec::new(),
            fuzzy_query: FuzzyQuery::Empty,
            location: None,
        };
    }

    if should_parse_file_constraints(query) {
        let parser = QueryParser::new(FileSearchConfig);
        return parser.parse(query);
    }

    let fuzzy_parts: Vec<&str> = query
        .split_whitespace()
        .filter(|token| is_useful_fuzzy_token(token))
        .collect();

    let fuzzy_query = match fuzzy_parts.as_slice() {
        [] => FuzzyQuery::Empty,
        [single] => FuzzyQuery::Text(single),
        parts => FuzzyQuery::Parts(parts.to_vec()),
    };

    FFFQuery {
        raw_query: query,
        constraints: Vec::new(),
        fuzzy_query,
        location: None,
    }
}

// Resolve the git-status colour used for the row's left-edge bar.
fn git_status_bar_color(status: Option<&str>) -> Option<u32> {
    match status {
        Some("modified") => Some(0xF5A524),
        Some("staged_new") | Some("staged_modified") => Some(0x32D583),
        Some("staged_deleted") | Some("deleted") => Some(0xF97066),
        Some("renamed") => Some(0x8E8E93),
        Some("untracked") => Some(0xA48EFF),
        Some("ignored") => Some(0x6C6C70),
        Some("clean") | None => None,
        Some(_) => Some(0x6C6C70),
    }
}

fn render_file_icon(icon: Option<FileIconPath>, muted: u32) -> AnyElement {
    match icon {
        Some(FileIconPath::Embedded(path)) => svg()
            .path(path)
            .size(px(16.0))
            .flex_shrink_0()
            .text_color(rgb(muted))
            .into_any_element(),
        Some(FileIconPath::External(path)) => {
            img(path).size(px(16.0)).flex_shrink_0().into_any_element()
        }
        None => div()
            .w(px(16.0))
            .h(px(16.0))
            .flex_shrink_0()
            .into_any_element(),
    }
}

// Run a live grep query using the upstream parser and grep engine.
fn execute_grep_search(
    picker: &FilePicker,
    query: &str,
    base: &Path,
    excluded_dirs: &[PathBuf],
    abort_signal: Arc<AtomicBool>,
    grep_mode: GrepMode,
) -> (Vec<FileItemSnapshot>, usize, usize) {
    let query = query.trim();
    let fuzzy_query_text: String;
    let parsed = match grep_mode {
        GrepMode::Fuzzy => {
            fuzzy_query_text = query
                .split_whitespace()
                .filter(|token| is_useful_fuzzy_token(token))
                .collect::<Vec<_>>()
                .join(" ");
            let fuzzy_query = if fuzzy_query_text.is_empty() {
                FuzzyQuery::Empty
            } else {
                FuzzyQuery::Text(fuzzy_query_text.as_str())
            };
            FFFQuery {
                raw_query: query,
                constraints: Vec::new(),
                fuzzy_query,
                location: None,
            }
        }
        _ => {
            fuzzy_query_text = String::new();
            fff_search::grep::parse_grep_query(query)
        }
    };
    let primary_mode = match grep_mode {
        GrepMode::PlainText => GrepMode::PlainText,
        GrepMode::Regex => GrepMode::Regex,
        GrepMode::Fuzzy => GrepMode::Fuzzy,
    };

    let grep_started = Instant::now();
    let run = |mode| {
        picker.grep(
            &parsed,
            &GrepSearchOptions {
                mode,
                page_limit: 200,
                max_matches_per_file: 5,
                smart_case: true,
                time_budget_ms: GREP_TIME_BUDGET_MS,
                abort_signal: Some(abort_signal.clone()),
                ..Default::default()
            },
        )
    };

    let grep_result = run(primary_mode);
    let mut items: Vec<FileItemSnapshot> = Vec::new();
    let mut item_by_path = std::collections::HashMap::<PathBuf, usize>::new();
    for gm in &grep_result.matches {
        let Some(fi) = grep_result.files.get(gm.file_index) else {
            continue;
        };
        if fi.is_binary() {
            continue;
        }
        let absolute_path = fi.absolute_path(picker, base);
        if path_is_excluded(&absolute_path, excluded_dirs) {
            continue;
        }
        let file_name = fi.file_name(picker);
        let dir = fi.dir_str(picker);
        let grep_match = GrepMatchLine {
            line_number: gm.line_number,
            line_content: gm.line_content.clone(),
            byte_ranges: gm.match_byte_offsets.iter().copied().collect(),
        };
        if let Some(&idx) = item_by_path.get(&absolute_path) {
            items[idx].grep_matches.push(grep_match);
        } else {
            item_by_path.insert(absolute_path.clone(), items.len());
            items.push(FileItemSnapshot {
                git_status: format_git_status_opt(fi.git_status).map(str::to_string),
                frecency_score: fi.access_frecency_score,
                match_ranges: find_match_ranges(query, &file_name),
                file_name,
                dir,
                absolute_path,
                grep_matches: vec![grep_match],
            });
        }
    }

    let total_files_seen = grep_result.total_files.max(grep_result.filtered_file_count);
    let total_matched = items.len();
    info!(
        query = %query,
        grep_mode = ?grep_mode,
        primary_mode = ?primary_mode,
        fuzzy_query = %fuzzy_query_text,
        total_files = total_files_seen,
        total_matched,
        returned = items.len(),
        elapsed = ?grep_started.elapsed(),
        "grep search completed"
    );
    (items, total_files_seen, total_matched)
}

fn path_is_excluded(path: &Path, excluded_dirs: &[PathBuf]) -> bool {
    excluded_dirs
        .iter()
        .any(|excluded| path.starts_with(excluded))
}

impl FffPicker {
    // Create a new picker rooted at `base_path` and start the background file scan.
    pub fn new(
        base_path: PathBuf,
        shared: PickerSharedState,
        enable_content_indexing: bool,
        start_in_grep: bool,
        initial_query: String,
        excluded_dirs: Vec<PathBuf>,
        print_stdout: bool,
        editor: String,
        responder: Option<ResponderArc>,
        cx: &mut Context<Self>,
    ) -> Self {
        let text_field = cx.new(|cx| {
            let mut field = TextField::new("Search files...", cx);
            field.set_text(initial_query.clone(), cx);
            field
        });

        cx.observe(&text_field, |this, _entity, cx| {
            let new_query = this.text_field.read(cx).text();
            if new_query != this.query {
                this.query = new_query;
                this.status_message = None;
                Arc::make_mut(&mut this.selected_paths).clear();
                this.preview_scroll_row = 0;
                this.run_search(cx);
            }
        })
        .detach();

        let mut instance = Self {
            shared_picker: shared.shared_picker,
            shared_frecency: shared.shared_frecency,
            shared_query_tracker: shared.shared_query_tracker,
            base_path: base_path.clone(),
            view: if start_in_grep {
                SearchView::Grep
            } else {
                SearchView::Files
            },
            excluded_dirs,
            print_stdout,
            grep_mode: GrepMode::PlainText,
            query: initial_query,
            results: Arc::new(Vec::new()),
            total_files: 0,
            total_matched: 0,
            indexed_count: 0,
            selected: 0,
            selected_paths: Arc::new(BTreeSet::new()),
            scan_done: false,
            search_epoch: 0,
            search_in_flight: false,
            search_abort: None,
            preview_epoch: 0,
            preview_loading: false,
            preview_loading_visible: false,
            preview_scroll_row: 0,
            preview_start_line: 1,
            theme_version: theme::version(),
            focus_handle: cx.focus_handle(),
            list_scroll: UniformListScrollHandle::new(),
            preview_scroll: UniformListScrollHandle::new(),
            preview_lines: Arc::new(Vec::new()),
            status_message: None,
            text_field,
            editor,
            dismiss_on_blur: None,
            dismiss_on_window_blur: None,
            responder,
        };

        instance.start_scan(base_path, enable_content_indexing, cx);
        instance
    }

    // Close the popup when the window loses focus, matching Raycast-style dismissal.
    pub fn install_focus_lost_dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_on_window_blur = Some(cx.observe_window_activation(window, |_this, window, _cx| {
            if !window.is_window_active() {
                window.remove_window();
            }
        }));
        self.dismiss_on_blur = Some(cx.on_blur(&self.focus_handle, window, |_this, window, _cx| {
            window.remove_window();
        }));
    }

    // Start the file indexer and trigger the initial search when indexing is ready.
    #[tracing::instrument(skip(self, cx, base_path), fields(base_path = %base_path.display(), enable_content_indexing))]
    fn start_scan(
        &mut self,
        base_path: PathBuf,
        enable_content_indexing: bool,
        cx: &mut Context<Self>,
    ) {
        let sp = self.shared_picker.clone();
        let sf = self.shared_frecency.clone();
        let sq = self.shared_query_tracker.clone();

        self.spawn_index_progress_poll(cx);

        let existing_picker = self.shared_picker.read().ok().and_then(|guard| {
            guard.as_ref().map(|picker| {
                (
                    picker.base_path().to_path_buf(),
                    picker.get_scan_progress().is_scanning,
                )
            })
        });

        if let Some((existing_base_path, is_scanning)) = existing_picker
            && existing_base_path == base_path
        {
            info!(
                base_path = %base_path.display(),
                is_scanning,
                "reusing resident file index"
            );
            if is_scanning {
                cx.spawn(
                    async move |this: WeakEntity<FffPicker>, cx: &mut AsyncApp| {
                        let scan_done =
                            smol::unblock(move || sp.wait_for_scan(Duration::from_secs(60))).await;
                        if !scan_done {
                            warn!(base_path = %base_path.display(), "resident file scan timed out");
                        }

                        let update_result = this.update(cx, |this, cx| {
                            this.scan_done = true;
                            cx.notify();
                            this.run_search(cx);
                            info!(
                                scan_done = this.scan_done,
                                results = this.results.len(),
                                "resident scan state applied to picker"
                            );
                        });

                        if let Err(err) = update_result {
                            warn!(
                                error = %err,
                                "failed to apply resident scan state to picker"
                            );
                        }
                    },
                )
                .detach();
            } else {
                self.scan_done = true;
                self.run_search(cx);
                info!(
                    scan_done = self.scan_done,
                    results = self.results.len(),
                    "resident scan state applied to picker"
                );
            }
            return;
        }

        info!("starting file index");

        cx.spawn(
            async move |this: WeakEntity<FffPicker>, cx: &mut AsyncApp| {
                smol::unblock(move || {
                    preview::warm_highlighter();

                    trace!(home = ?std::env::var("HOME").ok(), "initializing shared trackers");
                    if let Ok(home) = std::env::var("HOME") {
                        let data_dir = PathBuf::from(home).join(".local/share/fff");
                        let _ = std::fs::create_dir_all(&data_dir);
                        if let Ok(tracker) =
                            FrecencyTracker::open(data_dir.join("frecency.lmdb"))
                        {
                            let _ = sf.init(tracker);
                        }
                        if let Ok(tracker) = QueryTracker::open(
                            data_dir.join("queries.lmdb").to_string_lossy().as_ref(),
                        ) {
                            let _ = sq.init(tracker);
                        }
                    }
                    if let Err(err) = FilePicker::new_with_shared_state(
                        sp.clone(),
                        sf,
                        FilePickerOptions {
                            base_path: base_path.to_string_lossy().to_string(),
                            enable_mmap_cache: false,
                            // Disable the persistent grep content cache so
                            // grep falls back to a per-search reusable buffer
                            // instead of mmap-pinning every searched file
                            // (default allows ~512 MB and never frees in a
                            // daemon-resident picker). Keep max_file_size at
                            // its default (10 MB) — zeroing it would make
                            // get_content_for_search reject every file and
                            // grep would return no matches.
                            cache_budget: Some(ContentCacheBudget {
                                max_files: 0,
                                max_bytes: 0,
                                ..ContentCacheBudget::default()
                            }),
                            enable_content_indexing,
                            mode: FFFMode::Neovim,
                            watch: false,
                            ..Default::default()
                        },
                    ) {
                        error!(error = %err, base_path = %base_path.display(), "failed to initialize file picker");
                    }

                    let scan_completed = sp.wait_for_scan(Duration::from_secs(60));
                    if scan_completed {
                        info!(base_path = %base_path.display(), "initial file scan completed");
                    } else {
                        warn!(base_path = %base_path.display(), "initial file scan timed out");
                    }
                })
                .await;

                let update_result = this.update(cx, |this, cx| {
                    this.scan_done = true;
                    cx.notify();
                    this.run_search(cx);
                    info!(
                        scan_done = this.scan_done,
                        results = this.results.len(),
                        "scan completion applied to picker state"
                    );
                });

                if let Err(err) = update_result {
                    warn!(error = %err, "failed to apply scan completion to picker state");
                }
            },
        )
        .detach();
    }

    // Poll the shared picker's scan progress at ~150 ms while scanning is
    // active and publish `scanned_files_count` into `indexed_count` so the
    // UI can render a live counter. The loop exits as soon as the scan
    // reports `is_scanning == false`, or when the entity is dropped.
    fn spawn_index_progress_poll(&self, cx: &mut Context<Self>) {
        let shared_picker = self.shared_picker.clone();
        cx.spawn(
            async move |this: WeakEntity<FffPicker>, cx: &mut AsyncApp| {
                loop {
                    let Some(progress) = shared_picker
                        .read()
                        .ok()
                        .and_then(|guard| guard.as_ref().map(|p| p.get_scan_progress()))
                    else {
                        smol::Timer::after(Duration::from_millis(150)).await;
                        if this.upgrade().is_none() {
                            return;
                        }
                        continue;
                    };

                    let count = progress.scanned_files_count;
                    let scanning = progress.is_scanning;
                    let update = this.update(cx, |this, cx| {
                        if this.indexed_count != count {
                            this.indexed_count = count;
                            cx.notify();
                        }
                    });

                    if update.is_err() {
                        return;
                    }

                    if !scanning {
                        return;
                    }

                    smol::Timer::after(Duration::from_millis(150)).await;
                }
            },
        )
        .detach();
    }

    // Run the active search view and render the corresponding result set.
    fn run_search(&mut self, cx: &mut Context<Self>) {
        if !self.scan_done {
            return;
        }

        if let Some(abort) = &self.search_abort {
            abort.store(true, Ordering::Release);
        }
        self.search_epoch = self.search_epoch.wrapping_add(1);
        self.search_in_flight = true;
        let abort_signal = Arc::new(AtomicBool::new(false));
        self.search_abort = Some(abort_signal.clone());
        let epoch = self.search_epoch;
        let shared_picker = self.shared_picker.clone();
        let shared_query_tracker = self.shared_query_tracker.clone();
        let query_str = self.query.clone();
        let view = self.view;
        let grep_mode = self.grep_mode;
        let excluded_dirs = self.excluded_dirs.clone();
        info!(
            epoch,
            query = %query_str.trim(),
            view = ?view,
            grep_mode = ?grep_mode,
            "starting search"
        );

        cx.spawn(
            async move |this: WeakEntity<FffPicker>, cx: &mut AsyncApp| {
                let (items, total_files, total_matched) = smol::unblock(move || {
                    let Ok(guard) = shared_picker.read() else {
                        return (Vec::new(), 0, 0);
                    };
                    let Some(picker) = guard.as_ref() else {
                        return (Vec::new(), 0, 0);
                    };

                    let base = picker.base_path().to_path_buf();
                    let query = query_str.trim().to_string();

                    match view {
                        SearchView::Files => {
                            let file_search_started = Instant::now();
                            let parse_started = Instant::now();
                            let parsed = build_file_query(&query);
                            let parse_elapsed = parse_started.elapsed();
                            let query_tracker = shared_query_tracker.read().ok();
                            let search = picker.fuzzy_search(
                                &parsed,
                                query_tracker
                                    .as_deref()
                                    .and_then(|tracker| tracker.as_ref()),
                                FuzzySearchOptions {
                                    max_threads: search_threads(),
                                    project_path: Some(picker.base_path()),
                                    combo_boost_score_multiplier: 100,
                                    min_combo_count: 3,
                                    pagination: PaginationArgs {
                                        offset: 0,
                                        limit: 200,
                                    },
                                    ..Default::default()
                                },
                            );
                            let fuzzy_items = search
                                .items
                                .iter()
                                .filter_map(|fi| {
                                    if fi.is_binary() {
                                        return None;
                                    }
                                    let absolute_path = fi.absolute_path(picker, &base);
                                    if path_is_excluded(&absolute_path, &excluded_dirs) {
                                        return None;
                                    }
                                    let file_name = fi.file_name(picker);
                                    let dir = fi.dir_str(picker);
                                    Some(FileItemSnapshot {
                                        git_status: format_git_status_opt(fi.git_status)
                                            .map(str::to_string),
                                        frecency_score: fi.access_frecency_score,
                                        match_ranges: find_match_ranges(&query, &file_name),
                                        file_name,
                                        dir,
                                        absolute_path,
                                        grep_matches: vec![],
                                    })
                                })
                                .collect::<Vec<_>>();
                            let visible_results = fuzzy_items.len();
                            info!(
                                epoch,
                                query = %query,
                                query_mode = if parsed.constraints.is_empty() { "plain_fuzzy" } else { "file_filter" },
                                constraints = parsed.constraints.len(),
                                parse_elapsed = ?parse_elapsed,
                                search_elapsed = ?file_search_started.elapsed(),
                                total_files = search.total_files,
                                total_matched = search.total_matched,
                                visible_results,
                                "file search completed"
                            );

                            (fuzzy_items, search.total_files, visible_results)
                        }
                        SearchView::Grep => {
                            if query.is_empty() {
                                return (Vec::new(), 0, 0);
                            }

                            execute_grep_search(
                                picker,
                                &query_str,
                                &base,
                                &excluded_dirs,
                                abort_signal,
                                grep_mode,
                            )
                        }
                    }
                })
                .await;

                let update_result = this.update(cx, |this, cx| {
                    if this.search_epoch != epoch {
                        trace!(epoch, "discarding stale search result");
                        this.finish_search(epoch, cx);
                        return;
                    }
                    debug!(
                        epoch,
                        results = items.len(),
                        total_files,
                        total_matched,
                        "applying search result"
                    );
                    this.results = Arc::new(items);
                    this.total_files = total_files;
                    this.total_matched = total_matched;
                    this.selected = 0;
                    this.preview_scroll_row = 0;
                    Arc::make_mut(&mut this.selected_paths)
                        .retain(|path| this.results.iter().any(|item| &item.absolute_path == path));
                    if !this.results.is_empty() {
                        this.list_scroll.scroll_to_item(
                            this.visual_index(this.selected),
                            ScrollStrategy::Bottom,
                        );
                    }
                    this.load_preview(cx);
                    cx.notify();
                    info!(
                        epoch,
                        view = ?this.view,
                        query = %this.query,
                        visible_results = this.results.len(),
                        selected = this.selected,
                        scan_done = this.scan_done,
                        "search results applied"
                    );
                    this.finish_search(epoch, cx);
                    trace!(
                        epoch,
                        scan_done = this.scan_done,
                        results = this.results.len(),
                        "search result applied to picker state"
                    );
                });

                if let Err(err) = update_result {
                    warn!(error = %err, epoch, "failed to apply search result to picker state");
                }
            },
        )
        .detach();
    }

    // Finish the active search and schedule any query that arrived while it was running.
    fn finish_search(&mut self, epoch: u64, _cx: &mut Context<Self>) {
        if self.search_epoch != epoch {
            return;
        }
        self.search_in_flight = false;
        self.search_abort = None;
    }

    // Clear results and repaint immediately, then kick off a fresh search on
    // the next frame. This lets GPUI flush the mode-change render before the
    // search work starts, avoiding a visible hang on the stale result list.
    fn switch_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(abort) = &self.search_abort {
            abort.store(true, Ordering::Release);
        }
        Arc::make_mut(&mut self.results).clear();
        self.total_files = 0;
        self.total_matched = 0;
        self.selected = 0;
        Arc::make_mut(&mut self.selected_paths).clear();
        Arc::make_mut(&mut self.preview_lines).clear();
        self.preview_loading = false;
        self.preview_loading_visible = false;
        self.status_message = None;
        cx.notify();
        cx.defer_in(window, |this, _window, cx| {
            this.run_search(cx);
        });
    }

    // Load and syntax-highlight the selected file preview in the background.
    fn load_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_epoch = self.preview_epoch.wrapping_add(1);
        let preview_epoch = self.preview_epoch;
        let (path, grep_matches) = match self.results.get(self.selected) {
            Some(r) => (r.absolute_path.clone(), r.grep_matches.clone()),
            None => {
                self.preview_lines = Arc::new(vec![]);
                self.preview_loading = false;
                self.preview_loading_visible = false;
                self.preview_scroll_row = 0;
                self.preview_start_line = 1;
                return;
            }
        };

        self.preview_loading = true;
        self.preview_loading_visible = false;
        trace!(
            preview_epoch,
            path = %path.display(),
            grep_matches = grep_matches.len(),
            "loading preview"
        );
        let first_match_line = grep_matches.iter().map(|m| m.line_number as usize).min();
        let theme = cx.global::<AppTheme>();
        let match_highlight = theme.match_highlight;
        let match_bg = theme.match_highlight_bg;

        cx.spawn(
            async move |this: WeakEntity<FffPicker>, cx: &mut AsyncApp| {
                smol::Timer::after(Duration::from_millis(120)).await;
                this.update(cx, |this, cx| {
                    if this.preview_epoch == preview_epoch
                        && this.preview_loading
                        && this.preview_lines.is_empty()
                    {
                        this.preview_loading_visible = true;
                        cx.notify();
                    }
                })
                .ok();
            },
        )
        .detach();

        cx.spawn(
            async move |this: WeakEntity<FffPicker>, cx: &mut AsyncApp| {
                let (start_line, lines) = smol::unblock(move || {
                    let (start_line, mut lines) =
                        preview::highlight_file_window(&path, first_match_line);
                    for gm in &grep_matches {
                        let idx = (gm.line_number as usize).saturating_sub(start_line);
                        if let Some(line) = lines.get_mut(idx) {
                            line.spans = preview::overlay_match_ranges(
                                &line.spans,
                                &gm.byte_ranges,
                                match_highlight,
                                Some(match_bg),
                            );
                        }
                    }
                    (start_line, lines)
                })
                .await;

                this.update(cx, |this, cx| {
                    if this.preview_epoch != preview_epoch {
                        trace!(preview_epoch, "discarding stale preview result");
                        return;
                    }
                    this.preview_lines = Arc::new(lines);
                    this.preview_loading = false;
                    this.preview_loading_visible = false;
                    this.preview_start_line = start_line;
                    this.preview_scroll_row = first_match_line
                        .map(|line| line.saturating_sub(start_line))
                        .unwrap_or(0);
                    let scroll_to = this.preview_scroll_row;
                    this.preview_scroll.scroll_to_item(
                        scroll_to.saturating_sub(preview::MATCH_CONTEXT_BEFORE),
                        ScrollStrategy::Top,
                    );
                    cx.notify();
                })
                .ok();
            },
        )
        .detach();
    }

    // Compute the per-path goto (line, column) from the matching grep entry, if any.
    fn goto_for_path(&self, path: &Path) -> Option<(usize, usize)> {
        self.results
            .iter()
            .find(|item| item.absolute_path == path)
            .and_then(|item| item.grep_matches.first())
            .map(|m| {
                let line = m.line_number as usize;
                let column = m
                    .byte_ranges
                    .first()
                    .and_then(|(start, _)| {
                        let start = *start as usize;
                        m.line_content
                            .get(..start)
                            .map(|prefix| prefix.chars().count() + 1)
                    })
                    .unwrap_or(1);
                (line, column)
            })
    }

    // Update frecency / query trackers for the selected paths.
    fn track_open(&self, paths: &[PathBuf]) {
        if self.view == SearchView::Grep
            && let Ok(guard) = self.shared_picker.read()
            && let Some(picker) = guard.as_ref()
            && let Ok(mut tracker_guard) = self.shared_query_tracker.write()
            && let Some(tracker) = tracker_guard.as_mut()
        {
            let _ = tracker.track_grep_query(&self.query, picker.base_path());
        }

        for path in paths {
            if self.view == SearchView::Files
                && let Ok(guard) = self.shared_picker.read()
                && let Some(picker) = guard.as_ref()
                && let Ok(mut tracker_guard) = self.shared_query_tracker.write()
                && let Some(tracker) = tracker_guard.as_mut()
            {
                let _ = tracker.track_query_completion(&self.query, picker.base_path(), path);
            }

            if let Ok(guard) = self.shared_frecency.read()
                && let Some(tracker) = guard.as_ref()
            {
                let _ = tracker.track_access(path);
            }
        }
    }

    // Open the selected file(s). For client-forwarded sessions writes paths back over the IPC
    // socket (the client process spawns the editor). For daemon-side sessions (menubar /
    // hotkey / daemon-startup --open) spawns the editor inline.
    fn on_open_selected(&mut self, _: &OpenSelected, window: &mut Window, cx: &mut Context<Self>) {
        let paths_to_open: Vec<PathBuf> = if !self.selected_paths.is_empty() {
            self.selected_paths.iter().cloned().collect()
        } else if let Some(item) = self.results.get(self.selected) {
            vec![item.absolute_path.clone()]
        } else {
            return;
        };

        self.track_open(&paths_to_open);

        let entries: Vec<PickEntry> = paths_to_open
            .iter()
            .map(|p| {
                let goto = self.goto_for_path(p);
                PickEntry {
                    path: p.clone(),
                    line: goto.map(|g| g.0),
                    column: goto.map(|g| g.1),
                }
            })
            .collect();

        if self.responder.is_some() {
            send_pick_response(self.responder.take(), &entries);
            window.remove_window();
            return;
        }

        let mut opened = 0usize;
        let mut last_error: Option<String> = None;
        for entry in &entries {
            let goto = entry.line.zip(entry.column);
            if self.print_stdout {
                println!("{}", entry.path.display());
                opened += 1;
                continue;
            }
            match editor::open_in_editor(
                &entry.path,
                goto,
                &self.editor,
                editor::EditorLaunchMode::Detached,
            ) {
                Ok(child) => {
                    info!(pid = child.id(), path = %entry.path.display(), "spawned editor");
                    opened += 1;
                }
                Err(err) => {
                    error!(error = %err, path = %entry.path.display(), "open failed");
                    last_error = Some(err.to_string());
                }
            }
        }

        if opened > 0 {
            self.status_message = Some(if entries.len() == 1 {
                format!("Opened {}", entries[0].path.display())
            } else {
                format!("Opened {opened} files")
            });
            cx.notify();
            window.remove_window();
        } else if let Some(err) = last_error {
            self.status_message = Some(format!(
                "Open failed: {err}  (log: {})",
                log::path_for_display()
            ));
            cx.notify();
        }
    }

    // Close the current picker window without terminating the resident service.
    fn on_quit(&mut self, _: &Quit, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }
}

// Send an empty PickResponse to the client when the picker is dismissed without a selection
// (Esc, focus-loss, window-close, session replacement). For non-client sessions
// (responder=None) this is a no-op. on_open_selected calls take() first so the inner stream is
// already gone by the time Drop runs after a successful pick.
impl Drop for FffPicker {
    fn drop(&mut self) {
        if self.responder.is_some() {
            send_pick_response(self.responder.take(), &[]);
        }
    }
}

impl FffPicker {
    // Move selection visually down toward the input — i.e. toward a better-ranked result.
    fn on_select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.results.is_empty() && self.selected > 0 {
            self.selected -= 1;
            self.list_scroll
                .scroll_to_item(self.visual_index(self.selected), ScrollStrategy::Center);
            self.load_preview(cx);
            cx.notify();
        }
    }

    // Move selection visually up — toward a worse-ranked result.
    fn on_select_prev(&mut self, _: &SelectPrev, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.results.is_empty() && self.selected + 1 < self.results.len() {
            self.selected += 1;
            self.list_scroll
                .scroll_to_item(self.visual_index(self.selected), ScrollStrategy::Center);
            self.load_preview(cx);
            cx.notify();
        }
    }

    // Translate a result index to a visual list index. The list renders bottom-up,
    // so rank 0 (best match) sits at the bottom, just above the input.
    fn visual_index(&self, data_index: usize) -> usize {
        self.results
            .len()
            .saturating_sub(1)
            .saturating_sub(data_index)
    }

    // Select the clicked row and refresh the preview.
    fn on_select_row(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.results.len() {
            return;
        }

        if self.selected == index {
            self.on_open_selected(&OpenSelected, window, cx);
            return;
        }

        self.selected = index;
        self.list_scroll
            .scroll_to_item(self.visual_index(self.selected), ScrollStrategy::Center);
        self.load_preview(cx);
        window.focus(&self.text_field_focus_handle(cx));
        cx.notify();
    }

    // Toggle the selected state for the current row.
    fn on_toggle_selected(
        &mut self,
        _: &ToggleSelected,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.results.get(self.selected) else {
            return;
        };
        let paths = Arc::make_mut(&mut self.selected_paths);
        if !paths.insert(item.absolute_path.clone()) {
            paths.remove(&item.absolute_path);
        }
        cx.notify();
    }

    // Toggle selection across all visible results: clear if anything is marked,
    // otherwise mark every result.
    fn on_toggle_select_all(
        &mut self,
        _: &ToggleSelectAll,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = Arc::make_mut(&mut self.selected_paths);
        if paths.is_empty() {
            for item in &*self.results {
                paths.insert(item.absolute_path.clone());
            }
        } else {
            paths.clear();
        }
        cx.notify();
    }

    // Shift-tab: cycle grep mode in grep view, otherwise move selection up.
    fn on_shift_tab(&mut self, _: &ShiftTab, window: &mut Window, cx: &mut Context<Self>) {
        match self.view {
            SearchView::Grep => self.on_cycle_grep_mode(&CycleGrepMode, window, cx),
            SearchView::Files => self.on_select_prev(&SelectPrev, window, cx),
        }
    }

    // Cycle through the available grep modes.
    fn on_cycle_grep_mode(
        &mut self,
        _: &CycleGrepMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.grep_mode = match self.grep_mode {
            GrepMode::PlainText => GrepMode::Regex,
            GrepMode::Regex => GrepMode::Fuzzy,
            GrepMode::Fuzzy => GrepMode::PlainText,
        };
        self.switch_mode(window, cx);
    }

    // Restore the previous query from the local search history.
    fn on_cycle_previous_query(
        &mut self,
        _: &CyclePreviousQuery,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(query) = (|| {
            let guard = self.shared_query_tracker.read().ok()?;
            let tracker = guard.as_ref()?;
            let picker_guard = self.shared_picker.read().ok()?;
            let picker = picker_guard.as_ref()?;
            let project_path = picker.base_path();
            match self.view {
                SearchView::Files => tracker.get_historical_query(project_path, 0).ok().flatten(),
                SearchView::Grep => tracker
                    .get_historical_grep_query(project_path, 0)
                    .ok()
                    .flatten(),
            }
        })() else {
            self.status_message = Some("No query history".to_string());
            cx.notify();
            return;
        };

        self.text_field
            .update(cx, |field, cx| field.set_text(query, cx));
    }

    // Scroll the preview pane toward the top.
    fn on_preview_scroll_up(
        &mut self,
        _: &PreviewScrollUp,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preview_scroll_row = self.preview_scroll_row.saturating_sub(6);
        self.preview_scroll
            .scroll_to_item(self.preview_scroll_row, ScrollStrategy::Top);
        cx.notify();
    }

    // Scroll the preview pane toward the bottom.
    fn on_preview_scroll_down(
        &mut self,
        _: &PreviewScrollDown,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.preview_lines.is_empty() {
            self.preview_scroll_row =
                (self.preview_scroll_row + 6).min(self.preview_lines.len() - 1);
            self.preview_scroll
                .scroll_to_item(self.preview_scroll_row, ScrollStrategy::Top);
            cx.notify();
        }
    }

    // Switch back to file search mode.
    fn on_switch_files(&mut self, _: &SwitchFiles, window: &mut Window, cx: &mut Context<Self>) {
        if self.view != SearchView::Files {
            self.view = SearchView::Files;
            self.switch_mode(window, cx);
        }
    }

    // Switch to live grep mode.
    fn on_switch_grep(&mut self, _: &SwitchGrep, window: &mut Window, cx: &mut Context<Self>) {
        if self.view != SearchView::Grep {
            self.view = SearchView::Grep;
            self.grep_mode = GrepMode::PlainText;
            self.switch_mode(window, cx);
        }
    }

    // Return the text field focus handle so the window can focus it on startup.
    pub fn text_field_focus_handle(&self, cx: &App) -> FocusHandle {
        self.text_field.focus_handle(cx)
    }
}

impl Render for FffPicker {
    // Render the picker layout.
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current_theme_version = theme::version();
        if self.theme_version != current_theme_version {
            self.theme_version = current_theme_version;
            if self.selected < self.results.len() {
                self.load_preview(cx);
            }
        }
        let theme = cx.global::<AppTheme>().clone();
        let ui_font_family = theme.ui_font_family.clone();
        let buffer_font_family = theme.buffer_font_family.clone();
        let ui_font_size = px(theme.ui_font_size);
        let buffer_font_size = px(theme.buffer_font_size);
        let preview_line_height = px(theme.buffer_font_size);
        let results = self.results.clone();
        let preview_lines = self.preview_lines.clone();
        let selected = self.selected;
        let scan_done = self.scan_done;
        let total_files = self.total_files;
        let total_matched = self.total_matched;
        let indexed_count = self.indexed_count;
        let selected_count = self.selected_paths.len();
        let selected_paths = self.selected_paths.clone();
        let list_scroll = self.list_scroll.clone();
        let preview_scroll = self.preview_scroll.clone();
        let selected_path = results.get(selected).map(|item| item.absolute_path.clone());
        let preview_header_path = selected_path.as_ref().map(|path| {
            path.strip_prefix(&self.base_path)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned()
        });
        trace!(
            scan_done,
            results = results.len(),
            selected,
            preview_lines = preview_lines.len(),
            selected_count,
            view = ?self.view,
            query = %self.query,
            status_message = ?self.status_message,
            "rendering picker"
        );
        let preview_placeholder = if !scan_done {
            ""
        } else if self.preview_loading_visible {
            "Loading\u{2026}"
        } else if selected_path.is_some() && preview_lines.is_empty() {
            "No preview"
        } else if self.view == SearchView::Grep && self.query.trim().is_empty() {
            "Type to grep"
        } else {
            "No preview"
        };

        let mut status_text = if let Some(message) = self.status_message.clone() {
            message
        } else if !scan_done {
            if indexed_count > 0 {
                format!("indexing. {indexed_count} files")
            } else {
                String::new()
            }
        } else {
            let indexed = if total_files > 0 {
                total_files
            } else {
                indexed_count
            };
            format!(
                "{} shown  {selected_count} selected  {total_matched} matches  {indexed} indexed",
                results.len()
            )
        };
        let mode_hint = match self.view {
            SearchView::Files => "cmd-g grep",
            SearchView::Grep => "cmd-f files",
        };
        if self.view == SearchView::Grep {
            let mode = match self.grep_mode {
                GrepMode::PlainText => "plain",
                GrepMode::Regex => "regex",
                GrepMode::Fuzzy => "fuzzy",
            };
            if !status_text.is_empty() {
                status_text.push_str("  \u{2022}  ");
            }
            status_text.push_str(&format!("mode: {mode}  \u{21E7}tab mode"));
        }

        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_open_selected))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_toggle_selected))
            .on_action(cx.listener(Self::on_toggle_select_all))
            .on_action(cx.listener(Self::on_shift_tab))
            .on_action(cx.listener(Self::on_cycle_grep_mode))
            .on_action(cx.listener(Self::on_cycle_previous_query))
            .on_action(cx.listener(Self::on_preview_scroll_up))
            .on_action(cx.listener(Self::on_preview_scroll_down))
            .on_action(cx.listener(Self::on_switch_files))
            .on_action(cx.listener(Self::on_switch_grep))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme.bg))
            .text_color(rgb(theme.text_primary))
            .text_size(ui_font_size)
            .when_some(ui_font_family.clone(), |this, family| this.font_family(family))
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .flex()
                    .flex_row()
                    .overflow_hidden()
                    .child(
                        div()
                            .w(px(theme.picker_pane_width))
                            .h_full()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                    .child(
                div()
                    .flex_1()
                    .w_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .when(!scan_done, |this| {
                        let label = if indexed_count > 0 {
                            format!("Indexing {indexed_count} files")
                        } else {
                            "Indexing".to_string()
                        };

                        this.child(
                            div()
                                .flex_1()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_sm()
                                .text_color(rgb(theme.text_dim))
                                .child(label),
                        )
                    })
                    .when(scan_done && results.is_empty(), |this| {
                        if self.view == SearchView::Grep && self.query.trim().is_empty() {
                            let hint_row = |key: &'static str, desc: &'static str| {
                                div()
                                    .flex()
                                    .gap(px(8.0))
                                    .text_xs()
                                    .text_color(rgb(theme.text_dim))
                                    .child(div().w(px(140.0)).child(key))
                                    .child(div().child(desc))
                            };
                            this.child(
                                div()
                                    .flex_1()
                                    .size_full()
                                    .px(px(20.0))
                                    .pt(px(20.0))
                                    .flex()
                                    .flex_col()
                                    .gap(px(4.0))
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(rgb(theme.text_dim))
                                            .child("Start typing to search file contents..."),
                                    )
                                    .child(div().h(px(8.0)))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(theme.text_secondary))
                                            .child("Tips:"),
                                    )
                                    .child(hint_row(
                                        "\"pattern *.rs\"",
                                        "search only in Rust files",
                                    ))
                                    .child(hint_row(
                                        "\"pattern /src/\"",
                                        "limit search to src/ directory",
                                    ))
                                    .child(hint_row(
                                        "\"!test pattern\"",
                                        "exclude test files",
                                    )),
                            )
                        } else {
                            this.child(
                                div()
                                    .flex_1()
                                    .size_full()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_sm()
                                    .text_color(rgb(theme.text_dim))
                                    .child("No files matched"),
                            )
                        }
                    })
                    .when(scan_done && !results.is_empty(), {
                        let row_theme = theme.clone();
                        move |this| {
                            let theme = row_theme.clone();
                            let list_panel = div()
                            .w_full()
                            .h_full()
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            .child(
                                uniform_list(
                                    "results",
                                    results.len(),
                                    cx.processor(move |_this, range: std::ops::Range<usize>, _window, cx| {
                                        let total = results.len();
                                        range
                                            .map(|visual_i| {
                                                // List renders bottom-up: rank 0 sits at the
                                                // visual bottom, just above the input.
                                                let i = total - 1 - visual_i;
                                                let item = &results[i];
                                                let is_selected = i == selected;
                                                let is_marked = selected_paths.contains(&item.absolute_path);
                                                let badge_color = if is_selected {
                                                    theme.text_primary
                                                } else {
                                                    theme.text_secondary
                                                };
                                                // Monospace approximation: a char is roughly 0.6 of the em.
                                                let char_px = theme.ui_font_size * 0.6;
                                                let overhead_px = 101.0;
                                                let filename_chars =
                                                    item.file_name.chars().count() as f32;
                                                let avail_px = (theme.picker_pane_width
                                                    - filename_chars * char_px
                                                    - overhead_px)
                                                    .max(0.0);
                                                let path_max_chars =
                                                    ((avail_px / char_px) as usize).max(12);
                                                let display_dir =
                                                    shorten_dir_for_row(&item.dir, path_max_chars);
                                                let bar_color =
                                                    git_status_bar_color(item.git_status.as_deref());
                                                let file_icon =
                                                    theme::file_icon_for_path(&item.absolute_path);

                                                let content_match: Option<(String, Vec<Range<usize>>)> =
                                                    if item.match_ranges.is_empty() {
                                                        item.grep_matches.first().map(|m| {
                                                            let strip = m.line_content.len()
                                                                - m.line_content.trim_start().len();
                                                            let text =
                                                                m.line_content.trim().to_string();
                                                            let ranges = m
                                                                .byte_ranges
                                                                .iter()
                                                                .filter_map(|&(s, e)| {
                                                                    let s = (s as usize)
                                                                        .saturating_sub(strip);
                                                                    let e = (e as usize)
                                                                        .saturating_sub(strip)
                                                                        .min(text.len());
                                                                    if s < e { Some(s..e) } else { None }
                                                                })
                                                                .collect();
                                                            (text, ranges)
                                                        })
                                                    } else {
                                                        None
                                                    };

                                                div()
                                                    .id(("row", i))
                                                    .w_full()
                                                    .h(px(28.0))
                                                    .flex()
                                                    .items_center()
                                                    .bg(if is_selected {
                                                        rgb(theme.selected_row)
                                                    } else {
                                                        rgb(theme.bg)
                                                    })
                                                    .hover(|s| s.bg(rgb(theme.hover_row)))
                                                    .cursor_pointer()
                                                    .on_click(cx.listener(move |this, _, window, cx| {
                                                        this.on_select_row(i, window, cx);
                                                    }))
                                                    .child({
                                                        let mut bar = div()
                                                            .w(px(3.0))
                                                            .h(px(18.0))
                                                            .flex_shrink_0();
                                                        if let Some(color) = bar_color {
                                                            bar = bar.bg(rgb(color));
                                                        }
                                                        bar
                                                    })
                                                    .child(
                                                        div()
                                                            .pl(px(10.0))
                                                            .pr(px(12.0))
                                                            .flex_1()
                                                            .min_w(px(0.0))
                                                            .flex()
                                                            .items_center()
                                                            .gap(px(8.0))
                                                            .child(
                                                                div()
                                                                    .flex_1()
                                                                    .min_w(px(0.0))
                                                                    .overflow_hidden()
                                                                    .flex()
                                                                    .items_center()
                                                                    .gap(px(8.0))
                                                                    .text_sm()
                                                                    .child(render_file_icon(
                                                                        file_icon.clone(),
                                                                        theme.icon_muted,
                                                                    ))
                                                                    .when(content_match.is_some(), |d| {
                                                                        let (text, ranges) =
                                                                            content_match.as_ref().unwrap();
                                                                        d.child(
                                                                            div()
                                                                                .text_color(rgb(
                                                                                    theme.text_dim,
                                                                                ))
                                                                                .flex_shrink_0()
                                                                                .child(
                                                                                    item.file_name.clone(),
                                                                                ),
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .text_color(rgb(
                                                                                    theme.text_secondary,
                                                                                ))
                                                                                .flex_shrink_0()
                                                                                .child(format!(
                                                                                    ":{}",
                                                                                    item.grep_matches
                                                                                        .first()
                                                                                        .map(|m| m.line_number)
                                                                                        .unwrap_or(0)
                                                                                )),
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .flex_1()
                                                                                .min_w(px(0.0))
                                                                                .overflow_hidden()
                                                                                .child(
                                                                                    render_highlighted(
                                                                                        text,
                                                                                        ranges,
                                                                                        &theme,
                                                                                    ),
                                                                                ),
                                                                        )
                                                                    })
                                                                    .when(content_match.is_none(), |d| {
                                                                        d.child(
                                                                            div()
                                                                                .flex_shrink_0()
                                                                                .child(render_highlighted(
                                                                                    &item.file_name,
                                                                                    &item.match_ranges,
                                                                                    &theme,
                                                                                )),
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .text_xs()
                                                                                .text_color(rgb(
                                                                                    theme.text_secondary,
                                                                                ))
                                                                                .min_w(px(0.0))
                                                                                .overflow_hidden()
                                                                                .child(display_dir.clone()),
                                                                        )
                                                                    }),
                                                            )
                                                            .child(
                                                                div()
                                                                    .flex()
                                                                    .flex_shrink_0()
                                                                    .items_center()
                                                                    .gap(px(6.0))
                                                                    .when(
                                                                        !item.grep_matches.is_empty(),
                                                                        |this| {
                                                                            this.child(
                                                                                div()
                                                                                    .text_xs()
                                                                                    .text_color(rgb(badge_color))
                                                                                    .flex_shrink_0()
                                                                                    .child(format!(
                                                                                        "{}",
                                                                                        item.grep_matches
                                                                                            .len()
                                                                                    )),
                                                                            )
                                                                        },
                                                                    )
                                                                    .when(
                                                                        item.frecency_score > 0,
                                                                        |this| {
                                                                            this.child(
                                                                                div()
                                                                                    .text_xs()
                                                                                    .text_color(rgb(badge_color))
                                                                                    .flex_shrink_0()
                                                                                    .child(format!(
                                                                                        "\u{2728} {}",
                                                                                        item.frecency_score
                                                                                    )),
                                                                            )
                                                                        },
                                                                    )
                                                                    .when(is_marked, |this| {
                                                                        this.child(
                                                                            div()
                                                                                .text_xs()
                                                                                .text_color(rgb(badge_color))
                                                                                .flex_shrink_0()
                                                                                .child("\u{25CF}"),
                                                                        )
                                                                    }),
                                                            ),
                                                    )
                                            })
                                            .collect()
                                    }),
                                )
                                .flex_1()
                                .w_full()
                                .track_scroll(list_scroll),
                            );
                            this.child(list_panel)
                        }
                    }),
                    )
                    .child(
                div()
                    .w_full()
                    .h(px(46.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .border_t_1()
                    .border_color(rgb(theme.border))
                    .text_size(buffer_font_size)
                    .when_some(buffer_font_family.clone(), |this, family| {
                        this.font_family(family)
                    })
                    .child(
                        div()
                            .text_color(rgb(theme.match_highlight))
                            .text_sm()
                            .child("🪿"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .w_full()
                            .min_w(px(0.0))
                            .child(self.text_field.clone()),
                    ),
                    )
            )
            .child(
                div()
                    .w(px(1.0))
                    .h_full()
                    .bg(rgb(theme.border))
                    .flex_shrink_0(),
            )
                    .child(
                        div()
                            .flex_1()
                            .h_full()
                            .flex()
                            .flex_col()
                            .bg(rgb(theme.preview_bg))
                            .text_size(buffer_font_size)
                            .when_some(buffer_font_family.clone(), |this, family| {
                                this.font_family(family)
                            })
                            .overflow_hidden()
                            .when_some(preview_header_path, |this, path| {
                                this.child(
                                    div()
                                        .w_full()
                                        .h(px(28.0))
                                        .px(px(12.0))
                                        .flex()
                                        .items_center()
                                        .border_b_1()
                                        .border_color(rgb(theme.border))
                                        .text_xs()
                                        .text_color(rgb(theme.text_secondary))
                                        .child(path),
                                )
                            })
                            .when(preview_lines.is_empty(), |this| {
                                this.child(
                                    div()
                                        .size_full()
                                        .flex()
                                .items_center()
                                .justify_center()
                                .text_xs()
                                .text_color(rgb(theme.text_dim))
                                .child(preview_placeholder),
                        )
                    })
                            .when(!preview_lines.is_empty(), |this| {
                        this.child(
                            uniform_list("preview", preview_lines.len(), move |range, _window, _cx| {
                                range
                                    .map(|i| {
                                        let line = &preview_lines[i];
                                        div()
                                            .id(("pl", i))
                                            .h(preview_line_height)
                                        .px(px(8.0))
                                        .flex()
                                        .items_center()
                                        .children(line.spans.iter().map(|span| {
                                                let mut element = div()
                                                    .text_xs()
                                                    .line_height(preview_line_height)
                                                    .text_color(rgb(span.color))
                                                    .when(span.bold, |d| d.font_weight(FontWeight::BOLD))
                                                    .when(span.italic, |d| d.italic())
                                                    .when(span.underline, |d| d.underline())
                                                    .when(span.strikethrough, |d| d.line_through());
                                                if let Some(bg_color) = span.bg {
                                                    element = element
                                                        .bg(rgba((bg_color << 8) | 0x77))
                                                        .px(px(3.0))
                                                        .rounded(px(4.0));
                                                }
                                                element.child(span.text.clone())
                                            }))
                                    })
                                    .collect()
                            })
                            .flex_1()
                            .w_full()
                            .track_scroll(preview_scroll),
                        )
                    }),
            )
            )
            .child(
                div()
                    .w_full()
                    .h(px(28.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .bg(rgb(theme.status_bar_bg))
                    .border_t_1()
                    .border_color(rgb(theme.border))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme.text_dim))
                            .child(status_text),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme.text_dim))
                            .child(match self.view {
                                SearchView::Grep => format!(
                                    "\u{2191}\u{2193} nav  tab toggle  {mode_hint}  \u{23CE} open  esc quit"
                                ),
                                SearchView::Files => format!(
                                    "\u{2191}\u{2193} nav  tab toggle  {mode_hint}  \u{23CE} open  esc quit"
                                ),
                            }),
                    ),
            )
    }
}
