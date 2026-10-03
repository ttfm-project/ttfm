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

use reedline::{Hinter, History};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub use crate::autocomplete::*;

pub const DEFAULT_QUERY: &str = "*:*";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompletionPhase {
    #[default]
    Off,
    GhostActive,
    MenuPaging,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompletionState {
    phase: CompletionPhase,
    menu_base_line: String,
    menu_base_pos: usize,
    suppressed: bool,
}

impl CompletionState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self) -> CompletionPhase {
        self.phase
    }

    pub fn is_suppressed(&self) -> bool {
        self.suppressed
    }

    pub fn menu_base(&self) -> Option<(String, usize)> {
        if self.phase == CompletionPhase::MenuPaging {
            Some((self.menu_base_line.clone(), self.menu_base_pos))
        } else {
            None
        }
    }

    pub fn enter_ghost(&mut self) {
        if self.phase == CompletionPhase::MenuPaging {
            return;
        }
        self.phase = CompletionPhase::GhostActive;
    }

    pub fn enter_menu(&mut self, line: String, pos: usize) {
        self.phase = CompletionPhase::MenuPaging;
        self.menu_base_line = line;
        self.menu_base_pos = pos;
        self.suppressed = false;
    }

    pub fn clear_ghost(&mut self) {
        if self.phase == CompletionPhase::GhostActive {
            self.phase = CompletionPhase::Off;
        }
    }

    pub fn set_off(&mut self) {
        self.phase = CompletionPhase::Off;
    }

    pub fn dismiss(&mut self) -> Option<(String, usize)> {
        let restore = self.menu_base();
        self.phase = CompletionPhase::Off;
        self.suppressed = true;
        restore
    }

    pub fn unsuppress(&mut self) {
        self.suppressed = false;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

pub struct InteractiveHinter {
    current_line: String,
    transient_message: Option<(String, Instant)>,
    ghost_slot: Option<String>,
    last_query: String,
    last_path: String,
    last_computed_hint: String,
    completion: Arc<Mutex<CompletionState>>,
    candidate_source:
        Option<Arc<dyn Fn(&str, usize) -> Vec<Candidate> + Send + Sync>>,
}

impl Default for InteractiveHinter {
    fn default() -> Self {
        Self {
            current_line: String::new(),
            transient_message: None,
            ghost_slot: None,
            last_query: DEFAULT_QUERY.to_string(),
            last_path: ".".to_string(),
            last_computed_hint: String::new(),
            completion: Arc::new(Mutex::new(CompletionState::new())),
            candidate_source: None,
        }
    }
}

impl InteractiveHinter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_ghost(&mut self, ghost: Option<String>) {
        self.ghost_slot = ghost;
    }

    pub fn set_last_query(&mut self, query: &str) {
        self.last_query = query.to_string();
    }

    pub fn set_last_path(&mut self, path: &str) {
        self.last_path = path.to_string();
    }

    pub fn completion_state(&self) -> Arc<Mutex<CompletionState>> {
        Arc::clone(&self.completion)
    }

    pub fn set_completion_state(&mut self, state: Arc<Mutex<CompletionState>>) {
        self.completion = state;
    }

    pub fn phase(&self) -> CompletionPhase {
        self.with_state(|s| s.phase())
            .unwrap_or(CompletionPhase::Off)
    }

    pub fn is_tracing(&self) -> bool {
        self.phase() == CompletionPhase::GhostActive
    }

    pub fn is_trace_cancelled(&self) -> bool {
        self.with_state(|s| s.is_suppressed()).unwrap_or(false)
    }

    pub fn set_candidate_source(
        &mut self,
        source: Arc<dyn Fn(&str, usize) -> Vec<Candidate> + Send + Sync>,
    ) {
        self.candidate_source = Some(source);
    }

    pub fn cancel_trace(&mut self) {
        self.with_state_mut(|s| {
            s.dismiss();
        });
        self.last_computed_hint.clear();
        self.ghost_slot = None;
    }

    pub fn clear_active_hint(&mut self) {
        self.with_state_mut(|s| s.set_off());
        self.last_computed_hint.clear();
        self.ghost_slot = None;
    }

    fn with_state<T>(
        &self,
        f: impl FnOnce(&CompletionState) -> T,
    ) -> Option<T> {
        self.completion.lock().ok().map(|s| f(&s))
    }

    fn with_state_mut<T>(
        &self,
        f: impl FnOnce(&mut CompletionState) -> T,
    ) -> Option<T> {
        self.completion.lock().ok().map(|mut s| f(&mut s))
    }

    pub fn set_transient_message(&mut self, msg: &str) {
        self.transient_message = Some((msg.to_string(), Instant::now()));
    }

    pub fn clear_transient_message(&mut self) {
        self.transient_message = None;
    }

    pub fn current_line(&self) -> &str {
        &self.current_line
    }

    pub fn reset(&mut self) {
        self.ghost_slot = None;
        self.transient_message = None;
        self.current_line.clear();
        self.last_computed_hint.clear();
        self.with_state_mut(|s| s.reset());
    }

    pub fn handle_line(&mut self, line: &str, pos: usize) -> String {
        let history = reedline::FileBackedHistory::default();
        self.handle(line, pos, &history, false, "")
    }

    fn quoted_last_path(&self) -> String {
        if self.last_path.contains(' ') && !self.last_path.starts_with('"') {
            format!("\"{}\"", self.last_path)
        } else {
            self.last_path.clone()
        }
    }

    fn unique_suffix_from_source(
        &self,
        line: &str,
        pos: usize,
    ) -> Option<String> {
        let source = self.candidate_source.as_ref()?;
        let cands = source(line, pos);
        unique_ghost_suffix(line, pos, &cands)
    }

    fn hint_last_query_trace(&mut self, trimmed: &str) -> Option<String> {
        if trimmed == "s" {
            if self.last_query.is_empty() {
                self.with_state_mut(|s| s.clear_ghost());
                return Some(String::new());
            }
            self.with_state_mut(|s| s.enter_ghost());
            return Some(format!(" {}", self.last_query));
        }
        let rest = trimmed.strip_prefix("s ")?;
        if !self.is_tracing() {
            return None;
        }
        match self.last_query.strip_prefix(rest) {
            Some(suffix) if suffix.is_empty() => {
                self.with_state_mut(|s| s.clear_ghost());
                Some(String::new())
            }
            Some(suffix) => Some(suffix.to_string()),
            None => None,
        }
    }

    fn hint_index_path_trace(&mut self, trimmed: &str) -> Option<String> {
        let quoted_path = self.quoted_last_path();
        if trimmed == "i" {
            self.with_state_mut(|s| s.enter_ghost());
            return Some(format!(" {quoted_path}"));
        }
        let rest = trimmed.strip_prefix("i ")?;
        if self.is_tracing() {
            return match quoted_path.strip_prefix(rest) {
                Some(suffix) if suffix.is_empty() => {
                    self.with_state_mut(|s| s.clear_ghost());
                    Some(String::new())
                }
                Some(suffix) => Some(suffix.to_string()),
                None => {
                    self.with_state_mut(|s| s.clear_ghost());
                    Some(String::new())
                }
            };
        }
        if rest.is_empty() {
            self.with_state_mut(|s| s.enter_ghost());
            return Some(quoted_path);
        }
        None
    }

    fn hint_edit_query_trace(&mut self, trimmed: &str) -> Option<String> {
        if trimmed != "t" && trimmed != "u" {
            return None;
        }
        if self.last_query.is_empty() {
            self.with_state_mut(|s| s.clear_ghost());
            return Some(String::new());
        }
        self.with_state_mut(|s| s.enter_ghost());
        let escaped = self.last_query.replace('"', "\\\"");
        Some(format!(" \"{escaped}\" "))
    }

    fn compute_hint_suffix(&mut self, line: &str, pos: usize) -> String {
        if pos != line.len() {
            return String::new();
        }
        if self.phase() == CompletionPhase::MenuPaging {
            return String::new();
        }
        let trimmed = line.trim_start();
        if self.is_trace_cancelled() {
            if trimmed == "s "
                || trimmed == "i "
                || trimmed == "t "
                || trimmed == "u "
            {
                self.with_state_mut(|s| s.unsuppress());
            } else {
                return String::new();
            }
        }
        if let Some(hint) = self.hint_last_query_trace(trimmed) {
            return hint;
        }
        if let Some(hint) = self.hint_index_path_trace(trimmed) {
            return hint;
        }
        if let Some(hint) = self.hint_edit_query_trace(trimmed) {
            return hint;
        }
        if self.phase() == CompletionPhase::GhostActive {
            if let Some(suffix) = self.unique_suffix_from_source(line, pos) {
                return suffix;
            }
            self.with_state_mut(|s| s.clear_ghost());
        }
        String::new()
    }
}

impl Hinter for InteractiveHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        _history: &dyn History,
        use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        self.current_line = line.to_string();

        if let Some((msg, instant)) = self.transient_message.as_ref() {
            if instant.elapsed() < std::time::Duration::from_millis(1500) {
                return if use_ansi_coloring {
                    format!("\x1b[2m{msg}\x1b[0m")
                } else {
                    msg.clone()
                };
            } else {
                self.transient_message = None;
            }
        }

        if pos != line.len() {
            return String::new();
        }

        let hint = self.compute_hint_suffix(line, pos);
        self.last_computed_hint = hint.clone();

        if let Some(ref ghost) = self.ghost_slot {
            if !ghost.is_empty() {
                return if use_ansi_coloring {
                    format!("\x1b[2m{ghost}\x1b[0m")
                } else {
                    ghost.clone()
                };
            }
        }

        if hint.is_empty() {
            String::new()
        } else if use_ansi_coloring {
            format!("\x1b[2m{hint}\x1b[0m")
        } else {
            hint
        }
    }

    fn complete_hint(&self) -> String {
        if let Some(ref ghost) = self.ghost_slot {
            if !ghost.is_empty() {
                return ghost.clone();
            }
        }
        self.last_computed_hint.clone()
    }

    fn next_hint_token(&self) -> String {
        if let Some(ref ghost) = self.ghost_slot {
            if !ghost.is_empty() {
                return ghost
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string();
            }
        }
        self.last_computed_hint
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hinter_ansi_coloring_applies_dimmed_style() {
        let mut hinter = InteractiveHinter::new();
        hinter.set_ghost(Some("ext:rs".to_string()));

        let history = reedline::FileBackedHistory::default();
        let uncolored = hinter.handle("s ", 2, &history, false, "");
        assert_eq!(uncolored, "ext:rs");

        let colored = hinter.handle("s ", 2, &history, true, "");
        assert_eq!(colored, "\x1b[2mext:rs\x1b[0m");

        assert_eq!(hinter.complete_hint(), "ext:rs");
    }

    #[test]
    fn test_hinter_tracks_current_line() {
        let mut hinter = InteractiveHinter::new();
        assert_eq!(hinter.current_line(), "");
        hinter.handle_line("s foo", 5);
        assert_eq!(hinter.current_line(), "s foo");
        hinter.reset();
        assert_eq!(hinter.current_line(), "");
    }

    #[test]
    fn test_hinter_transient_message() {
        let mut hinter = InteractiveHinter::new();
        hinter.set_transient_message(" (no matches)");
        let history = reedline::FileBackedHistory::default();
        let colored = hinter.handle("s foo", 5, &history, true, "");
        assert_eq!(colored, "\x1b[2m (no matches)\x1b[0m");

        hinter.clear_transient_message();
        let cleared = hinter.handle("s foo", 5, &history, true, "");
        assert_eq!(cleared, "");
    }

    #[test]
    fn test_hinter_complete_hint_and_next_token() {
        let mut hinter = InteractiveHinter::new();
        hinter.set_ghost(Some("\"ext:rs & fn:bar\"".to_string()));
        assert_eq!(hinter.complete_hint(), "\"ext:rs & fn:bar\"");
        assert_eq!(hinter.next_hint_token(), "\"ext:rs");
    }

    #[test]
    fn test_completion_state_phase_transitions() {
        let mut st = CompletionState::new();
        assert_eq!(st.phase(), CompletionPhase::Off);

        st.enter_ghost();
        assert_eq!(st.phase(), CompletionPhase::GhostActive);

        st.enter_menu("s ".into(), 2);
        assert_eq!(st.phase(), CompletionPhase::MenuPaging);
        assert_eq!(st.menu_base(), Some(("s ".into(), 2)));

        st.enter_ghost();
        assert_eq!(st.phase(), CompletionPhase::MenuPaging);

        let restore = st.dismiss();
        assert_eq!(restore, Some(("s ".into(), 2)));
        assert_eq!(st.phase(), CompletionPhase::Off);
        assert!(st.is_suppressed());
        assert_eq!(st.menu_base(), None);

        st.enter_ghost();
        st.clear_ghost();
        assert_eq!(st.phase(), CompletionPhase::Off);
        st.set_off();
        assert_eq!(st.phase(), CompletionPhase::Off);
    }

    #[test]
    fn test_hinter_off_does_not_query_candidate_source() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let mut hinter = InteractiveHinter::new();
        hinter.set_last_query("extension:rs");
        hinter.set_candidate_source(Arc::new(move |_line, _pos| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            vec![Candidate {
                raw: "extension:".into(),
                kind: CandidateKind::Type,
                span: (2, 5),
            }]
        }));

        assert_eq!(hinter.handle_line("s extra", 7), "");
        assert_eq!(hinter.phase(), CompletionPhase::Off);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        assert_eq!(hinter.handle_line("s ext", 5), "");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_hinter_empty_last_query_shows_no_ghost() {
        let mut hinter = InteractiveHinter::new();
        hinter.set_last_query("extension:rs");
        assert_eq!(hinter.handle_line("s", 1), " extension:rs");
        hinter.set_last_query("");
        assert_eq!(hinter.handle_line("s", 1), "");
        assert_eq!(hinter.handle_line("t", 1), "");
        assert_eq!(hinter.handle_line("u", 1), "");
        assert_ne!(hinter.phase(), CompletionPhase::GhostActive);
    }

    #[test]
    fn test_hinter_ghost_only_at_end_of_line() {
        let mut hinter = InteractiveHinter::new();
        hinter.set_last_query("extension:rs");
        assert_eq!(hinter.handle_line("s", 1), " extension:rs");
        assert_eq!(hinter.phase(), CompletionPhase::GhostActive);
        assert_eq!(hinter.handle_line("s", 0), "");
        assert_eq!(hinter.handle_line("s extension:rs", 5), "");
    }

    #[test]
    fn test_hinter_unique_ghost_only_when_ghost_active() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);
        let mut hinter = InteractiveHinter::new();
        hinter.set_last_query("foo:bar");
        hinter.set_candidate_source(Arc::new(move |line: &str, _pos| {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            if line == "s ext" {
                vec![Candidate {
                    raw: "extension:".into(),
                    kind: CandidateKind::Type,
                    span: (2, 5),
                }]
            } else {
                vec![]
            }
        }));

        assert_eq!(hinter.handle_line("s xyz", 5), "");
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        hinter.completion_state().lock().unwrap().enter_ghost();
        assert_eq!(hinter.handle_line("s ext", 5), "ension:");
        assert!(calls.load(Ordering::SeqCst) >= 1);
        assert_eq!(hinter.phase(), CompletionPhase::GhostActive);
    }
}
