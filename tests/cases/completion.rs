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

use std::path::PathBuf;
use tempfile::TempDir;
use ttfm::{db::Store, indexing::Indexer, tag::TagRegistry};

fn setup_completion_env(
    files: &[&str],
) -> (Store, TagRegistry, TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
    std::fs::create_dir_all(&root).unwrap();
    for name in files {
        let p = root.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, *name).unwrap();
    }
    let registry = TagRegistry::with_standard();
    let store = Store::open(base.join("db")).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run(&[&root], None, false)
        .unwrap();
    (store, registry, dir, root)
}

#[test]
fn test_cli_complete_subcommand_process_execution() -> anyhow::Result<()> {
    let ttfm_bin = env!("CARGO_BIN_EXE_ttfm");
    let dir = tempfile::tempdir()?;
    let ttfm_home = dir.path().join("ttfm_home");
    std::fs::create_dir_all(&ttfm_home)?;

    let files_dir = dir.path().join("files");
    std::fs::create_dir_all(&files_dir)?;
    std::fs::write(files_dir.join("main.rs"), "fn main() {}")?;

    let index_status = std::process::Command::new(ttfm_bin)
        .env("TTFM_HOME", &ttfm_home)
        .args(["index", files_dir.to_str().unwrap()])
        .status()?;
    assert!(index_status.success());

    // 1. CLI complete コマンドで型名補完 (s ext -> extension:)
    let out_ext = std::process::Command::new(ttfm_bin)
        .env("TTFM_HOME", &ttfm_home)
        .args(["complete", "--shell", "bash", "--", "ttfm search ext"])
        .output()?;
    assert!(
        out_ext.status.success(),
        "complete failed: {}",
        String::from_utf8_lossy(&out_ext.stderr)
    );
    let stdout_ext = String::from_utf8(out_ext.stdout)?;
    assert!(stdout_ext.lines().any(|line| line == "extension:"));

    // 2. CLI complete コマンドでタグ値補完 (s extension:r -> rs)
    let out_val = std::process::Command::new(ttfm_bin)
        .env("TTFM_HOME", &ttfm_home)
        .args([
            "complete",
            "--shell",
            "bash",
            "--",
            "ttfm search extension:r",
        ])
        .output()?;
    assert!(
        out_val.status.success(),
        "complete failed: {}",
        String::from_utf8_lossy(&out_val.stderr)
    );
    let stdout_val = String::from_utf8(out_val.stdout)?;
    assert!(stdout_val.lines().any(|line| line == "rs"));

    Ok(())
}

#[test]
fn test_cli_complete_read_only_no_db_side_effects() -> anyhow::Result<()> {
    let ttfm_bin = env!("CARGO_BIN_EXE_ttfm");
    let dir = tempfile::tempdir()?;
    let ttfm_home = dir.path().join("ttfm_home_empty");

    let out = std::process::Command::new(ttfm_bin)
        .env("TTFM_HOME", &ttfm_home)
        .args(["complete", "--shell", "bash", "--", "ttfm search ext"])
        .output()?;
    assert!(out.status.success());
    assert!(!ttfm_home.join("db").exists());

    Ok(())
}

use reedline::{Completer, Emacs, KeyCode, KeyModifiers, ReedlineEvent};
use ttfm::cli::interactive::runner::{
    build_keybindings, InteractiveCompleter, InteractiveEditMode,
};

#[test]
fn test_interactive_completer_past_query_single_candidate_on_s() {
    let fetcher = Box::new(|_line: &str, _pos: usize| vec![]);
    let mut completer = InteractiveCompleter::new(fetcher, "extension:rs");

    let suggestions_s = completer.complete("s", 1);
    assert_eq!(suggestions_s.len(), 1);
    assert_eq!(suggestions_s[0].value, " extension:rs");

    let suggestions_space = completer.complete("s ", 2);
    assert_eq!(suggestions_space.len(), 0);

    let suggestions_mid = completer.complete("s other", 1);
    assert_eq!(suggestions_mid.len(), 0);
}

#[test]
fn test_interactive_completer_backspace_consistency_no_state_divergence() {
    let (store, registry, _dir, _root) = setup_completion_env(&["a.rs", "b.c"]);
    let reg = registry.clone();
    let st = store.try_clone().unwrap();
    let fetcher = Box::new(move |line: &str, pos: usize| {
        ttfm::autocomplete::fetch_candidates(&st, &reg, line, pos, 0, 100)
    });
    let mut completer = InteractiveCompleter::new(fetcher, "extension:rs");

    let initial_s = completer.complete("s", 1);
    assert_eq!(initial_s.len(), 1);
    assert_eq!(initial_s[0].value, " extension:rs");

    let after_space = completer.complete("s ", 2);
    assert!(after_space.len() > 1);

    let after_ext = completer.complete("s ext", 5);
    assert!(after_ext.iter().any(|s| s.value == "extension:"));

    let back_to_space = completer.complete("s ", 2);
    assert_eq!(back_to_space.len(), after_space.len());

    let back_to_s = completer.complete("s", 1);
    assert_eq!(back_to_s.len(), 1);
    assert_eq!(back_to_s[0].value, " extension:rs");
}

#[test]
fn test_quoted_label_completion_preserves_single_quotes_boundary() {
    let (store, registry, _dir, _root) =
        setup_completion_env(&["my document.txt"]);
    let reg = registry.clone();
    let st = store.try_clone().unwrap();
    let fetcher = Box::new(move |line: &str, pos: usize| {
        ttfm::autocomplete::fetch_candidates(&st, &reg, line, pos, 0, 100)
    });
    let mut completer = InteractiveCompleter::new(fetcher, "");

    let suggestions = completer.complete("t \"filename:my", 14);
    assert!(!suggestions.is_empty());
    assert!(!suggestions[0].value.contains("\"\""));
    assert!(suggestions[0].value.ends_with("\" "));
}

#[test]
fn test_edit_query_completion_offers_allowed_types() {
    let (store, registry, _dir, _root) = setup_completion_env(&["test.txt"]);
    ttfm::edit::edit(
        &store,
        &registry,
        "filename:test.txt",
        Some("project:ttfm"),
        ttfm::edit::QueryType::Tag,
        None,
        ttfm::edit::WriteOptions::default(),
        &mut Vec::new(),
    )
    .unwrap();
    let reg = registry.clone();
    let st = store.try_clone().unwrap();
    let fetcher = Box::new(move |line: &str, pos: usize| {
        ttfm::autocomplete::fetch_candidates(&st, &reg, line, pos, 0, 100)
    });
    let mut completer = InteractiveCompleter::new(fetcher, "");

    let suggestions = completer.complete("t \"filename:test.txt\" ", 22);
    assert!(!suggestions.is_empty());
    assert!(suggestions.iter().any(|s| s.value == "project:"));
    assert!(!suggestions.iter().any(|s| s.value == "filename:"));
}

#[test]
fn test_interactive_keybindings_shift_tab_and_backtab_bindings() {
    let kb = build_keybindings();
    let shift_backtab = kb.find_binding(KeyModifiers::SHIFT, KeyCode::BackTab);
    assert_eq!(
        shift_backtab,
        Some(ReedlineEvent::UntilFound(vec![ReedlineEvent::MenuPrevious]))
    );
    let none_backtab = kb.find_binding(KeyModifiers::NONE, KeyCode::BackTab);
    assert_eq!(
        none_backtab,
        Some(ReedlineEvent::UntilFound(vec![ReedlineEvent::MenuPrevious]))
    );
}

#[test]
fn test_interactive_edit_mode_passes_through_menu_events_without_interception()
{
    let mut mode = InteractiveEditMode::new(Emacs::new(build_keybindings()));
    let menu_prev =
        ReedlineEvent::UntilFound(vec![ReedlineEvent::MenuPrevious]);
    let result = mode.handle_reedline_event(menu_prev.clone());
    assert_eq!(result, menu_prev);
}

#[test]
fn test_interactive_multibyte_label_completion_utf8_safety() {
    let fetcher = Box::new(|line: &str, _pos: usize| {
        if line.starts_with("s filename:あ") {
            vec![ttfm::autocomplete::Candidate {
                raw: "あい ".to_string(),
                kind: ttfm::autocomplete::CandidateKind::Label,
                span: (11, 14),
            }]
        } else {
            vec![]
        }
    });
    let mut completer = InteractiveCompleter::new(fetcher, "");
    let suggestions = completer.complete("s filename:あ", 14);
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].value, "あい ");
}

#[test]
fn test_bash_completion_script_has_nospace() {
    let script_path =
        concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/completion/ttfm.bash");
    let content = std::fs::read_to_string(script_path)
        .expect("scripts/completion/ttfm.bash must exist");
    assert!(
        content.contains("-o nospace"),
        "scripts/completion/ttfm.bash must configure '-o nospace' to prevent unwanted trailing space on type completion"
    );
}

#[test]
fn test_completion_subcommand_prints_embedded_scripts() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("absent");
    for (shell, file) in [
        ("bash", "ttfm.bash"),
        ("zsh", "_ttfm"),
        ("fish", "ttfm.fish"),
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_ttfm"))
            .env("TTFM_HOME", &home)
            .args(["completion", shell])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "ttfm completion {shell} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let expected = std::fs::read(format!(
            "{}/scripts/completion/{file}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        assert_eq!(out.stdout, expected, "script mismatch for {shell}");
        assert!(!home.exists());
    }
}

#[test]
fn test_dispatch_command_scoped_guard_prevents_deadlock() {
    use std::sync::{Arc, Mutex};
    use ttfm::cli::interactive::state::State;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("files");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("test.txt"), "hello").unwrap();

    let registry = TagRegistry::with_standard();
    let store = Store::open(dir.path().join("db")).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run(&[&root], None, false)
        .unwrap();

    let state = Arc::new(Mutex::new(State::new()));
    let last_path = Arc::new(Mutex::new(".".to_string()));
    let config = ttfm::config::Config::load();
    let write_options = ttfm::edit::WriteOptions::default();

    let (tx, rx) = std::sync::mpsc::channel();
    let state_clone = Arc::clone(&state);
    let last_path_clone = Arc::clone(&last_path);
    let store_clone = store.try_clone().unwrap();
    let reg_clone = registry.clone();

    std::thread::spawn(move || {
        let mut stdin = std::io::empty();
        let mut stdout = std::io::sink();
        let mut stderr = std::io::sink();
        let cmd = ttfm::cli::interactive::command::Command::Search(
            "extension:txt".to_string(),
        );
        {
            let mut st = state_clone.lock().unwrap();
            let mut lp = last_path_clone.lock().unwrap();
            let _ = ttfm::cli::interactive::command::dispatch_command(
                cmd,
                &mut st,
                &store_clone,
                &reg_clone,
                &config,
                write_options,
                &mut lp,
                &mut stdin,
                &mut stdout,
                &mut stderr,
            );
        }
        let lock_res = state_clone.lock();
        assert!(lock_res.is_ok());
        tx.send(()).unwrap();
    });
    rx.recv_timeout(std::time::Duration::from_millis(1500))
        .expect("state lock must not deadlock during command dispatch");
}

#[test]
fn test_hinter_ghost_trace_state_machine_maintains_and_suppresses() {
    use ttfm::cli::interactive::autocomplete::InteractiveHinter;

    let mut hinter = InteractiveHinter::new();
    hinter.set_last_query("extension:rs");
    assert_eq!(hinter.handle_line("s", 1), " extension:rs");
    assert_eq!(hinter.handle_line("s ", 2), "extension:rs");
    assert_eq!(hinter.handle_line("s ext", 5), "ension:rs");
    assert_eq!(hinter.handle_line("s extra", 7), "");
    assert_eq!(hinter.handle_line("s ext", 5), "");
    assert_eq!(hinter.handle_line("s ", 2), "");
    assert_eq!(hinter.handle_line("s", 1), " extension:rs");
}

#[test]
fn test_hinter_esc_cancels_ghost_trace_while_preserving_line() {
    use std::sync::{Arc, Mutex};
    use ttfm::cli::interactive::autocomplete::InteractiveHinter;
    use ttfm::cli::interactive::runner::InteractiveEditMode;

    let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
    hinter.lock().unwrap().set_last_query("extension:rs");
    hinter.lock().unwrap().set_ghost(Some("ension:".into()));
    let _ = hinter.lock().unwrap().handle_line("s", 1);
    assert!(hinter.lock().unwrap().is_tracing());

    let mut edit_mode = InteractiveEditMode::new(Emacs::default());
    let hinter_clone = Arc::clone(&hinter);
    edit_mode.set_hinter(hinter_clone);

    let ev = edit_mode.handle_reedline_event(ReedlineEvent::Esc);
    assert_eq!(ev, ReedlineEvent::Repaint);
    assert!(!hinter.lock().unwrap().is_tracing());
    assert_eq!(hinter.lock().unwrap().handle_line("s", 1), "");
    assert_eq!(hinter.lock().unwrap().handle_line("s ext", 5), "");
}

#[test]
fn test_hinter_index_command_path_ghost_trace_and_sync() {
    use ttfm::cli::interactive::autocomplete::InteractiveHinter;

    let mut hinter = InteractiveHinter::new();
    assert_eq!(hinter.handle_line("i", 1), " .");
    assert_eq!(hinter.handle_line("i ", 2), ".");

    hinter.set_last_path("my/project");
    assert_eq!(hinter.handle_line("i", 1), " my/project");
    assert_eq!(hinter.handle_line("i ", 2), "my/project");
    assert_eq!(hinter.handle_line("i my/", 5), "project");
    assert_eq!(hinter.handle_line("i other", 7), "");
    assert_eq!(hinter.handle_line("i my/", 5), "");
    assert_eq!(hinter.handle_line("i ", 2), "my/project");
}

#[test]
fn test_interactive_completer_index_command_single_tab_candidate() {
    use ttfm::cli::interactive::autocomplete::InteractiveHinter;

    let fetcher = Box::new(|_line: &str, _pos: usize| vec![]);
    let hinter =
        std::sync::Arc::new(std::sync::Mutex::new(InteractiveHinter::new()));
    let mut completer = InteractiveCompleter::with_paths(
        fetcher,
        Box::new(|| "".to_string()),
        Box::new(|| "src".to_string()),
        std::sync::Arc::clone(&hinter),
    );

    let suggestions_i = completer.complete("i", 1);
    assert_eq!(suggestions_i.len(), 1);
    assert_eq!(suggestions_i[0].value, " src");

    let suggestions_i_space = completer.complete("i ", 2);
    assert_eq!(suggestions_i_space.len(), 1);
    assert_eq!(suggestions_i_space[0].value, "src");
}

#[test]
fn test_paging_columnar_menu_cycles_pages_and_shift_tab_stops_at_bottom() {
    use reedline::{Menu, MenuBuilder, MenuEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use ttfm::cli::interactive::runner::PagingColumnarMenu;

    let (store, registry, _dir, _root) = setup_completion_env(&[
        "f01.txt", "f02.txt", "f03.txt", "f04.txt", "f05.txt", "f06.txt",
        "f07.txt", "f08.txt", "f09.txt", "f10.txt", "f11.txt", "f12.txt",
    ]);
    let offset = Arc::new(AtomicUsize::new(0));
    let offset_clone = Arc::clone(&offset);
    let reg = registry.clone();
    let st = store.try_clone().unwrap();
    let fetcher = Box::new(move |line: &str, pos: usize| {
        let cur_off = offset_clone.load(Ordering::SeqCst);
        ttfm::autocomplete::fetch_candidates(&st, &reg, line, pos, cur_off, 10)
    });
    let mut completer = InteractiveCompleter::new(fetcher, "");
    let mut paging_menu = PagingColumnarMenu::new(
        reedline::ColumnarMenu::default().with_name("completion_menu"),
        Arc::clone(&offset),
        10,
    );
    let mut editor = reedline::Editor::default();
    editor.edit_buffer(
        |b| b.set_buffer("s filename:f".to_string()),
        reedline::UndoBehavior::CreateUndoPoint,
    );

    paging_menu.menu_event(MenuEvent::Activate(false));
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(offset.load(Ordering::SeqCst), 0);
    assert_eq!(paging_menu.get_values().len(), 10);

    for _ in 0..10 {
        paging_menu.menu_event(MenuEvent::NextElement);
    }
    assert_eq!(offset.load(Ordering::SeqCst), 10);

    paging_menu.menu_event(MenuEvent::Edit(false));
    assert_eq!(offset.load(Ordering::SeqCst), 0);

    paging_menu.menu_event(MenuEvent::Activate(false));
    paging_menu.update_values(&mut editor, &mut completer);
    paging_menu.menu_event(MenuEvent::PreviousElement);
    assert_eq!(offset.load(Ordering::SeqCst), 0);
}

#[test]
fn test_paging_columnar_menu_shift_tab_pages_backward_when_not_on_page_zero() {
    use reedline::{Menu, MenuBuilder, MenuEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use ttfm::autocomplete::{Candidate, CandidateKind};
    use ttfm::cli::interactive::runner::PagingColumnarMenu;

    let offset = Arc::new(AtomicUsize::new(0));
    let offset_clone = Arc::clone(&offset);
    let fetcher = Box::new(move |_line: &str, pos: usize| {
        let cur_off = offset_clone.load(Ordering::SeqCst);
        (cur_off..cur_off + 10)
            .map(|i| Candidate {
                raw: format!("type{i}:"),
                kind: CandidateKind::Type,
                span: (2, pos),
            })
            .collect()
    });
    let mut completer = InteractiveCompleter::new(fetcher, "")
        .with_paging_offset(Arc::clone(&offset));
    let mut paging_menu = PagingColumnarMenu::new(
        reedline::ColumnarMenu::default().with_name("completion_menu"),
        Arc::clone(&offset),
        10,
    );
    let mut editor = reedline::Editor::default();
    editor.edit_buffer(
        |b| b.set_buffer("s ".to_string()),
        reedline::UndoBehavior::CreateUndoPoint,
    );

    paging_menu.menu_event(MenuEvent::Activate(false));
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(paging_menu.get_values()[0].value, "type0:");

    for _ in 0..10 {
        paging_menu.menu_event(MenuEvent::NextElement);
    }
    assert_eq!(offset.load(Ordering::SeqCst), 10);
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(paging_menu.get_values()[0].value, "type10:");

    for _ in 0..10 {
        paging_menu.menu_event(MenuEvent::NextElement);
    }
    assert_eq!(offset.load(Ordering::SeqCst), 20);
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(paging_menu.get_values()[0].value, "type20:");

    paging_menu.menu_event(MenuEvent::NextElement);
    paging_menu.menu_event(MenuEvent::PreviousElement);
    assert_eq!(offset.load(Ordering::SeqCst), 20);

    editor.edit_buffer(
        |b| {
            b.set_buffer("s type20:".to_string());
            b.set_insertion_point(9);
        },
        reedline::UndoBehavior::CreateUndoPoint,
    );
    paging_menu.menu_event(MenuEvent::PreviousElement);
    assert_eq!(offset.load(Ordering::SeqCst), 10);
    paging_menu.menu_event(MenuEvent::PreviousElement);
    assert_eq!(offset.load(Ordering::SeqCst), 10);

    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(editor.get_buffer(), "s ");
    assert_eq!(paging_menu.get_values()[0].value, "type10:");
    assert_eq!(paging_menu.get_values()[9].value, "type19:");

    for _ in 0..9 {
        paging_menu.menu_event(MenuEvent::PreviousElement);
        assert_eq!(offset.load(Ordering::SeqCst), 10);
    }
    paging_menu.menu_event(MenuEvent::PreviousElement);
    assert_eq!(offset.load(Ordering::SeqCst), 0);
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(editor.get_buffer(), "s ");
    assert_eq!(paging_menu.get_values()[0].value, "type0:");
    assert_eq!(paging_menu.get_values()[9].value, "type9:");
}

#[test]
fn test_paging_columnar_menu_keeps_base_prefix_across_page_boundary() {
    use reedline::{Menu, MenuBuilder, MenuEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use ttfm::autocomplete::{Candidate, CandidateKind};
    use ttfm::cli::interactive::runner::PagingColumnarMenu;

    let seen = Arc::new(Mutex::new(Vec::<(String, usize, usize)>::new()));
    let offset = Arc::new(AtomicUsize::new(0));
    let offset_clone = Arc::clone(&offset);
    let seen_clone = Arc::clone(&seen);
    let fetcher = Box::new(move |line: &str, pos: usize| {
        let cur_off = offset_clone.load(Ordering::SeqCst);
        seen_clone
            .lock()
            .unwrap()
            .push((line.to_string(), pos, cur_off));
        (cur_off..cur_off + 10)
            .map(|i| Candidate {
                raw: format!("type{i}:"),
                kind: CandidateKind::Type,
                span: (2, pos),
            })
            .collect()
    });
    let mut completer = InteractiveCompleter::new(fetcher, "")
        .with_paging_offset(Arc::clone(&offset));
    let mut paging_menu = PagingColumnarMenu::new(
        reedline::ColumnarMenu::default().with_name("completion_menu"),
        Arc::clone(&offset),
        10,
    );
    let mut editor = reedline::Editor::default();
    editor.edit_buffer(
        |b| b.set_buffer("s ".to_string()),
        reedline::UndoBehavior::CreateUndoPoint,
    );

    paging_menu.menu_event(MenuEvent::Activate(false));
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(paging_menu.get_values()[0].value, "type0:");

    editor.edit_buffer(
        |b| {
            b.set_buffer("s type0:".to_string());
            b.set_insertion_point(8);
        },
        reedline::UndoBehavior::CreateUndoPoint,
    );

    for _ in 0..10 {
        paging_menu.menu_event(MenuEvent::NextElement);
    }
    assert_eq!(offset.load(Ordering::SeqCst), 10);

    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(editor.get_buffer(), "s ");
    assert_eq!(paging_menu.get_values()[0].value, "type10:");
    let calls = seen.lock().unwrap();
    assert!(calls.iter().any(|(l, _, off)| l == "s " && *off == 10));
    assert!(!calls
        .iter()
        .any(|(l, _, off)| { l.contains("type0:") && *off == 10 }));
}

#[test]
fn test_paging_columnar_menu_drops_tab_while_page_fetch_in_flight() {
    use reedline::{Menu, MenuBuilder, MenuEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use ttfm::autocomplete::{Candidate, CandidateKind};
    use ttfm::cli::interactive::runner::PagingColumnarMenu;

    let offset = Arc::new(AtomicUsize::new(0));
    let offset_clone = Arc::clone(&offset);
    let fetcher = Box::new(move |_line: &str, pos: usize| {
        let cur_off = offset_clone.load(Ordering::SeqCst);
        (cur_off..cur_off + 10)
            .map(|i| Candidate {
                raw: format!("type{i}:"),
                kind: CandidateKind::Type,
                span: (2, pos),
            })
            .collect()
    });
    let mut completer = InteractiveCompleter::new(fetcher, "")
        .with_paging_offset(Arc::clone(&offset));
    let mut paging_menu = PagingColumnarMenu::new(
        reedline::ColumnarMenu::default().with_name("completion_menu"),
        Arc::clone(&offset),
        10,
    );
    let mut editor = reedline::Editor::default();
    editor.edit_buffer(
        |b| b.set_buffer("s ".to_string()),
        reedline::UndoBehavior::CreateUndoPoint,
    );

    paging_menu.menu_event(MenuEvent::Activate(false));
    paging_menu.update_values(&mut editor, &mut completer);
    for _ in 0..10 {
        paging_menu.menu_event(MenuEvent::NextElement);
    }
    assert_eq!(offset.load(Ordering::SeqCst), 10);
    for _ in 0..30 {
        paging_menu.menu_event(MenuEvent::NextElement);
    }
    assert_eq!(offset.load(Ordering::SeqCst), 10);
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(offset.load(Ordering::SeqCst), 10);
    paging_menu.menu_event(MenuEvent::NextElement);
    assert_eq!(offset.load(Ordering::SeqCst), 10);
}

#[test]
fn test_autocomplete_composite_directory_routes_to_dir_filenames() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("files");
    std::fs::create_dir_all(root.join("sub_directory")).unwrap();
    std::fs::write(root.join("regular_file.txt"), "hello").unwrap();

    let registry = TagRegistry::with_standard();
    let store = Store::open(dir.path().join("db")).unwrap();
    Indexer::new(&store, &registry).initialize_tables().unwrap();
    Indexer::new(&store, &registry)
        .run(&[&root], None, false)
        .unwrap();

    let candidates = ttfm::autocomplete::fetch_tag_candidates(
        &store,
        &registry,
        "directory",
        "",
        0,
        10,
    )
    .unwrap();
    assert!(candidates.iter().any(|c| c == "sub_directory"));
    assert!(!candidates.iter().any(|c| c == "regular_file.txt"));
}

#[test]
fn test_autocomplete_name_falls_back_to_filename_without_user_name() {
    let (store, registry, _dir, _root) =
        setup_completion_env(&["plain_name_target.rs"]);

    let names = ttfm::autocomplete::fetch_tag_candidates(
        &store, &registry, "name", "plain_", 0, 10,
    )
    .unwrap();
    assert!(
        names.iter().any(|n| n == "plain_name_target.rs"),
        "{names:?}"
    );

    let mut sink = Vec::new();
    let resp = ttfm::search::search_short(
        &store,
        &registry,
        "tag:\"name:plain_*\"",
        ttfm::search::SearchOptions::default(),
        &mut sink,
    )
    .unwrap();
    let tags: Vec<String> = resp
        .results
        .into_iter()
        .filter_map(|item| item.representative.tags.first().map(|t| t.as_str()))
        .collect();
    assert!(
        tags.iter().any(|t| t == "name:plain_name_target.rs"),
        "{tags:?}"
    );
}

#[test]
fn test_autocomplete_metadata_axes_and_enum_suggestions() {
    let (store, registry, _dir, _root) = setup_completion_env(&["main.rs"]);

    let types = ttfm::autocomplete::fetch_tag_candidates(
        &store, &registry, "type", "", 0, 50,
    )
    .unwrap();
    assert!(types.iter().any(|t| t == "extension"));
    assert!(types.iter().any(|t| t == "size"));

    let kinds = ttfm::autocomplete::fetch_tag_candidates(
        &store,
        &registry,
        "item_kind",
        "",
        0,
        10,
    )
    .unwrap();
    assert!(kinds.iter().any(|k| k == "file"));
    assert!(kinds.iter().any(|k| k == "tag"));

    let origins = ttfm::autocomplete::fetch_tag_candidates(
        &store, &registry, "origin", "", 0, 10,
    )
    .unwrap();
    assert!(origins.iter().any(|o| o == "file"));
    assert!(origins.iter().any(|o| o == "user"));
}

#[test]
fn test_interactive_edit_mode_esc_clears_buffer_preserving_searched_context() {
    use reedline::{EditCommand, Emacs, ReedlineEvent};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use ttfm::cli::interactive::runner::InteractiveEditMode;

    let mut edit_mode = InteractiveEditMode::new(Emacs::default());
    let on_escape_called = Arc::new(AtomicBool::new(false));
    let on_escape_clone = Arc::clone(&on_escape_called);
    edit_mode.set_on_escape(Box::new(move || {
        on_escape_clone.store(true, Ordering::SeqCst);
        true
    }));

    edit_mode.notify_buffer_length(5);
    let ev = edit_mode.handle_reedline_event(ReedlineEvent::Esc);
    assert_eq!(ev, ReedlineEvent::Edit(vec![EditCommand::Clear]));
    assert!(!on_escape_called.load(Ordering::SeqCst));

    edit_mode.notify_buffer_length(0);
    let ev_empty = edit_mode.handle_reedline_event(ReedlineEvent::Esc);
    assert_eq!(ev_empty, ReedlineEvent::ExecuteHostCommand("q".to_string()));
    assert!(on_escape_called.load(Ordering::SeqCst));
}

#[test]
fn test_hinter_quoted_path_with_spaces() {
    use ttfm::cli::interactive::autocomplete::InteractiveHinter;

    let mut hinter = InteractiveHinter::new();
    hinter.set_last_path("/my dir/photos");

    assert_eq!(hinter.handle_line("i", 1), " \"/my dir/photos\"");
    assert_eq!(hinter.handle_line("i ", 2), "\"/my dir/photos\"");
    assert_eq!(hinter.handle_line("i \"/my", 6), " dir/photos\"");
}

#[test]
fn test_interactive_completer_past_query_on_t_and_u() {
    use reedline::Completer;
    use std::sync::{Arc, Mutex};
    use ttfm::cli::interactive::autocomplete::InteractiveHinter;
    use ttfm::cli::interactive::runner::InteractiveCompleter;

    let fetcher = Box::new(|_line: &str, _pos: usize| vec![]);
    let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
    let mut completer = InteractiveCompleter::with_paths(
        fetcher,
        Box::new(|| "extension:rs".to_string()),
        Box::new(|| ".".to_string()),
        Arc::clone(&hinter),
    );

    let suggestions_t = completer.complete("t", 1);
    assert_eq!(suggestions_t.len(), 1);
    assert_eq!(suggestions_t[0].value, " \"extension:rs\" ");

    let suggestions_u = completer.complete("u", 1);
    assert_eq!(suggestions_u.len(), 1);
    assert_eq!(suggestions_u[0].value, " \"extension:rs\" ");
}

#[test]
fn test_interactive_completer_suppresses_quick_complete_after_esc_cancel() {
    use reedline::Completer;
    use std::sync::{Arc, Mutex};
    use ttfm::cli::interactive::autocomplete::InteractiveHinter;
    use ttfm::cli::interactive::runner::InteractiveCompleter;

    let fetcher = Box::new(|_line: &str, _pos: usize| vec![]);
    let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
    hinter.lock().unwrap().set_last_query("extension:rs");
    hinter.lock().unwrap().cancel_trace();

    let mut completer = InteractiveCompleter::with_paths(
        fetcher,
        Box::new(|| "extension:rs".to_string()),
        Box::new(|| ".".to_string()),
        Arc::clone(&hinter),
    );

    let suggestions = completer.complete("s", 1);
    assert!(suggestions.is_empty());
}

#[test]
fn test_autocomplete_nest_operator_term_finished() {
    let ctx = ttfm::autocomplete::parse_completion_context(
        "s path: & project:ttfm ",
        23,
    );
    let ops = ttfm::autocomplete::fetch_operator_candidates(&ctx);
    assert!(ops.contains(&"&:".to_string()));
    assert!(ops.contains(&"&".to_string()));
}

#[test]
fn test_autocomplete_paren_expression_start() {
    let ctx = ttfm::autocomplete::parse_completion_context("s (ext", 6);
    assert!(
        matches!(ctx, ttfm::autocomplete::CompletionContext::ExpressionStart { prefix, .. } if prefix == "ext")
    );
}

#[test]
fn test_autocomplete_inside_aggregate_function() {
    let (store, registry, _dir, _root) = setup_completion_env(&["main.rs"]);
    let open = "s sum(";
    let repl = ttfm::autocomplete::fetch_candidates(
        &store,
        &registry,
        open,
        open.len(),
        0,
        50,
    );
    assert!(!repl.is_empty(), "{repl:?}");
    assert!(
        repl.iter()
            .any(|c| c.kind == ttfm::autocomplete::CandidateKind::Type),
        "{repl:?}"
    );
    assert!(
        repl.iter()
            .any(|c| c.kind == ttfm::autocomplete::CandidateKind::Function),
        "{repl:?}"
    );
    assert!(repl.iter().all(|c| c.raw != "random("), "{repl:?}");

    let closed = "s sum()";
    let pos = closed.find(')').unwrap();
    let inside = ttfm::autocomplete::fetch_candidates(
        &store, &registry, closed, pos, 0, 50,
    );
    assert!(!inside.is_empty(), "{inside:?}");
    assert!(
        inside
            .iter()
            .any(|c| c.kind == ttfm::autocomplete::CandidateKind::Type),
        "{inside:?}"
    );
}

#[test]
fn test_index_path_completion_quotes_spaced_paths() {
    let (store, registry, dir, _root) = setup_completion_env(&[]);
    let root = dir.path().canonicalize().unwrap().join("paths");
    std::fs::create_dir_all(root.join("my dir")).unwrap();
    std::fs::write(root.join("my dir/inner.txt"), "a").unwrap();
    std::fs::write(root.join("foo bar.txt"), "a").unwrap();
    std::fs::write(root.join("baz.txt"), "a").unwrap();
    std::fs::write(root.join("q\"x.txt"), "a").unwrap();
    let r = root.display();

    let line = format!("i {r}/");
    let raws: Vec<String> = ttfm::autocomplete::fetch_candidates(
        &store,
        &registry,
        &line,
        line.len(),
        0,
        50,
    )
    .into_iter()
    .map(|c| c.raw)
    .collect();
    assert!(raws.contains(&format!("\"{r}/foo bar.txt\" ")));
    assert!(raws.contains(&format!("{r}/baz.txt ")));
    assert!(raws.contains(&format!("\"{r}/my dir/")));
    assert!(!raws.iter().any(|p| p.contains("q\"x")));

    let line = format!("i \"{r}/my dir/in");
    let cands = ttfm::autocomplete::fetch_candidates(
        &store,
        &registry,
        &line,
        line.len(),
        0,
        50,
    );
    assert_eq!(cands.len(), 1);
    assert_eq!(cands[0].raw, format!("\"{r}/my dir/inner.txt\" "));
    assert_eq!(cands[0].span, (2, line.len()));

    let completed = format!("{}{}", &line[..cands[0].span.0], cands[0].raw);
    assert_eq!(
        ttfm::cli::interactive::command::parse_paths(&completed[2..]),
        vec![root.join("my dir/inner.txt")]
    );
}

#[test]
fn test_split_path_words_tracks_quotes_and_offsets() {
    let words = ttfm::util::split_path_words("a \"b c\"d \"\" \"e f");
    let got: Vec<(usize, &str)> =
        words.iter().map(|w| (w.start, w.text.as_str())).collect();
    assert_eq!(got, [(0, "a"), (2, "b cd"), (9, ""), (12, "e f")]);
}

#[test]
fn test_hinter_ghost_unique_type_label_function_suffixes() {
    use std::sync::Arc;
    use ttfm::autocomplete::{Candidate, CandidateKind};
    use ttfm::cli::interactive::autocomplete::InteractiveHinter;

    let mut hinter = InteractiveHinter::new();
    hinter.set_last_query("extension:rs");
    hinter.set_last_query("foo:bar");
    hinter.set_candidate_source(Arc::new(|line: &str, _pos| {
        if line == "s ext" {
            vec![Candidate {
                raw: "extension:".into(),
                kind: CandidateKind::Type,
                span: (2, 5),
            }]
        } else if line == "s extension:r" {
            vec![Candidate {
                raw: "rs ".into(),
                kind: CandidateKind::Label,
                span: (12, 13),
            }]
        } else if line == "s cou" {
            vec![Candidate {
                raw: "count(".into(),
                kind: CandidateKind::Function,
                span: (2, 5),
            }]
        } else {
            vec![]
        }
    }));
    assert_eq!(hinter.handle_line("s", 1), " foo:bar");
    assert_eq!(hinter.handle_line("s ", 2), "foo:bar");
    assert_eq!(hinter.handle_line("s xyz", 5), "");
    hinter.completion_state().lock().unwrap().enter_ghost();
    assert_eq!(hinter.handle_line("s ext", 5), "ension:");
    hinter.completion_state().lock().unwrap().enter_ghost();
    assert_eq!(hinter.handle_line("s extension:r", 13), "s ");
    hinter.completion_state().lock().unwrap().enter_ghost();
    assert_eq!(hinter.handle_line("s cou", 5), "nt(");
}

#[test]
fn test_hinter_ghost_store_unique_candidates() {
    use std::sync::Arc;
    use ttfm::autocomplete::unique_ghost_suffix;
    use ttfm::cli::interactive::autocomplete::InteractiveHinter;

    let (store, registry, _dir, _root) = setup_completion_env(&["a.rs", "b.c"]);
    let st = std::sync::Mutex::new(store.try_clone().unwrap());
    let reg = registry.clone();
    let mut hinter = InteractiveHinter::new();
    hinter.set_candidate_source(Arc::new(move |line, pos| {
        let st = st.lock().unwrap();
        ttfm::autocomplete::fetch_candidates(&st, &reg, line, pos, 0, 2)
    }));
    let ext = ttfm::autocomplete::fetch_candidates(
        &store, &registry, "s ext", 5, 0, 2,
    );
    assert_eq!(ext.len(), 1);
    assert_eq!(ext[0].raw, "extension:");
    assert_eq!(hinter.handle_line("s ext", 5), "");
    hinter.completion_state().lock().unwrap().enter_ghost();
    assert_eq!(hinter.handle_line("s ext", 5), "ension:");
    let lab = ttfm::autocomplete::fetch_candidates(
        &store,
        &registry,
        "s extension:r",
        13,
        0,
        2,
    );
    assert_eq!(lab.len(), 1);
    assert_eq!(lab[0].raw.trim(), "rs");
    hinter.completion_state().lock().unwrap().enter_ghost();
    assert_eq!(
        hinter.handle_line("s extension:r", 13),
        unique_ghost_suffix("s extension:r", 13, &lab).unwrap()
    );
    let cou = ttfm::autocomplete::fetch_candidates(
        &store, &registry, "s cou", 5, 0, 2,
    );
    assert_eq!(cou.len(), 1);
    assert_eq!(cou[0].raw, "count(");
    hinter.completion_state().lock().unwrap().enter_ghost();
    assert_eq!(hinter.handle_line("s cou", 5), "nt(");
    let space =
        ttfm::autocomplete::fetch_candidates(&store, &registry, "s ", 2, 0, 2);
    assert!(space.len() > 1);
    hinter.clear_active_hint();
    assert_eq!(hinter.handle_line("s ", 2), "");
}

#[test]
fn test_path_label_span_matches_other_labels() {
    let (store, registry, _dir, root) =
        setup_completion_env(&["other/alpha.rs", "other/beta.rs"]);
    let prefix = format!("{}/other/al", root.display());
    let line = format!("s path:{prefix}");
    let cands = ttfm::autocomplete::fetch_candidates(
        &store,
        &registry,
        &line,
        line.len(),
        0,
        10,
    );
    assert_eq!(cands.len(), 1);
    assert!(cands[0].raw.contains("alpha.rs"));
    assert_eq!(cands[0].span, (7, line.len()));
    let ext = ttfm::autocomplete::parse_completion_context("s extension:r", 13);
    let path_ctx =
        ttfm::autocomplete::parse_completion_context(&line, line.len());
    assert!(matches!(
        (ext, path_ctx),
        (
            ttfm::autocomplete::CompletionContext::TypeColon { .. },
            ttfm::autocomplete::CompletionContext::TypeColon { .. }
        )
    ));
}

fn apply_edit_mode_esc(
    ev: ReedlineEvent,
    editor: &mut reedline::Editor,
    menu: &mut ttfm::cli::interactive::runner::PagingColumnarMenu,
) {
    use reedline::{EditCommand, Menu, MenuEvent, UndoBehavior};
    let events = match ev {
        ReedlineEvent::Multiple(evs) => evs,
        other => vec![other],
    };
    for e in events {
        match e {
            ReedlineEvent::Esc => {
                menu.menu_event(MenuEvent::Deactivate);
            }
            ReedlineEvent::Edit(cmds) => {
                for cmd in cmds {
                    match cmd {
                        EditCommand::Clear => {
                            editor.edit_buffer(
                                |b| b.clear(),
                                UndoBehavior::CreateUndoPoint,
                            );
                        }
                        EditCommand::InsertString(s) => {
                            editor.edit_buffer(
                                |b| b.insert_str(&s),
                                UndoBehavior::CreateUndoPoint,
                            );
                        }
                        EditCommand::MoveToPosition { position, .. } => {
                            editor.edit_buffer(
                                |b| b.set_insertion_point(position),
                                UndoBehavior::CreateUndoPoint,
                            );
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

#[test]
fn test_esc_while_menu_paging_dismisses_menu_and_preserves_query() {
    use reedline::{EditCommand, Menu, MenuBuilder, MenuEvent, UndoBehavior};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use ttfm::autocomplete::{Candidate, CandidateKind};
    use ttfm::cli::interactive::autocomplete::{
        CompletionPhase, InteractiveHinter,
    };
    use ttfm::cli::interactive::runner::{
        InteractiveCompleter, InteractiveEditMode, PagingColumnarMenu,
    };

    let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
    let state = hinter.lock().unwrap().completion_state();
    let fetcher = Box::new(|_line: &str, pos: usize| {
        (0..10)
            .map(|i| Candidate {
                raw: format!("type{i}:"),
                kind: CandidateKind::Type,
                span: (2, pos),
            })
            .collect()
    });
    let mut completer = InteractiveCompleter::new(fetcher, "");
    let mut paging_menu = PagingColumnarMenu::new(
        reedline::ColumnarMenu::default().with_name("completion_menu"),
        Arc::new(AtomicUsize::new(0)),
        10,
    )
    .with_completion_state(Arc::clone(&state));
    let mut editor = reedline::Editor::default();
    editor.edit_buffer(
        |b| b.set_buffer("s ".to_string()),
        UndoBehavior::CreateUndoPoint,
    );

    paging_menu.menu_event(MenuEvent::Activate(false));
    paging_menu.update_values(&mut editor, &mut completer);
    assert!(paging_menu.is_active());
    assert_eq!(state.lock().unwrap().phase(), CompletionPhase::MenuPaging);
    assert_eq!(state.lock().unwrap().menu_base(), Some(("s ".into(), 2)));

    editor.edit_buffer(
        |b| {
            b.set_buffer("s type0:".to_string());
            b.set_insertion_point(8);
        },
        UndoBehavior::CreateUndoPoint,
    );

    let mut edit_mode = InteractiveEditMode::new(Emacs::default());
    edit_mode.set_hinter(Arc::clone(&hinter));
    edit_mode.notify_buffer_length(8);
    let ev = edit_mode.handle_reedline_event(ReedlineEvent::Esc);

    assert_ne!(ev, ReedlineEvent::Edit(vec![EditCommand::Clear]));
    assert_eq!(state.lock().unwrap().phase(), CompletionPhase::Off);
    assert!(!hinter.lock().unwrap().is_tracing());

    apply_edit_mode_esc(ev, &mut editor, &mut paging_menu);
    assert!(!paging_menu.is_active());
    assert_eq!(editor.get_buffer(), "s ");
    assert_eq!(editor.line_buffer().insertion_point(), 2);
}

#[test]
fn test_completion_phase_ssot_shared_across_hinter_menu_and_edit_mode() {
    use reedline::{Menu, MenuBuilder, MenuEvent, UndoBehavior};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use ttfm::autocomplete::{Candidate, CandidateKind};
    use ttfm::cli::interactive::autocomplete::{
        CompletionPhase, InteractiveHinter,
    };
    use ttfm::cli::interactive::runner::{
        InteractiveCompleter, InteractiveEditMode, PagingColumnarMenu,
    };

    let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
    hinter.lock().unwrap().set_last_query("extension:rs");
    let state = hinter.lock().unwrap().completion_state();
    let fetcher = Box::new(|_line: &str, pos: usize| {
        vec![Candidate {
            raw: "extension:".into(),
            kind: CandidateKind::Type,
            span: (2, pos),
        }]
    });
    let mut completer = InteractiveCompleter::new(fetcher, "extension:rs");
    let mut paging_menu = PagingColumnarMenu::new(
        reedline::ColumnarMenu::default().with_name("completion_menu"),
        Arc::new(AtomicUsize::new(0)),
        10,
    )
    .with_completion_state(Arc::clone(&state));
    let mut editor = reedline::Editor::default();
    editor.edit_buffer(
        |b| b.set_buffer("s ext".to_string()),
        UndoBehavior::CreateUndoPoint,
    );

    paging_menu.menu_event(MenuEvent::Activate(false));
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::MenuPaging);
    assert_eq!(hinter.lock().unwrap().handle_line("s ext", 5), "");
    assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::MenuPaging);

    paging_menu.menu_event(MenuEvent::Deactivate);
    assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::Off);

    assert_eq!(hinter.lock().unwrap().handle_line("s", 1), " extension:rs");
    assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::GhostActive);

    let mut edit_mode = InteractiveEditMode::new(Emacs::default());
    edit_mode.set_hinter(Arc::clone(&hinter));
    edit_mode.notify_buffer_length(1);
    let ev = edit_mode.handle_reedline_event(ReedlineEvent::Esc);
    assert_eq!(ev, ReedlineEvent::Repaint);
    assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::Off);
    assert_eq!(hinter.lock().unwrap().handle_line("s", 1), "");
}

#[test]
fn test_paging_dunder_prefix_does_not_corrupt_query_on_page_transition() {
    use reedline::{Menu, MenuBuilder, MenuEvent, UndoBehavior};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use ttfm::autocomplete::{Candidate, CandidateKind};
    use ttfm::cli::interactive::runner::{
        InteractiveCompleter, PagingColumnarMenu,
    };

    let offset = Arc::new(AtomicUsize::new(0));
    let offset_clone = Arc::clone(&offset);
    let fetcher = Box::new(move |_line: &str, _pos: usize| {
        let cur_off = offset_clone.load(Ordering::SeqCst);
        (cur_off..cur_off + 10)
            .map(|i| {
                let raw = if i < 10 {
                    format!("type{i}:")
                } else {
                    format!("__dunder{i}:")
                };
                Candidate {
                    raw,
                    kind: CandidateKind::Type,
                    span: (2, 2),
                }
            })
            .collect()
    });
    let mut completer = InteractiveCompleter::new(fetcher, "")
        .with_paging_offset(Arc::clone(&offset));
    let mut paging_menu = PagingColumnarMenu::new(
        reedline::ColumnarMenu::default().with_name("completion_menu"),
        Arc::clone(&offset),
        10,
    );
    let mut editor = reedline::Editor::default();
    editor.edit_buffer(
        |b| b.set_buffer("s ".to_string()),
        UndoBehavior::CreateUndoPoint,
    );

    paging_menu.menu_event(MenuEvent::Activate(false));
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(editor.get_buffer(), "s ");

    for _ in 0..10 {
        paging_menu.can_partially_complete(false, &mut editor, &mut completer);
        paging_menu.menu_event(MenuEvent::NextElement);
    }
    assert_eq!(offset.load(Ordering::SeqCst), 10);
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(paging_menu.get_values()[0].value, "__dunder10:");

    for _ in 0..5 {
        paging_menu.can_partially_complete(false, &mut editor, &mut completer);
        paging_menu.menu_event(MenuEvent::NextElement);
    }
    assert_eq!(editor.get_buffer(), "s ");
    assert!(
        !editor.get_buffer().contains("__"),
        "dunder common prefix must not be inserted into the query"
    );
}

#[test]
fn test_path_label_completion_matches_other_types_and_prefix_glob() {
    use ttfm::autocomplete::CandidateKind;

    let (store, registry, _dir, root) = setup_completion_env(&[
        "other/alpha.rs",
        "other/beta.rs",
        "lib/gamma.rs",
    ]);
    let path_empty = ttfm::autocomplete::fetch_candidates(
        &store, &registry, "s path:", 7, 0, 10,
    );
    let ext_empty = ttfm::autocomplete::fetch_candidates(
        &store,
        &registry,
        "s extension:",
        12,
        0,
        10,
    );
    assert!(!path_empty.is_empty());
    assert!(!ext_empty.is_empty());
    assert!(path_empty.iter().all(|c| c.kind == CandidateKind::Label));
    assert!(ext_empty.iter().all(|c| c.kind == CandidateKind::Label));
    assert!(path_empty.iter().all(|c| c.span == (7, 7)));
    assert!(ext_empty.iter().all(|c| c.span == (12, 12)));
    assert!(path_empty.iter().all(|c| c.raw.ends_with(' ')));
    assert!(ext_empty.iter().all(|c| c.raw.ends_with(' ')));

    let prefix = format!("{}/other/", root.display());
    let line = format!("s path:{prefix}");
    let path_pre = ttfm::autocomplete::fetch_candidates(
        &store,
        &registry,
        &line,
        line.len(),
        0,
        10,
    );
    assert!(!path_pre.is_empty());
    assert!(path_pre.iter().all(|c| c.kind == CandidateKind::Label));
    assert!(path_pre.iter().all(|c| c.span == (7, line.len())));
    assert!(path_pre.iter().all(|c| {
        let label = c.raw.trim_end_matches(' ').trim_matches('"');
        label.starts_with(&prefix)
    }));
    assert!(path_pre.iter().any(|c| c.raw.contains("alpha.rs")));
    assert!(!path_pre.iter().any(|c| c.raw.contains("gamma.rs")));

    let ext_pre = ttfm::autocomplete::fetch_candidates(
        &store,
        &registry,
        "s extension:r",
        13,
        0,
        10,
    );
    assert_eq!(ext_pre.len(), 1);
    assert_eq!(ext_pre[0].kind, CandidateKind::Label);
    assert_eq!(ext_pre[0].span, (12, 13));
}

#[test]
fn test_path_paging_preserves_exact_user_prefix() {
    use reedline::{Menu, MenuBuilder, MenuEvent, UndoBehavior};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use ttfm::cli::interactive::runner::{
        InteractiveCompleter, PagingColumnarMenu,
    };

    let (store, registry, _dir, root) = setup_completion_env(&[
        "d00.rs",
        "d01.rs",
        "d02.rs",
        "d03.rs",
        "d04.rs",
        "d05.rs",
        "d06.rs",
        "d07.rs",
        "d08.rs",
        "d09.rs",
        "__init__.rs",
        "__main__.rs",
    ]);
    let offset = Arc::new(AtomicUsize::new(0));
    let offset_clone = Arc::clone(&offset);
    let st = store.try_clone().unwrap();
    let reg = registry.clone();
    let fetcher = Box::new(move |line: &str, pos: usize| {
        let cur_off = offset_clone.load(Ordering::SeqCst);
        ttfm::autocomplete::fetch_candidates(&st, &reg, line, pos, cur_off, 10)
    });
    let mut completer = InteractiveCompleter::new(fetcher, "")
        .with_paging_offset(Arc::clone(&offset));
    let mut paging_menu = PagingColumnarMenu::new(
        reedline::ColumnarMenu::default().with_name("completion_menu"),
        Arc::clone(&offset),
        10,
    );
    let empty_line = "s path:".to_string();
    let mut editor = reedline::Editor::default();
    editor.edit_buffer(
        |b| {
            b.set_buffer(empty_line.clone());
            b.set_insertion_point(empty_line.len());
        },
        UndoBehavior::CreateUndoPoint,
    );

    paging_menu.menu_event(MenuEvent::Activate(false));
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(paging_menu.get_values().len(), 10);
    let page1_has_dunder = paging_menu
        .get_values()
        .iter()
        .any(|s| s.value.contains("__"));

    for _ in 0..10 {
        paging_menu.can_partially_complete(false, &mut editor, &mut completer);
        assert_eq!(editor.get_buffer(), empty_line);
        paging_menu.menu_event(MenuEvent::NextElement);
    }
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(offset.load(Ordering::SeqCst), 10);
    assert_eq!(editor.get_buffer(), empty_line);

    let prefix_line = format!("s path:{}/", root.display());
    offset.store(0, Ordering::SeqCst);
    editor.edit_buffer(
        |b| {
            b.set_buffer(prefix_line.clone());
            b.set_insertion_point(prefix_line.len());
        },
        UndoBehavior::CreateUndoPoint,
    );
    paging_menu.menu_event(MenuEvent::Activate(false));
    paging_menu.update_values(&mut editor, &mut completer);
    for _ in 0..10 {
        paging_menu.can_partially_complete(false, &mut editor, &mut completer);
        assert_eq!(editor.get_buffer(), prefix_line);
        paging_menu.menu_event(MenuEvent::NextElement);
    }
    paging_menu.update_values(&mut editor, &mut completer);
    assert_eq!(editor.get_buffer(), prefix_line);
    assert!(
        page1_has_dunder
            || paging_menu
                .get_values()
                .iter()
                .any(|s| s.value.contains("__"))
    );
}

#[test]
fn test_esc_while_off_with_hinter_clears_non_empty_buffer() {
    use reedline::EditCommand;
    use std::sync::{Arc, Mutex};
    use ttfm::cli::interactive::autocomplete::{
        CompletionPhase, InteractiveHinter,
    };
    use ttfm::cli::interactive::runner::InteractiveEditMode;

    let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
    hinter.lock().unwrap().set_last_query("extension:rs");
    hinter.lock().unwrap().handle_line("s extra", 7);
    assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::Off);

    let mut edit_mode = InteractiveEditMode::new(Emacs::default());
    edit_mode.set_hinter(Arc::clone(&hinter));
    edit_mode.notify_buffer_length(7);
    let ev = edit_mode.handle_reedline_event(ReedlineEvent::Esc);
    assert_eq!(ev, ReedlineEvent::Edit(vec![EditCommand::Clear]));
    assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::Off);
}

#[test]
fn test_nav_and_delete_keys_turn_completion_off_and_clear_ghost() {
    use reedline::{EditCommand, Hinter};
    use std::sync::{Arc, Mutex};
    use ttfm::cli::interactive::autocomplete::{
        CompletionPhase, InteractiveHinter,
    };
    use ttfm::cli::interactive::runner::InteractiveEditMode;

    let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
    hinter.lock().unwrap().set_last_query("extension:rs");
    assert_eq!(hinter.lock().unwrap().handle_line("s", 1), " extension:rs");
    assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::GhostActive);

    let mut edit_mode = InteractiveEditMode::new(Emacs::default());
    edit_mode.set_hinter(Arc::clone(&hinter));

    let ev = edit_mode.handle_reedline_event(ReedlineEvent::Left);
    assert_eq!(ev, ReedlineEvent::Left);
    assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::Off);
    assert_eq!(hinter.lock().unwrap().complete_hint(), "");

    hinter.lock().unwrap().set_last_query("extension:rs");
    assert_eq!(hinter.lock().unwrap().handle_line("s", 1), " extension:rs");
    let ev = edit_mode.handle_reedline_event(ReedlineEvent::Edit(vec![
        EditCommand::Backspace,
    ]));
    assert_eq!(ev, ReedlineEvent::Edit(vec![EditCommand::Backspace]));
    assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::Off);
    assert_eq!(hinter.lock().unwrap().complete_hint(), "");

    hinter.lock().unwrap().handle_line("s", 1);
    let ev = edit_mode
        .handle_reedline_event(ReedlineEvent::Edit(vec![EditCommand::Delete]));
    assert_eq!(ev, ReedlineEvent::Edit(vec![EditCommand::Delete]));
    assert_eq!(hinter.lock().unwrap().phase(), CompletionPhase::Off);
}

#[test]
fn test_partial_completion_syncs_query_base_with_base_line() {
    use reedline::{Menu, MenuBuilder, MenuEvent, UndoBehavior};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use ttfm::autocomplete::{Candidate, CandidateKind};
    use ttfm::cli::interactive::runner::{
        InteractiveCompleter, PagingColumnarMenu,
    };

    let seen = Arc::new(Mutex::new(Vec::<(String, usize)>::new()));
    let offset = Arc::new(AtomicUsize::new(0));
    let offset_clone = Arc::clone(&offset);
    let seen_clone = Arc::clone(&seen);
    let fetcher = Box::new(move |line: &str, pos: usize| {
        let cur_off = offset_clone.load(Ordering::SeqCst);
        seen_clone.lock().unwrap().push((line.to_string(), pos));
        (cur_off..cur_off + 10)
            .map(|i| Candidate {
                raw: format!("author{i}:"),
                kind: CandidateKind::Type,
                span: (2, pos),
            })
            .collect()
    });
    let mut completer = InteractiveCompleter::new(fetcher, "")
        .with_paging_offset(Arc::clone(&offset));
    let mut paging_menu = PagingColumnarMenu::new(
        reedline::ColumnarMenu::default().with_name("completion_menu"),
        Arc::clone(&offset),
        10,
    )
    .with_query_base(completer.shared_query_base());
    let mut editor = reedline::Editor::default();
    editor.edit_buffer(
        |b| {
            b.set_buffer("s aut".to_string());
            b.set_insertion_point(5);
        },
        UndoBehavior::CreateUndoPoint,
    );

    paging_menu.menu_event(MenuEvent::Activate(false));
    paging_menu.update_values(&mut editor, &mut completer);
    let did =
        paging_menu.can_partially_complete(false, &mut editor, &mut completer);
    let after_partial = editor.get_buffer().to_string();
    let after_pos = editor.line_buffer().insertion_point();
    if did {
        assert_eq!(
            completer.query_base_value(),
            Some((after_partial.clone(), after_pos))
        );
    }

    editor.edit_buffer(
        |b| {
            b.set_buffer("s author0:".to_string());
            b.set_insertion_point(10);
        },
        UndoBehavior::CreateUndoPoint,
    );
    for _ in 0..10 {
        paging_menu.menu_event(MenuEvent::NextElement);
    }
    assert_eq!(offset.load(Ordering::SeqCst), 10);
    paging_menu.update_values(&mut editor, &mut completer);
    let calls = seen.lock().unwrap();
    if did {
        assert!(calls
            .iter()
            .any(|(l, p)| l == &after_partial && *p == after_pos));
        assert!(!calls.iter().any(|(l, _)| l.contains("author0:")
            && offset.load(Ordering::SeqCst) == 10));
        assert_eq!(editor.get_buffer(), after_partial);
    } else {
        assert!(calls.iter().any(|(l, _)| l == "s aut"));
    }
}

#[test]
fn test_quoted_author_label_keeps_type_colon_context() {
    let ctx = ttfm::autocomplete::parse_completion_context(
        r#"s author:"John Doe"#,
        18,
    );
    assert!(
        matches!(ctx, ttfm::autocomplete::CompletionContext::TypeColon { tag_type, prefix, .. } if tag_type == "author" && prefix == "John Doe")
    );
}

#[test]
fn test_bash_completion_script_uses_mapfile_not_compgen_w() {
    let script_path =
        concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/completion/ttfm.bash");
    let content = std::fs::read_to_string(script_path)
        .expect("scripts/completion/ttfm.bash must exist");
    assert!(
        !content.contains("compgen -W"),
        "compgen -W interpolates candidates and is command-injection-prone"
    );
    assert!(
        content.contains("mapfile -t"),
        "bash completion must read line-delimited candidates with mapfile -t"
    );
}

struct StubTtfm {
    _dir: tempfile::TempDir,
    bin_path: std::path::PathBuf,
    args_file: std::path::PathBuf,
}

impl StubTtfm {
    fn args_file(&self) -> &std::path::Path {
        &self.args_file
    }
}

fn write_stub_ttfm(script: &str) -> StubTtfm {
    let dir = tempfile::tempdir().unwrap();
    let bin_path = dir.path().join("ttfm");
    let args_file = dir.path().join("args.txt");
    let content = format!(
        "#!/bin/sh\nexport STUB_ARGS='{}'\n{script}",
        args_file.display()
    );
    std::fs::write(&bin_path, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&bin_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin_path, perms).unwrap();
    }
    StubTtfm {
        _dir: dir,
        bin_path,
        args_file,
    }
}

fn run_bash_with_stub(
    stub: &StubTtfm,
    bash_code: &str,
) -> std::process::Output {
    let script_path =
        concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/completion/ttfm.bash");
    let path_dir = stub.bin_path.parent().unwrap().display();
    std::process::Command::new("bash")
        .args([
            "-c",
            &format!(
                r#"
export PATH="{path_dir}:$PATH"
source "{script_path}"
{bash_code}
"#
            ),
        ])
        .output()
        .expect("bash must be available")
}

#[test]
fn test_bash_completion_script_passes_line_and_copies_replies_verbatim() {
    let stub = write_stub_ttfm(
        r#"printf '%s\n' "$*" > "$STUB_ARGS"
printf '%s\n' 'filename:\"my\ file.txt\"' 'a b '
"#,
    );
    let out = run_bash_with_stub(
        &stub,
        r#"
COMP_LINE='ttfm search filename:my'
COMP_POINT=${#COMP_LINE}
COMP_WORDBREAKS=$' \t\n"'\''><=;|&(:'
_ttfm_completion
printf '%s\0' "${COMPREPLY[@]}"
"#,
    );
    assert_eq!(
        split_nul(&out.stdout),
        ["filename:\\\"my\\ file.txt\\\"", "a b "]
    );
    let args = std::fs::read_to_string(stub.args_file()).unwrap();
    assert!(args.contains("--shell bash"));
    assert!(args.contains("--point 23"));
    assert!(args.contains("ttfm search filename:my"));
}

#[test]
fn test_bash_completion_script_pages_offset_on_repeated_tab() {
    let stub = write_stub_ttfm(r#"printf '%s\0' "$*" >> "$STUB_ARGS""#);
    run_bash_with_stub(
        &stub,
        r#"
COMP_LINE='ttfm search e'
COMP_POINT=${#COMP_LINE}
_ttfm_completion
_ttfm_completion
COMP_LINE='ttfm search ex'
COMP_POINT=${#COMP_LINE}
_ttfm_completion
"#,
    );
    let bytes = std::fs::read(stub.args_file()).unwrap();
    let calls = split_nul(&bytes);
    assert!(calls[0].contains("--offset 0"));
    assert!(calls[1].contains("--offset 10"));
    assert!(calls[2].contains("--offset 0"));
}

#[test]
fn test_bash_completion_script_preserves_offset_on_listing_comp_type() {
    let stub = write_stub_ttfm(r#"printf '%s\0' "$*" >> "$STUB_ARGS""#);
    run_bash_with_stub(
        &stub,
        r#"
COMP_LINE='ttfm search e'
COMP_POINT=${#COMP_LINE}
COMP_TYPE=9
_ttfm_completion
COMP_TYPE=63
_ttfm_completion
COMP_TYPE=63
_ttfm_completion
"#,
    );
    let bytes = std::fs::read(stub.args_file()).unwrap();
    let calls = split_nul(&bytes);
    assert!(calls[0].contains("--offset 0"));
    assert!(calls[1].contains("--offset 0"));
    assert!(calls[2].contains("--offset 10"));
}

struct Replay {
    replies: Vec<String>,
    argv: Vec<String>,
    canary: bool,
}

fn run_ttfm(home: &std::path::Path, args: &[&str]) {
    let ttfm_bin = env!("CARGO_BIN_EXE_ttfm");
    let status = std::process::Command::new(ttfm_bin)
        .env("TTFM_HOME", home)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "run_ttfm failed for args: {args:?}");
}

fn prepend_path(bin_dir: &std::path::Path) -> std::ffi::OsString {
    let current_path = std::env::var_os("PATH").unwrap_or_default();
    let mut new_path = bin_dir.as_os_str().to_os_string();
    if !current_path.is_empty() {
        new_path.push(":");
        new_path.push(current_path);
    }
    new_path
}

fn split_nul(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect()
}

fn longest_common_prefix(strings: &[String]) -> String {
    if strings.is_empty() {
        return String::new();
    }
    let first = &strings[0];
    let mut len = 0;
    for (i, c) in first.char_indices() {
        let all_match = strings
            .iter()
            .all(|s| s.get(i..).map_or(false, |sub| sub.starts_with(c)));
        if all_match {
            len = i + c.len_utf8();
        } else {
            break;
        }
    }
    first[..len].to_string()
}

fn setup_shell_fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let files = dir.path().join("files");
    std::fs::create_dir_all(&files).unwrap();
    for name in [
        "test$(touch canary).txt",
        "it's.txt",
        "a!b.txt",
        "my file.txt",
        "nl\nname.txt",
        "baz.txt",
    ] {
        std::fs::write(files.join(name), "x").unwrap();
    }
    run_ttfm(&home, &["index", files.to_str().unwrap()]);
    run_ttfm(&home, &["-y", "tag", "filename:baz.txt", "project:x"]);
    (dir, home)
}

fn bash_complete_and_replay(
    home: &std::path::Path,
    cwd: &std::path::Path,
    line: &str,
    wordbreaks: &str,
    rl_start: usize,
    open_quote: Option<char>,
) -> Replay {
    let bin_dir = std::path::Path::new(env!("CARGO_BIN_EXE_ttfm"))
        .parent()
        .unwrap();
    let script =
        concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/completion/ttfm.bash");
    let cmd = format!(
        r#"
source "{script}"
COMP_LINE="$LINE"
COMP_POINT=${{#LINE}}
COMP_WORDBREAKS="$WB"
_ttfm_completion
printf '%s\0' "${{COMPREPLY[@]}}"
"#
    );
    let out = std::process::Command::new("bash")
        .current_dir(cwd)
        .env("PATH", prepend_path(bin_dir))
        .env("TTFM_HOME", home)
        .env("LINE", line)
        .env("WB", wordbreaks)
        .args(["-c", &cmd])
        .output()
        .unwrap();
    let replies = split_nul(&out.stdout);
    let readline_closes_quote = replies.len() == 1 && open_quote.is_some();
    assert!(!readline_closes_quote, "{replies:?}");
    let lcp = longest_common_prefix(&replies);
    let mut edited = format!("{}{}", &line[..rl_start], lcp);
    if let Some(q) = open_quote {
        edited.push(q);
    }
    let argv_out = std::process::Command::new("bash")
        .current_dir(cwd)
        .args([
            "-c",
            r#"eval "set -- $1"; printf '%s\0' "$@""#,
            "_",
            &edited,
        ])
        .output()
        .unwrap();
    Replay {
        replies,
        argv: split_nul(&argv_out.stdout),
        canary: cwd.join("canary").exists(),
    }
}

const WB_DEFAULT: &str = " \t\n\"'><=;|&(:";
const WB_NO_COLON: &str = " \t\n\"'><=;|&(";

#[test]
fn test_bash_completion_unquoted_escapes_shell_metacharacters() {
    let (dir, home) = setup_shell_fixture();
    let line = "ttfm search filename:test";
    let r = bash_complete_and_replay(
        &home,
        dir.path(),
        line,
        WB_DEFAULT,
        line.find(':').unwrap() + 1,
        None,
    );
    assert_eq!(
        r.argv,
        ["ttfm", "search", "filename:\"test$(touch canary).txt\""]
    );
    assert!(!r.canary);
}

#[test]
fn test_bash_completion_single_quoted_keeps_quote_open() {
    let (dir, home) = setup_shell_fixture();
    let line = "ttfm search 'filename:test";
    let r = bash_complete_and_replay(
        &home,
        dir.path(),
        line,
        WB_DEFAULT,
        line.find('\'').unwrap() + 1,
        Some('\''),
    );
    assert_eq!(r.replies.len(), 2);
    assert_eq!(
        r.argv,
        ["ttfm", "search", "filename:\"test$(touch canary).txt\" "]
    );
    assert!(!r.canary);
}

#[test]
fn test_bash_completion_double_quoted_escapes_dollar_and_quote() {
    let (dir, home) = setup_shell_fixture();
    let line = "ttfm search \"filename:test";
    let r = bash_complete_and_replay(
        &home,
        dir.path(),
        line,
        WB_DEFAULT,
        line.find('"').unwrap() + 1,
        Some('"'),
    );
    assert_eq!(
        r.argv,
        ["ttfm", "search", "filename:\"test$(touch canary).txt\" "]
    );
    assert!(!r.canary);
}

#[test]
fn test_bash_completion_single_quote_inside_label() {
    let (dir, home) = setup_shell_fixture();
    let line = "ttfm search 'filename:it";
    let r = bash_complete_and_replay(
        &home,
        dir.path(),
        line,
        WB_DEFAULT,
        line.find('\'').unwrap() + 1,
        Some('\''),
    );
    assert_eq!(r.argv, ["ttfm", "search", "filename:\"it's.txt\" "]);
}

#[test]
fn test_bash_completion_bang_inside_double_quotes() {
    let (dir, home) = setup_shell_fixture();
    let line = "ttfm search \"filename:a";
    let r = bash_complete_and_replay(
        &home,
        dir.path(),
        line,
        WB_DEFAULT,
        line.find('"').unwrap() + 1,
        Some('"'),
    );
    assert_eq!(r.argv, ["ttfm", "search", "filename:a!b.txt "]);
    assert!(r.replies.iter().all(|c| !c.contains("\\!")));
}

#[test]
fn test_bash_completion_respects_wordbreaks_without_colon() {
    let (dir, home) = setup_shell_fixture();
    let line = "ttfm search filename:my";
    let r = bash_complete_and_replay(
        &home,
        dir.path(),
        line,
        WB_NO_COLON,
        "ttfm search ".len(),
        None,
    );
    assert_eq!(r.argv, ["ttfm", "search", "filename:\"my file.txt\""]);
}

#[test]
fn test_bash_completion_drops_control_character_candidates() {
    let (dir, home) = setup_shell_fixture();
    let line = "ttfm search 'filename:nl";
    let r = bash_complete_and_replay(
        &home,
        dir.path(),
        line,
        WB_DEFAULT,
        line.find('\'').unwrap() + 1,
        Some('\''),
    );
    assert!(r.replies.is_empty());
}

#[test]
fn test_bash_completion_index_defers_to_shell_filenames() {
    let (dir, home) = setup_shell_fixture();
    let line = "ttfm index ";
    let r = bash_complete_and_replay(
        &home,
        dir.path(),
        line,
        WB_DEFAULT,
        line.len(),
        None,
    );
    assert!(r.replies.is_empty());
}

#[test]
fn test_bash_completion_tag_edit_argument() {
    let (dir, home) = setup_shell_fixture();
    let line = "ttfm tag 'filename:baz.txt' 'proj";
    let r = bash_complete_and_replay(
        &home,
        dir.path(),
        line,
        WB_DEFAULT,
        line.rfind('\'').unwrap() + 1,
        Some('\''),
    );
    assert_eq!(r.argv, ["ttfm", "tag", "filename:baz.txt", "project:"]);
}
