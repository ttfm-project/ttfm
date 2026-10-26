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

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::cmp::Ordering;
use std::io::{self, BufRead, Read, Write};

struct RawModeGuard;
impl RawModeGuard {
    fn enable() -> io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn read_raw_line() -> io::Result<Vec<u8>> {
    let _guard = RawModeGuard::enable()?;
    let mut out = io::stdout();
    let mut line = String::new();
    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let before = line.chars().count();
        match feed_key(&mut line, key) {
            KeyOutcome::Pending => {
                match line.chars().count().cmp(&before) {
                    Ordering::Greater => {
                        write!(out, "{}", line.chars().last().unwrap())?
                    }
                    Ordering::Less => write!(out, "\x08 \x08")?,
                    Ordering::Equal => {}
                }
                out.flush()?;
            }
            KeyOutcome::Submit => {
                write!(out, "\r\n")?;
                out.flush()?;
                line.push('\n');
                return Ok(line.into_bytes());
            }
            KeyOutcome::Cancel => {
                write!(out, "^C\r\n")?;
                out.flush()?;
                return Err(io::Error::other(PromptCancelled));
            }
            KeyOutcome::Eof => {
                write!(out, "\r\n")?;
                out.flush()?;
                return Ok(Vec::new());
            }
        }
    }
}

#[derive(Default)]
pub struct TerminalPromptInput {
    pending: Vec<u8>,
    pos: usize,
}

impl Read for TerminalPromptInput {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.fill_buf()?.read(buf)?;
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for TerminalPromptInput {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos >= self.pending.len() {
            self.pending.clear();
            self.pos = 0;
            self.pending = read_raw_line()?;
        }
        Ok(&self.pending[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.pending.len());
    }
}

#[derive(Debug)]
pub struct PromptCancelled;
impl std::fmt::Display for PromptCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "prompt cancelled")
    }
}
impl std::error::Error for PromptCancelled {}

pub fn is_prompt_cancelled(err: &io::Error) -> bool {
    err.get_ref().is_some_and(|e| e.is::<PromptCancelled>())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    Pending,
    Submit,
    Cancel,
    Eof,
}

pub fn feed_key(
    line: &mut String,
    key: crossterm::event::KeyEvent,
) -> KeyOutcome {
    let ctrl = key
        .modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL);
    match key.code {
        crossterm::event::KeyCode::Char('c') if ctrl => KeyOutcome::Cancel,
        crossterm::event::KeyCode::Char('d') if ctrl && line.is_empty() => {
            KeyOutcome::Eof
        }
        crossterm::event::KeyCode::Enter => KeyOutcome::Submit,
        crossterm::event::KeyCode::Backspace => {
            line.pop();
            KeyOutcome::Pending
        }
        crossterm::event::KeyCode::Char(c) if !ctrl => {
            line.push(c);
            KeyOutcome::Pending
        }
        _ => KeyOutcome::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn test_feed_key_builds_line_and_submits() {
        let mut line = String::new();
        assert_eq!(
            feed_key(&mut line, key(KeyCode::Char('y'), KeyModifiers::NONE)),
            KeyOutcome::Pending
        );
        assert_eq!(
            feed_key(&mut line, key(KeyCode::Enter, KeyModifiers::NONE)),
            KeyOutcome::Submit
        );
        assert_eq!(line, "y");
    }

    #[test]
    fn test_feed_key_backspace_removes_last_char() {
        let mut line = "yn".to_string();
        assert_eq!(
            feed_key(&mut line, key(KeyCode::Backspace, KeyModifiers::NONE)),
            KeyOutcome::Pending
        );
        assert_eq!(line, "y");
    }

    #[test]
    fn test_feed_key_ctrl_c_cancels() {
        let mut line = "y".to_string();
        assert_eq!(
            feed_key(&mut line, key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyOutcome::Cancel
        );
    }

    #[test]
    fn test_feed_key_ctrl_d_ends_input_only_on_empty_line() {
        let mut empty = String::new();
        assert_eq!(
            feed_key(
                &mut empty,
                key(KeyCode::Char('d'), KeyModifiers::CONTROL)
            ),
            KeyOutcome::Eof
        );
        let mut typed = "y".to_string();
        assert_eq!(
            feed_key(
                &mut typed,
                key(KeyCode::Char('d'), KeyModifiers::CONTROL)
            ),
            KeyOutcome::Pending
        );
        assert_eq!(typed, "y");
    }

    #[test]
    fn test_is_prompt_cancelled_detects_only_cancellation() {
        assert!(is_prompt_cancelled(&std::io::Error::other(PromptCancelled)));
        assert!(!is_prompt_cancelled(&std::io::Error::other("other")));
    }
}
