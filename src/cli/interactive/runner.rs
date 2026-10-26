// Copyright (C) 2026 The TTFM Project Contributors
// See the CONTRIBUTORS file at the top-level directory of this distribution
// for a list of copyright holders.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::autocomplete::{fetch_candidates, Candidate};
use crate::cli::interactive::autocomplete::{
    CompletionPhase, CompletionState, InteractiveHinter, DEFAULT_QUERY,
};
use crate::cli::interactive::command::{
    dispatch_command, parse_command, print_menu,
};
use crate::cli::interactive::prompt_input::TerminalPromptInput;
use crate::cli::interactive::state::State;
use crate::config::Config;
use crate::db::Store;
use crate::edit::WriteOptions;
use crate::tag::TagRegistry;
use reedline::{
    default_emacs_keybindings, ColumnarMenu, EditCommand, EditMode, Emacs,
    FileBackedHistory, Hinter, KeyCode, KeyModifiers, Menu, MenuBuilder,
    MenuEvent, MenuSettings, Painter, PromptEditMode, Reedline, ReedlineEvent,
    ReedlineMenu, ReedlineRawEvent, Suggestion, UndoBehavior,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub struct SharedHinter(pub Arc<Mutex<InteractiveHinter>>);

impl reedline::Hinter for SharedHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        history: &dyn reedline::History,
        use_ansi_coloring: bool,
        cwd: &str,
    ) -> String {
        if let Ok(mut h) = self.0.lock() {
            h.handle(line, pos, history, use_ansi_coloring, cwd)
        } else {
            String::new()
        }
    }

    fn complete_hint(&self) -> String {
        if let Ok(h) = self.0.lock() {
            h.complete_hint()
        } else {
            String::new()
        }
    }

    fn next_hint_token(&self) -> String {
        if let Ok(h) = self.0.lock() {
            h.next_hint_token()
        } else {
            String::new()
        }
    }
}

pub fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub type CandidateFetcher = Box<dyn Fn(&str, usize) -> Vec<Candidate> + Send>;
pub type LastQueryProvider = Box<dyn Fn() -> String + Send>;
pub type SearchedQueryProvider = Box<dyn Fn() -> Option<String> + Send>;
pub type EscapeHandler = Box<dyn Fn() -> bool + Send>;

pub struct PagingColumnarMenu {
    inner: ColumnarMenu,
    current_offset: Arc<AtomicUsize>,
    page_size: usize,
    current_page_index: usize,
    base_line: String,
    base_pos: usize,
    capture_base: bool,
    page_fetch_in_flight: bool,
    select_last_on_fetch: bool,
    allow_partial: bool,
    completion: Option<Arc<Mutex<CompletionState>>>,
    shared_query_base: Option<Arc<Mutex<Option<(String, usize)>>>>,
}

impl PagingColumnarMenu {
    pub fn new(
        inner: ColumnarMenu,
        current_offset: Arc<AtomicUsize>,
        page_size: usize,
    ) -> Self {
        Self {
            inner,
            current_offset,
            page_size,
            current_page_index: 0,
            base_line: String::new(),
            base_pos: 0,
            capture_base: false,
            page_fetch_in_flight: false,
            select_last_on_fetch: false,
            allow_partial: false,
            completion: None,
            shared_query_base: None,
        }
    }

    pub fn with_completion_state(
        mut self,
        state: Arc<Mutex<CompletionState>>,
    ) -> Self {
        self.completion = Some(state);
        self
    }

    pub fn with_query_base(
        mut self,
        slot: Arc<Mutex<Option<(String, usize)>>>,
    ) -> Self {
        self.shared_query_base = Some(slot);
        self
    }

    fn apply_query_base(&mut self, editor: &mut reedline::Editor) {
        if self.page_fetch_in_flight {
            let line = self.base_line.clone();
            let pos = self.base_pos;
            editor.edit_buffer(
                |b| {
                    b.set_buffer(line);
                    b.set_insertion_point(pos);
                },
                UndoBehavior::CreateUndoPoint,
            );
        } else if self.capture_base {
            self.base_line = editor.get_buffer().to_string();
            self.base_pos = editor.line_buffer().insertion_point();
            self.capture_base = false;
            self.persist_query_base();
        }
    }

    fn persist_query_base(&self) {
        if let Some(ref slot) = self.shared_query_base {
            if let Ok(mut q) = slot.lock() {
                *q = Some((self.base_line.clone(), self.base_pos));
            }
        }
        self.enter_menu_phase();
    }

    fn enter_menu_phase(&self) {
        let Some(ref st) = self.completion else {
            return;
        };
        let Ok(mut s) = st.lock() else {
            return;
        };
        s.enter_menu(self.base_line.clone(), self.base_pos);
    }

    fn leave_menu_phase(&self) {
        let Some(ref st) = self.completion else {
            return;
        };
        let Ok(mut s) = st.lock() else {
            return;
        };
        if s.phase() == CompletionPhase::MenuPaging {
            s.set_off();
        }
    }

    fn land_on_last_candidate(
        &mut self,
        editor: &mut reedline::Editor,
        completer: &mut dyn reedline::Completer,
        painter: &Painter,
    ) {
        self.select_last_on_fetch = false;
        let n = self.inner.get_values().len();
        self.current_page_index = n.saturating_sub(1);
        if n > 1 {
            self.inner.menu_event(MenuEvent::PreviousElement);
            self.inner
                .update_working_details(editor, completer, painter);
        }
    }
}

impl MenuBuilder for PagingColumnarMenu {
    fn settings_mut(&mut self) -> &mut MenuSettings {
        self.inner.settings_mut()
    }
}

pub fn can_quick_complete_values(
    line: &str,
    pos: usize,
    values: &[Suggestion],
) -> bool {
    let [only] = values else {
        return false;
    };
    crate::autocomplete::ghost_suffix_for_span(
        line,
        pos,
        (only.span.start, only.span.end),
        &only.value,
    )
    .is_some()
}

impl Menu for PagingColumnarMenu {
    fn settings(&self) -> &MenuSettings {
        self.inner.settings()
    }

    fn is_active(&self) -> bool {
        self.inner.is_active()
    }

    fn can_quick_complete(&self) -> bool {
        false
    }

    fn can_partially_complete(
        &mut self,
        _values_updated: bool,
        editor: &mut reedline::Editor,
        completer: &mut dyn reedline::Completer,
    ) -> bool {
        if !self.allow_partial
            || self.page_fetch_in_flight
            || self.current_offset.load(Ordering::SeqCst) > 0
        {
            return false;
        }
        self.allow_partial = false;
        if self.capture_base {
            self.update_values(editor, completer);
            self.inner.menu_event(MenuEvent::Activate(true));
        }
        let values = self.inner.get_values();
        if can_quick_complete_values(&self.base_line, self.base_pos, values) {
            self.inner.replace_in_buffer(editor);
            self.menu_event(MenuEvent::Deactivate);
            return true;
        }
        if values.len() <= 1
            || values.iter().any(|s| s.span.start == s.span.end)
        {
            return false;
        }
        let did = self.inner.can_partially_complete(true, editor, completer);
        if did {
            self.base_line = editor.get_buffer().to_string();
            self.base_pos = editor.line_buffer().insertion_point();
            self.persist_query_base();
        }
        did
    }

    fn update_values(
        &mut self,
        editor: &mut reedline::Editor,
        completer: &mut dyn reedline::Completer,
    ) {
        self.apply_query_base(editor);
        self.inner.update_values(editor, completer);
        if self.select_last_on_fetch {
            let n = self.inner.get_values().len();
            self.current_page_index = n.saturating_sub(1);
        }
        self.page_fetch_in_flight = false;
    }

    fn update_working_details(
        &mut self,
        editor: &mut reedline::Editor,
        completer: &mut dyn reedline::Completer,
        painter: &Painter,
    ) {
        self.apply_query_base(editor);
        self.inner
            .update_working_details(editor, completer, painter);
        if self.inner.get_values().is_empty()
            && self.current_offset.load(Ordering::SeqCst) > 0
        {
            self.current_offset.store(0, Ordering::SeqCst);
            self.current_page_index = 0;
            self.select_last_on_fetch = false;
            self.inner.update_values(editor, completer);
        } else if self.select_last_on_fetch {
            self.land_on_last_candidate(editor, completer, painter);
        }
        self.page_fetch_in_flight = false;
    }

    fn replace_in_buffer(&self, editor: &mut reedline::Editor) {
        self.inner.replace_in_buffer(editor);
    }

    fn menu_required_lines(&self, terminal_columns: u16) -> u16 {
        self.inner.menu_required_lines(terminal_columns)
    }

    fn menu_string(
        &self,
        available_lines: u16,
        use_ansi_coloring: bool,
    ) -> String {
        self.inner.menu_string(available_lines, use_ansi_coloring)
    }

    fn min_rows(&self) -> u16 {
        self.inner.min_rows()
    }

    fn get_values(&self) -> &[Suggestion] {
        self.inner.get_values()
    }

    fn menu_event(&mut self, event: MenuEvent) {
        match event {
            MenuEvent::Activate(_) | MenuEvent::Edit(_) => {
                self.current_offset.store(0, Ordering::SeqCst);
                self.current_page_index = 0;
                self.page_fetch_in_flight = false;
                self.select_last_on_fetch = false;
                self.capture_base = true;
                let is_activate = matches!(event, MenuEvent::Activate(_));
                self.allow_partial = is_activate;
                let inner_ev = if is_activate {
                    MenuEvent::Activate(false)
                } else {
                    MenuEvent::Edit(false)
                };
                self.inner.menu_event(inner_ev);
            }
            MenuEvent::Deactivate => {
                self.current_offset.store(0, Ordering::SeqCst);
                self.current_page_index = 0;
                self.page_fetch_in_flight = false;
                self.select_last_on_fetch = false;
                self.capture_base = false;
                self.inner.menu_event(event);
                self.leave_menu_phase();
            }
            MenuEvent::NextElement => {
                self.allow_partial = false;
                if self.page_fetch_in_flight {
                    return;
                }
                self.select_last_on_fetch = false;
                let cur_vals = self.inner.get_values().len();
                if cur_vals == 0 {
                    self.inner.menu_event(event);
                } else if self.current_page_index + 1 < cur_vals {
                    self.current_page_index += 1;
                    self.inner.menu_event(event);
                } else if cur_vals == self.page_size {
                    self.current_offset
                        .fetch_add(self.page_size, Ordering::SeqCst);
                    self.current_page_index = 0;
                    self.page_fetch_in_flight = true;
                    self.inner.menu_event(MenuEvent::Edit(false));
                } else if self.current_offset.load(Ordering::SeqCst) > 0 {
                    self.current_offset.store(0, Ordering::SeqCst);
                    self.current_page_index = 0;
                    self.page_fetch_in_flight = true;
                    self.inner.menu_event(MenuEvent::Edit(false));
                } else {
                    self.current_page_index = 0;
                    self.inner.menu_event(event);
                }
            }
            MenuEvent::PreviousElement => {
                self.allow_partial = false;
                if self.page_fetch_in_flight {
                    return;
                }
                let cur_vals = self.inner.get_values().len();
                if cur_vals == 0 {
                    self.inner.menu_event(event);
                } else if self.current_page_index > 0 {
                    self.current_page_index -= 1;
                    self.select_last_on_fetch = false;
                    self.inner.menu_event(event);
                } else if self.current_offset.load(Ordering::SeqCst) > 0 {
                    let prev = self
                        .current_offset
                        .load(Ordering::SeqCst)
                        .saturating_sub(self.page_size);
                    self.current_offset.store(prev, Ordering::SeqCst);
                    self.current_page_index = self.page_size.saturating_sub(1);
                    self.select_last_on_fetch = true;
                    self.page_fetch_in_flight = true;
                    self.inner.menu_event(MenuEvent::Edit(false));
                } else {
                    self.current_page_index = cur_vals.saturating_sub(1);
                    self.inner.menu_event(event);
                }
            }
            other => self.inner.menu_event(other),
        }
    }
}

pub struct InteractiveCompleter {
    pub fetcher: CandidateFetcher,
    pub last_query_provider: LastQueryProvider,
    pub searched_query_provider: Option<SearchedQueryProvider>,
    pub untag_query_provider: Option<SearchedQueryProvider>,
    pub last_path_provider: Option<LastQueryProvider>,
    pub hinter: Arc<Mutex<InteractiveHinter>>,
    current_offset: Option<Arc<AtomicUsize>>,
    query_base: Arc<Mutex<Option<(String, usize)>>>,
}

impl InteractiveCompleter {
    pub fn new(
        fetcher: CandidateFetcher,
        last_query: impl Into<String>,
    ) -> Self {
        let q = last_query.into();
        Self {
            fetcher,
            last_query_provider: Box::new(move || q.clone()),
            searched_query_provider: None,
            untag_query_provider: None,
            last_path_provider: None,
            hinter: Arc::new(Mutex::new(InteractiveHinter::new())),
            current_offset: None,
            query_base: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_provider(
        fetcher: CandidateFetcher,
        provider: LastQueryProvider,
    ) -> Self {
        Self {
            fetcher,
            last_query_provider: provider,
            searched_query_provider: None,
            untag_query_provider: None,
            last_path_provider: None,
            hinter: Arc::new(Mutex::new(InteractiveHinter::new())),
            current_offset: None,
            query_base: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_query_provider(
        fetcher: CandidateFetcher,
        provider: SearchedQueryProvider,
    ) -> Self {
        Self {
            fetcher,
            last_query_provider: Box::new(|| String::new()),
            searched_query_provider: Some(provider),
            untag_query_provider: None,
            last_path_provider: None,
            hinter: Arc::new(Mutex::new(InteractiveHinter::new())),
            current_offset: None,
            query_base: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_paths(
        fetcher: CandidateFetcher,
        query_provider: LastQueryProvider,
        path_provider: LastQueryProvider,
        hinter: Arc<Mutex<InteractiveHinter>>,
    ) -> Self {
        Self {
            fetcher,
            last_query_provider: query_provider,
            searched_query_provider: None,
            untag_query_provider: None,
            last_path_provider: Some(path_provider),
            hinter,
            current_offset: None,
            query_base: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_searched_query_provider(
        mut self,
        provider: SearchedQueryProvider,
    ) -> Self {
        self.searched_query_provider = Some(provider);
        self
    }

    pub fn with_untag_query_provider(
        mut self,
        provider: SearchedQueryProvider,
    ) -> Self {
        self.untag_query_provider = Some(provider);
        self
    }

    pub fn with_paging_offset(mut self, offset: Arc<AtomicUsize>) -> Self {
        self.current_offset = Some(offset);
        self
    }

    pub fn shared_query_base(&self) -> Arc<Mutex<Option<(String, usize)>>> {
        Arc::clone(&self.query_base)
    }

    pub fn query_base_value(&self) -> Option<(String, usize)> {
        self.query_base.lock().ok().and_then(|q| q.clone())
    }
}

impl reedline::Completer for InteractiveCompleter {
    fn complete(
        &mut self,
        line: &str,
        pos: usize,
    ) -> Vec<reedline::Suggestion> {
        let offset = self
            .current_offset
            .as_ref()
            .map(|o| o.load(Ordering::SeqCst))
            .unwrap_or(0);
        let (line_buf, pos) = if offset == 0 {
            let base = (line.to_string(), pos);
            if let Ok(mut q) = self.query_base.lock() {
                *q = Some(base.clone());
            }
            base
        } else {
            self.query_base
                .lock()
                .ok()
                .and_then(|q| q.clone())
                .unwrap_or_else(|| (line.to_string(), pos))
        };
        let line = line_buf.as_str();
        let prefix = &line[..pos.min(line.len())];
        let trimmed = prefix.trim_start();
        let is_cancelled = self
            .hinter
            .lock()
            .map(|h| h.is_trace_cancelled())
            .unwrap_or(false);

        if !is_cancelled {
            if trimmed == "s" && line == "s" {
                let last_query = (self.last_query_provider)();
                if !last_query.is_empty() {
                    return vec![reedline::Suggestion {
                        value: format!(" {last_query}"),
                        description: None,
                        extra: None,
                        span: reedline::Span {
                            start: pos,
                            end: pos,
                        },
                        append_whitespace: false,
                        display_override: None,
                        match_indices: None,
                        style: None,
                    }];
                }
            }

            if (trimmed == "i" || trimmed == "i ") && line.trim() == "i" {
                let p = self
                    .last_path_provider
                    .as_ref()
                    .map(|p| (p)())
                    .unwrap_or_else(|| ".".to_string());
                let quoted_p = if p.contains(' ') && !p.starts_with('"') {
                    format!("\"{p}\"")
                } else {
                    p
                };
                let val = if trimmed == "i" {
                    format!(" {quoted_p}")
                } else {
                    quoted_p
                };
                return vec![reedline::Suggestion {
                    value: val,
                    description: None,
                    extra: None,
                    span: reedline::Span {
                        start: pos,
                        end: pos,
                    },
                    append_whitespace: false,
                    display_override: None,
                    match_indices: None,
                    style: None,
                }];
            }

            if (trimmed == "t" || trimmed == "u")
                && (line.trim() == "t" || line.trim() == "u")
            {
                let query_opt = if trimmed == "u" {
                    self.untag_query_provider.as_ref().and_then(|p| (p)())
                } else if let Some(ref p) = self.searched_query_provider {
                    (p)()
                } else {
                    let q = (self.last_query_provider)();
                    if q.is_empty() {
                        None
                    } else {
                        Some(q)
                    }
                };
                if let Some(query) = query_opt {
                    if !query.is_empty() {
                        let escaped = query.replace('"', "\\\"");
                        return vec![reedline::Suggestion {
                            value: format!(" \"{escaped}\" "),
                            description: None,
                            extra: None,
                            span: reedline::Span {
                                start: pos,
                                end: pos,
                            },
                            append_whitespace: false,
                            display_override: None,
                            match_indices: None,
                            style: None,
                        }];
                    }
                }
            }
        }

        (self.fetcher)(line, pos)
            .into_iter()
            .map(|c| reedline::Suggestion {
                value: c.raw,
                description: None,
                extra: None,
                span: reedline::Span {
                    start: c.span.0,
                    end: c.span.1,
                },
                append_whitespace: false,
                display_override: None,
                match_indices: None,
                style: None,
            })
            .collect()
    }
}

fn edit_command_clears_completion(cmd: &EditCommand) -> bool {
    matches!(
        cmd,
        EditCommand::Backspace
            | EditCommand::BackspaceWord
            | EditCommand::Delete
            | EditCommand::DeleteWord
            | EditCommand::MoveLeft { .. }
            | EditCommand::MoveRight { .. }
            | EditCommand::MoveWordLeft { .. }
            | EditCommand::MoveWordRight { .. }
            | EditCommand::MoveWordRightStart { .. }
            | EditCommand::MoveWordRightEnd { .. }
            | EditCommand::MoveBigWordLeft { .. }
            | EditCommand::MoveBigWordRightStart { .. }
            | EditCommand::MoveBigWordRightEnd { .. }
            | EditCommand::MoveToStart { .. }
            | EditCommand::MoveToEnd { .. }
            | EditCommand::MoveToLineStart { .. }
            | EditCommand::MoveToLineEnd { .. }
            | EditCommand::MoveToLineNonBlankStart { .. }
            | EditCommand::MoveLineUp { .. }
            | EditCommand::MoveLineDown { .. }
            | EditCommand::MoveToPosition { .. }
    )
}

fn event_clears_completion(
    event: &ReedlineEvent,
    phase: CompletionPhase,
) -> bool {
    match event {
        ReedlineEvent::Left
        | ReedlineEvent::Right
        | ReedlineEvent::Up
        | ReedlineEvent::Down => phase == CompletionPhase::GhostActive,
        ReedlineEvent::Edit(cmds) => {
            cmds.iter().any(edit_command_clears_completion)
        }
        ReedlineEvent::UntilFound(events) => {
            if events
                .iter()
                .any(|e| matches!(e, ReedlineEvent::HistoryHintComplete))
            {
                return false;
            }
            let moves = events.iter().any(|e| {
                matches!(
                    e,
                    ReedlineEvent::Left
                        | ReedlineEvent::Right
                        | ReedlineEvent::Up
                        | ReedlineEvent::Down
                        | ReedlineEvent::MenuLeft
                        | ReedlineEvent::MenuRight
                )
            });
            moves && phase == CompletionPhase::GhostActive
        }
        _ => false,
    }
}

pub struct InteractiveEditMode {
    inner: Emacs,
    on_escape: Option<EscapeHandler>,
    hinter: Option<Arc<Mutex<InteractiveHinter>>>,
    completion: Option<Arc<Mutex<CompletionState>>>,
    buffer_len: usize,
}

impl InteractiveEditMode {
    pub fn new(inner: Emacs) -> Self {
        Self {
            inner,
            on_escape: None,
            hinter: None,
            completion: None,
            buffer_len: 0,
        }
    }

    pub fn notify_buffer_length(&mut self, len: usize) {
        self.buffer_len = len;
    }

    pub fn set_on_escape(&mut self, on_escape: EscapeHandler) {
        self.on_escape = Some(on_escape);
    }

    pub fn set_completion_state(&mut self, state: Arc<Mutex<CompletionState>>) {
        self.completion = Some(state);
    }

    pub fn set_hinter(&mut self, hinter: Arc<Mutex<InteractiveHinter>>) {
        if let Ok(h) = hinter.lock() {
            self.completion = Some(h.completion_state());
        }
        self.hinter = Some(hinter);
    }

    fn current_phase(&self) -> CompletionPhase {
        if let Some(ref st) = self.completion {
            if let Ok(s) = st.lock() {
                return s.phase();
            }
        }
        if let Some(ref h) = self.hinter {
            if let Ok(hg) = h.lock() {
                return hg.phase();
            }
        }
        CompletionPhase::Off
    }

    fn dismiss_completion(&mut self) -> Option<(String, usize)> {
        let restore = if let Some(ref st) = self.completion {
            st.lock().ok().and_then(|mut s| s.dismiss())
        } else {
            None
        };
        if let Some(ref h) = self.hinter {
            if let Ok(mut hg) = h.lock() {
                hg.cancel_trace();
            }
        }
        restore
    }

    fn clear_ghost_on_nav_or_delete(&mut self, event: &ReedlineEvent) {
        if !event_clears_completion(event, self.current_phase()) {
            return;
        }
        if let Some(ref st) = self.completion {
            if let Ok(mut s) = st.lock() {
                s.set_off();
            }
        }
        if let Some(ref h) = self.hinter {
            if let Ok(mut hg) = h.lock() {
                hg.clear_active_hint();
            }
        }
    }

    fn menu_paging_esc_event(
        &self,
        restore: Option<(String, usize)>,
    ) -> ReedlineEvent {
        let Some((line, pos)) = restore else {
            return ReedlineEvent::Esc;
        };
        if line.is_empty() && self.buffer_len > 0 {
            return ReedlineEvent::Esc;
        }
        ReedlineEvent::Multiple(vec![
            ReedlineEvent::Esc,
            ReedlineEvent::Edit(vec![
                EditCommand::Clear,
                EditCommand::InsertString(line),
                EditCommand::MoveToPosition {
                    position: pos,
                    select: false,
                },
            ]),
        ])
    }

    fn bare_command_enter(
        &self,
        event: &ReedlineEvent,
    ) -> Option<ReedlineEvent> {
        let ReedlineEvent::UntilFound(events) = event else {
            return None;
        };
        if !events.contains(&ReedlineEvent::Submit) {
            return None;
        }
        let hinter = self.hinter.as_ref()?.lock().ok()?;
        let cmd = hinter.current_line().trim_start();
        if !matches!(cmd, "s" | "t" | "u" | "i")
            || !hinter.complete_hint().is_empty()
        {
            return None;
        }
        Some(ReedlineEvent::Edit(vec![
            EditCommand::Clear,
            EditCommand::InsertString(format!("{cmd} ")),
        ]))
    }

    pub fn handle_reedline_event(
        &mut self,
        event: ReedlineEvent,
    ) -> ReedlineEvent {
        if let Some(restored) = self.bare_command_enter(&event) {
            return restored;
        }
        self.clear_ghost_on_nav_or_delete(&event);
        match event {
            ReedlineEvent::Esc => match self.current_phase() {
                CompletionPhase::MenuPaging => {
                    let restore = self.dismiss_completion();
                    self.menu_paging_esc_event(restore)
                }
                CompletionPhase::GhostActive => {
                    self.dismiss_completion();
                    ReedlineEvent::Repaint
                }
                CompletionPhase::Off => {
                    if self.buffer_len > 0 {
                        return ReedlineEvent::Edit(vec![EditCommand::Clear]);
                    }
                    if let Some(ref on_escape) = self.on_escape {
                        if (on_escape)() {
                            return ReedlineEvent::ExecuteHostCommand(
                                "q".to_string(),
                            );
                        }
                    }
                    ReedlineEvent::Edit(vec![EditCommand::Clear])
                }
            },
            other => other,
        }
    }
}

impl EditMode for InteractiveEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        if let Some(ref h) = self.hinter {
            if let Ok(hg) = h.lock() {
                self.buffer_len = hg.current_line().len();
            }
        }
        let res = self.inner.parse_event(event);
        self.handle_reedline_event(res)
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }
}

pub fn build_keybindings() -> reedline::Keybindings {
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Enter,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::Submit,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::UntilFound(vec![ReedlineEvent::MenuPrevious]),
    );
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::BackTab,
        ReedlineEvent::UntilFound(vec![ReedlineEvent::MenuPrevious]),
    );
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Right,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::MenuRight,
            ReedlineEvent::Right,
        ]),
    );
    keybindings
}

pub fn sync_hinter_state(
    hinter: &Arc<Mutex<InteractiveHinter>>,
    state: &State,
    fallback: &str,
) {
    if let Ok(mut h) = hinter.lock() {
        let searched = state.last_query().map(str::to_string);
        let search_q = state.last_query().unwrap_or(fallback);
        let search_q = if search_q.is_empty() {
            DEFAULT_QUERY
        } else {
            search_q
        };
        h.set_last_query(search_q);
        h.set_searched_query(searched);
        h.set_untag_query(state.last_untag_query().map(str::to_string));
    }
}

pub fn sync_hinter_last_query(
    hinter: &Mutex<InteractiveHinter>,
    last_query: Option<&str>,
    fallback: &str,
) {
    if let Ok(mut h) = hinter.lock() {
        let q = last_query.unwrap_or(fallback);
        let q = if q.is_empty() { DEFAULT_QUERY } else { q };
        h.set_last_query(q);
        h.set_searched_query(last_query.map(str::to_string));
    }
}

pub fn run_interactive_terminal(
    store: &Store,
    registry: &TagRegistry,
    config: &Config,
    write_options: WriteOptions,
    initial_query: Option<&str>,
    quiet: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(Mutex::new(State::new()));
    let last_path = Arc::new(Mutex::new(".".to_string()));

    let home_dir = crate::get_ttfm_home()
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let _ = std::fs::create_dir_all(&home_dir);
    let history_path = home_dir.join("history");
    let history: Box<dyn reedline::History> =
        match FileBackedHistory::with_file(1000, history_path) {
            Ok(h) => Box::new(h),
            Err(_) => Box::new(FileBackedHistory::default()),
        };

    let keybindings = build_keybindings();
    let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));

    let shared_offset = Arc::new(AtomicUsize::new(0));
    let offset_for_fetcher = Arc::clone(&shared_offset);
    let reg_for_fetcher = registry.clone();
    pub const DEFAULT_INTERACTIVE_COMPLETION_LIMIT: usize = 40;

    let store_for_fetcher = store.try_clone().ok();
    let fetcher: CandidateFetcher = Box::new(move |line: &str, pos: usize| {
        if let Some(ref st) = store_for_fetcher {
            let cur_offset = offset_for_fetcher.load(Ordering::SeqCst);
            fetch_candidates(
                st,
                &reg_for_fetcher,
                line,
                pos,
                cur_offset,
                DEFAULT_INTERACTIVE_COMPLETION_LIMIT,
            )
        } else {
            vec![]
        }
    });
    let store_for_hinter = store.try_clone().ok().map(Mutex::new);
    let reg_for_hinter = registry.clone();
    if let Ok(mut h) = hinter.lock() {
        h.set_candidate_source(Arc::new(move |line, pos| {
            let Some(ref st) = store_for_hinter else {
                return vec![];
            };
            let Ok(st) = st.lock() else {
                return vec![];
            };
            fetch_candidates(&st, &reg_for_hinter, line, pos, 0, 2)
        }));
    }

    let state_clone = Arc::clone(&state);
    let on_escape: EscapeHandler = Box::new(move || {
        if let Ok(st) = state_clone.lock() {
            matches!(*st, State::Searched { .. })
        } else {
            false
        }
    });

    let state_for_provider = Arc::clone(&state);
    let default_q = initial_query.unwrap_or(DEFAULT_QUERY).to_string();
    let default_q_for_provider = default_q.clone();
    let last_query_provider: LastQueryProvider = Box::new(move || {
        if let Ok(st) = state_for_provider.lock() {
            if let Some(q) = st.last_query() {
                return q.to_string();
            }
        }
        default_q_for_provider.clone()
    });

    let state_for_searched_provider = Arc::clone(&state);
    let searched_query_provider: SearchedQueryProvider = Box::new(move || {
        state_for_searched_provider
            .lock()
            .ok()
            .and_then(|st| st.last_query().map(str::to_string))
    });

    let state_for_untag_provider = Arc::clone(&state);
    let untag_query_provider: SearchedQueryProvider = Box::new(move || {
        state_for_untag_provider
            .lock()
            .ok()
            .and_then(|st| st.last_untag_query().map(str::to_string))
    });

    let path_for_provider = Arc::clone(&last_path);
    let path_provider: LastQueryProvider = Box::new(move || {
        path_for_provider
            .lock()
            .map(|p| p.clone())
            .unwrap_or_else(|_| ".".to_string())
    });

    let mut edit_mode = InteractiveEditMode::new(Emacs::new(keybindings));
    edit_mode.set_on_escape(on_escape);
    edit_mode.set_hinter(Arc::clone(&hinter));

    let completer = InteractiveCompleter::with_paths(
        fetcher,
        last_query_provider,
        path_provider,
        Arc::clone(&hinter),
    )
    .with_searched_query_provider(searched_query_provider)
    .with_untag_query_provider(untag_query_provider)
    .with_paging_offset(Arc::clone(&shared_offset));
    let columnar_menu = ColumnarMenu::default().with_name("completion_menu");
    let completion_state = hinter
        .lock()
        .map(|h| h.completion_state())
        .unwrap_or_else(|_| Arc::new(Mutex::new(CompletionState::new())));
    let query_base = completer.shared_query_base();
    let paging_menu = Box::new(
        PagingColumnarMenu::new(
            columnar_menu,
            Arc::clone(&shared_offset),
            DEFAULT_INTERACTIVE_COMPLETION_LIMIT,
        )
        .with_completion_state(completion_state)
        .with_query_base(query_base),
    );

    let mut line_editor = Reedline::create()
        .with_quick_completions(true)
        .with_partial_completions(true)
        .with_hinter(Box::new(SharedHinter(hinter.clone())))
        .with_completer(Box::new(completer))
        .with_menu(ReedlineMenu::EngineCompleter(paging_menu))
        .with_history(history)
        .with_edit_mode(Box::new(edit_mode));

    if !quiet && initial_query.is_none() {
        print_menu(&mut std::io::stdout(), &state.lock().unwrap())?;
    }

    if let Some(q) = initial_query {
        if let Ok(mut h) = hinter.lock() {
            h.set_last_query(q);
        }
        match parse_command(&format!("s {q}"), false) {
            Ok(cmd) => {
                let mut prompt_input = TerminalPromptInput::default();
                let mut path_guard = last_path.lock().unwrap();
                if let Err(e) = dispatch_command(
                    cmd,
                    &mut state.lock().unwrap(),
                    store,
                    registry,
                    config,
                    write_options.clone(),
                    &mut path_guard,
                    &mut prompt_input,
                    &mut std::io::stdout(),
                    &mut std::io::stderr(),
                ) {
                    eprintln!("Error: {e}");
                }
            }
            Err(e) => eprintln!("Error: {e}"),
        }
    }

    loop {
        if let Ok(st) = state.lock() {
            sync_hinter_state(
                &hinter,
                &st,
                initial_query.unwrap_or(DEFAULT_QUERY),
            );
        }
        if let Ok(mut h) = hinter.lock() {
            h.reset();
        }
        let prompt_str = state.lock().unwrap().prompt_string();
        let prompt = reedline::DefaultPrompt::new(
            reedline::DefaultPromptSegment::Basic(prompt_str),
            reedline::DefaultPromptSegment::Empty,
        );

        match line_editor.read_line(&prompt) {
            Ok(reedline::Signal::Success(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let is_searched =
                    matches!(*state.lock().unwrap(), State::Searched { .. });
                match parse_command(trimmed, is_searched) {
                    Ok(cmd) => {
                        let mut prompt_input = TerminalPromptInput::default();
                        let dispatch_res = {
                            let mut state_guard = state.lock().unwrap();
                            let mut path_guard = last_path.lock().unwrap();
                            dispatch_command(
                                cmd,
                                &mut state_guard,
                                store,
                                registry,
                                config,
                                write_options.clone(),
                                &mut path_guard,
                                &mut prompt_input,
                                &mut std::io::stdout(),
                                &mut std::io::stderr(),
                            )
                        };
                        let _ = std::io::stdout().flush();
                        match dispatch_res {
                            Ok(cont) => {
                                if let Ok(st) = state.lock() {
                                    sync_hinter_state(&hinter, &st, &default_q);
                                }
                                if let Ok(lp) = last_path.lock() {
                                    if let Ok(mut h) = hinter.lock() {
                                        h.set_last_path(&lp);
                                    }
                                }
                                if !cont {
                                    break;
                                }
                            }
                            Err(e) => {
                                eprintln!("Error: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("{e}");
                    }
                }
            }
            Ok(reedline::Signal::CtrlC) => {
                let was_empty = hinter
                    .lock()
                    .map(|h| h.current_line().trim().is_empty())
                    .unwrap_or(true);
                let is_searched = state
                    .lock()
                    .map(|s| matches!(*s, State::Searched { .. }))
                    .unwrap_or(false);
                if was_empty && is_searched {
                    let mut st = state.lock().unwrap();
                    st.clear();
                    writeln!(std::io::stdout())?;
                    print_menu(&mut std::io::stdout(), &st)?;
                }
                continue;
            }
            Ok(reedline::Signal::CtrlD) => break,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }
    Ok(())
}

pub fn run_interactive_stream<R: Read, W: Write, E: Write>(
    store: &Store,
    registry: &TagRegistry,
    config: &Config,
    write_options: WriteOptions,
    initial_query: Option<&str>,
    input: R,
    output: &mut W,
    err_out: &mut E,
    quiet: bool,
) -> std::io::Result<()> {
    if !quiet {
        writeln!(
            err_out,
            "Warning: Running interactive mode in non-TTY pipe."
        )?;
    }
    let mut state = State::new();
    let mut last_path = ".".to_string();
    let mut reader = BufReader::new(input);

    if !quiet && initial_query.is_none() {
        let _ = print_menu(output, &state);
    }

    if let Some(q) = initial_query {
        match parse_command(&format!("s {q}"), false) {
            Ok(cmd) => {
                let _ = dispatch_command(
                    cmd,
                    &mut state,
                    store,
                    registry,
                    config,
                    write_options.clone(),
                    &mut last_path,
                    &mut reader,
                    output,
                    err_out,
                );
            }
            Err(e) => {
                let _ = writeln!(err_out, "Error: {e}");
            }
        }
    }

    let mut line_buf = String::new();
    loop {
        line_buf.clear();
        if reader.read_line(&mut line_buf)? == 0 {
            break;
        }
        let trimmed = line_buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let is_searched = matches!(state, State::Searched { .. });
        match parse_command(trimmed, is_searched) {
            Ok(cmd) => {
                match dispatch_command(
                    cmd,
                    &mut state,
                    store,
                    registry,
                    config,
                    write_options.clone(),
                    &mut last_path,
                    &mut reader,
                    output,
                    err_out,
                ) {
                    Ok(cont) => {
                        if !cont {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = writeln!(err_out, "Error: {e}");
                    }
                }
            }
            Err(e) => {
                let _ = writeln!(err_out, "{e}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tempfile::TempDir;

    #[test]
    fn test_runner_stream_quit() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        let config = Config::default();
        let write_opts = WriteOptions::default();

        let input = Cursor::new(b"q\n");
        let mut output = Vec::new();
        let mut err_out = Vec::new();

        run_interactive_stream(
            &store,
            &registry,
            &config,
            write_opts,
            None,
            input,
            &mut output,
            &mut err_out,
            true,
        )
        .unwrap();
    }

    #[test]
    fn test_interactive_edit_mode_esc_behavior() {
        let mut edit_mode = InteractiveEditMode::new(Emacs::default());

        assert_eq!(
            edit_mode.handle_reedline_event(ReedlineEvent::Esc),
            ReedlineEvent::Edit(vec![EditCommand::Clear])
        );

        edit_mode.set_on_escape(Box::new(|| true));
        assert_eq!(
            edit_mode.handle_reedline_event(ReedlineEvent::Esc),
            ReedlineEvent::ExecuteHostCommand("q".to_string())
        );

        edit_mode.set_on_escape(Box::new(|| false));
        assert_eq!(
            edit_mode.handle_reedline_event(ReedlineEvent::Esc),
            ReedlineEvent::Edit(vec![EditCommand::Clear])
        );
    }

    #[test]
    fn test_keybindings_right_key_until_found() {
        let kb = build_keybindings();
        let event =
            kb.find_binding(KeyModifiers::NONE, KeyCode::Right).unwrap();
        assert_eq!(
            event,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::HistoryHintComplete,
                ReedlineEvent::MenuRight,
                ReedlineEvent::Right,
            ])
        );
    }

    #[test]
    fn test_edit_mode_accept_keys_preserve_ghost_active_phase() {
        let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
        hinter.lock().unwrap().set_last_query("extension:rs");
        hinter.lock().unwrap().handle_line("s", 1);
        assert_eq!(
            hinter.lock().unwrap().phase(),
            CompletionPhase::GhostActive
        );

        let mut mode = InteractiveEditMode::new(Emacs::default());
        mode.set_hinter(Arc::clone(&hinter));

        let kb = build_keybindings();
        let right_ev =
            kb.find_binding(KeyModifiers::NONE, KeyCode::Right).unwrap();
        let res = mode.handle_reedline_event(right_ev.clone());
        assert_eq!(res, right_ev);
        assert_eq!(
            hinter.lock().unwrap().phase(),
            CompletionPhase::GhostActive
        );

        let ctrl_f = kb
            .find_binding(KeyModifiers::CONTROL, KeyCode::Char('f'))
            .unwrap();
        let res = mode.handle_reedline_event(ctrl_f.clone());
        assert_eq!(res, ctrl_f);
        assert_eq!(
            hinter.lock().unwrap().phase(),
            CompletionPhase::GhostActive
        );

        let left_ev = ReedlineEvent::Left;
        let _ = mode.handle_reedline_event(left_ev);
        assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::Off);
    }
}

#[cfg(test)]
mod runner_state_tests {
    use super::*;
    use crate::autocomplete::CandidateKind;
    use reedline::Completer;

    #[test]
    fn test_interactive_completer_deterministic_generation() {
        let fetcher = Box::new(|line: &str, _pos: usize| {
            if line.starts_with("s ext") {
                vec![Candidate {
                    raw: "extension:".to_string(),
                    kind: CandidateKind::Type,
                    span: (2, 5),
                }]
            } else {
                vec![]
            }
        });
        let mut completer = InteractiveCompleter::new(fetcher, "extension:rs");

        let s1 = completer.complete("s", 1);
        assert_eq!(s1.len(), 1);
        assert_eq!(s1[0].value, " extension:rs");

        let s2 = completer.complete("s ext", 5);
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].value, "extension:");

        let s3 = completer.complete("s", 1);
        assert_eq!(s3.len(), 1);
        assert_eq!(s3[0].value, " extension:rs");
    }

    #[test]
    fn test_interactive_completer_mid_line_does_not_trigger_past_query() {
        let fetcher = Box::new(|_line: &str, _pos: usize| vec![]);
        let mut completer = InteractiveCompleter::new(fetcher, "extension:rs");

        let res = completer.complete("s abc", 1);
        assert!(res.is_empty());
    }

    #[test]
    fn test_interactive_edit_mode_delegates_to_inner_emacs() {
        let mut mode =
            InteractiveEditMode::new(Emacs::new(build_keybindings()));
        let res = mode.handle_reedline_event(ReedlineEvent::Enter);
        assert_eq!(res, ReedlineEvent::Enter);
    }

    #[test]
    fn test_runner_on_escape_triggers_host_command_on_searched() {
        let mut mode =
            InteractiveEditMode::new(Emacs::new(build_keybindings()));
        mode.set_on_escape(Box::new(|| true));
        let res = mode.handle_reedline_event(ReedlineEvent::Esc);
        assert_eq!(res, ReedlineEvent::ExecuteHostCommand("q".to_string()));
    }

    #[test]
    fn test_edit_mode_backspace_clears_ghost_active_phase() {
        use crate::cli::interactive::autocomplete::CompletionPhase;

        let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
        hinter.lock().unwrap().set_last_query("extension:rs");
        hinter.lock().unwrap().handle_line("s", 1);
        assert_eq!(
            hinter.lock().unwrap().phase(),
            CompletionPhase::GhostActive
        );

        let mut mode = InteractiveEditMode::new(Emacs::default());
        mode.set_hinter(Arc::clone(&hinter));
        let ev = mode.handle_reedline_event(ReedlineEvent::Edit(vec![
            EditCommand::Backspace,
        ]));
        assert_eq!(ev, ReedlineEvent::Edit(vec![EditCommand::Backspace]));
        assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::Off);
    }

    #[test]
    fn test_sync_hinter_restores_default_query_when_state_returns_to_init() {
        let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
        assert_eq!(hinter.lock().unwrap().handle_line("s", 1), " *:*");

        hinter.lock().unwrap().set_last_query("extension:rs");
        let mut state = State::new();
        state.to_searched("project:alpha".into(), true, None, 1, None, false);
        sync_hinter_last_query(&hinter, state.last_query(), DEFAULT_QUERY);
        assert_eq!(
            hinter.lock().unwrap().handle_line("s", 1),
            " project:alpha"
        );

        state.clear();
        assert_eq!(state.last_query(), None);
        sync_hinter_last_query(&hinter, state.last_query(), DEFAULT_QUERY);
        assert_eq!(hinter.lock().unwrap().handle_line("s", 1), " *:*");

        state.to_searched("project:alpha".into(), true, None, 1, None, false);
        sync_hinter_last_query(&hinter, state.last_query(), "filename:a.txt");
        state.clear();
        sync_hinter_last_query(&hinter, state.last_query(), "filename:a.txt");
        assert_eq!(
            hinter.lock().unwrap().handle_line("s", 1),
            " filename:a.txt"
        );
    }

    #[test]
    fn test_interactive_completer_page_fetch_uses_stored_base_prefix() {
        use crate::autocomplete::Candidate;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        let offset = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(Vec::<(String, usize)>::new()));
        let seen_clone = Arc::clone(&seen);
        let fetcher = Box::new(move |line: &str, pos: usize| {
            seen_clone.lock().unwrap().push((line.to_string(), pos));
            vec![Candidate {
                raw: "author:".into(),
                kind: CandidateKind::Type,
                span: (2, pos),
            }]
        });
        let mut completer = InteractiveCompleter::new(fetcher, "")
            .with_paging_offset(Arc::clone(&offset));
        let _ = completer.complete("s ", 2);
        offset.store(10, Ordering::SeqCst);
        let _ = completer.complete("s author:", 9);
        let calls = seen.lock().unwrap();
        assert_eq!(calls[0], ("s ".into(), 2));
        assert_eq!(calls[1], ("s ".into(), 2));
    }

    #[test]
    fn test_interactive_completer_untag_uses_untag_query_provider() {
        let fetcher = Box::new(|_line: &str, _pos: usize| vec![]);
        let mut completer = InteractiveCompleter::with_query_provider(
            fetcher,
            Box::new(|| Some("project:".to_string())),
        )
        .with_untag_query_provider(Box::new(|| None));

        assert!(completer.complete("u", 1).is_empty());
        assert_eq!(completer.complete("t", 1)[0].value, " \"project:\" ");
    }

    #[test]
    fn test_interactive_completer_suppresses_edit_query_in_init_state() {
        let fetcher = Box::new(|_line: &str, _pos: usize| vec![]);
        let searched_q = Arc::new(Mutex::new(None));
        let provider_q = Arc::clone(&searched_q);
        let mut completer = InteractiveCompleter::with_query_provider(
            fetcher,
            Box::new(move || provider_q.lock().unwrap().clone()),
        );

        let res_t = completer.complete("t", 1);
        assert!(res_t.is_empty());
        let res_u = completer.complete("u", 1);
        assert!(res_u.is_empty());

        *searched_q.lock().unwrap() = Some("project:alpha".to_string());
        let res_t2 = completer.complete("t", 1);
        assert_eq!(res_t2.len(), 1);
        assert_eq!(res_t2[0].value, " \"project:alpha\" ");
    }

    #[test]
    fn test_sync_hinter_suppresses_edit_ghost_when_state_returns_to_init() {
        let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
        let mut state = State::new();

        sync_hinter_state(&hinter, &state, DEFAULT_QUERY);
        assert_eq!(hinter.lock().unwrap().handle_line("s", 1), " *:*");
        assert_eq!(hinter.lock().unwrap().handle_line("t", 1), "");
        assert_eq!(hinter.lock().unwrap().handle_line("u", 1), "");

        state.to_searched("project:alpha".into(), true, None, 1, None, false);
        sync_hinter_state(&hinter, &state, DEFAULT_QUERY);
        assert_eq!(
            hinter.lock().unwrap().handle_line("t", 1),
            " \"project:alpha\" "
        );
        assert_eq!(
            hinter.lock().unwrap().handle_line("u", 1),
            " \"project:alpha\" "
        );

        state.clear();
        sync_hinter_state(&hinter, &state, DEFAULT_QUERY);
        assert_eq!(hinter.lock().unwrap().handle_line("s", 1), " *:*");
        assert_eq!(hinter.lock().unwrap().handle_line("t", 1), "");
        assert_eq!(hinter.lock().unwrap().handle_line("u", 1), "");
    }

    #[test]
    fn test_edit_mode_enter_on_bare_command_restores_spaced_command() {
        let enter = build_keybindings()
            .find_binding(
                reedline::KeyModifiers::NONE,
                reedline::KeyCode::Enter,
            )
            .unwrap();
        for cmd in ["s", "t", "u", "i"] {
            let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
            hinter
                .lock()
                .unwrap()
                .set_searched_query(Some("extension:md".to_string()));
            hinter
                .lock()
                .unwrap()
                .set_untag_query(Some("project:x".to_string()));
            hinter.lock().unwrap().cancel_trace();
            hinter.lock().unwrap().handle_line(cmd, 1);
            let mut mode =
                InteractiveEditMode::new(Emacs::new(build_keybindings()));
            mode.set_hinter(Arc::clone(&hinter));
            assert_eq!(
                mode.handle_reedline_event(enter.clone()),
                reedline::ReedlineEvent::Edit(vec![
                    reedline::EditCommand::Clear,
                    reedline::EditCommand::InsertString(format!("{cmd} ")),
                ])
            );
            assert!(!hinter
                .lock()
                .unwrap()
                .handle_line(&format!("{cmd} "), 2)
                .is_empty());
        }
    }

    #[test]
    fn test_edit_mode_enter_with_ghost_passes_through() {
        let enter = build_keybindings()
            .find_binding(
                reedline::KeyModifiers::NONE,
                reedline::KeyCode::Enter,
            )
            .unwrap();
        let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
        hinter.lock().unwrap().set_last_query("extension:md");
        hinter.lock().unwrap().handle_line("s", 1);
        let mut mode =
            InteractiveEditMode::new(Emacs::new(build_keybindings()));
        mode.set_hinter(Arc::clone(&hinter));
        assert_eq!(mode.handle_reedline_event(enter.clone()), enter);
    }
}
