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

use std::io::Cursor;
use std::path::PathBuf;
use tempfile::TempDir;
use ttfm::{
    cli::interactive::run_interactive_stream, config::Config, db::Store,
    edit::WriteOptions, indexing::Indexer, tag::TagRegistry,
};

fn setup_env(files: &[&str]) -> (Store, TagRegistry, TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("files");
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
fn test_interactive_init_menu_and_quit() {
    let (store, registry, _dir, _root) = setup_env(&["a.txt"]);
    let input = Cursor::new(b"m\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("s : Search"));
    assert!(out_str.contains("i : Index directories"));
    assert!(!out_str.contains("Examples:"));
}

#[test]
fn test_interactive_search_and_next_paging() {
    let files: Vec<String> =
        (0..45).map(|i| format!("file_{i:02}.rs")).collect();
    let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let (store, registry, _dir, _root) = setup_env(&file_refs);
    let input = Cursor::new(b"s extension:rs\nn\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(
        out_str.contains("items matched")
            || out_str.contains("s \x1b[2msearch\x1b[0m")
    );
    assert!(out_str.contains("file_"));
}

#[test]
fn test_interactive_tag_quoted_and_auto_requery_refresh() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input =
        Cursor::new(b"s extension:txt\nt \"extension:txt\" project:alpha\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("Updated tags"));
    assert!(out_str.contains("sample.txt"));
}

#[test]
fn test_interactive_tag_requires_quotes_and_error_message() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(
        b"s extension:txt\nt unquoted project:alpha\nt \"extension:txt\"\nq\n",
    );
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let err_str = String::from_utf8(err_out).unwrap();
    assert!(err_str.contains("double quotes"));
    assert!(err_str.contains("Missing required argument"));
}

#[test]
fn test_interactive_untag_rejects_empty_quotes() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(b"s extension:txt\nu \"extension:txt\" \"\"\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let err_str = String::from_utf8(err_out).unwrap();
    assert!(err_str.contains("Missing required argument"));
}

#[test]
fn test_interactive_rejects_empty_search_query() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(b"s \"\"\nt \"\" project:alpha\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let err_str = String::from_utf8(err_out).unwrap();
    assert!(
        err_str.contains("Empty query")
            || err_str.contains("Missing required argument")
            || err_str.contains("syntax error")
            || err_str.contains("Empty search query")
    );
}

#[test]
fn test_interactive_clear_index_with_prompt() {
    let (store, registry, _dir, _root) = setup_env(&["a.txt"]);
    let input = Cursor::new(b"clear\ny\ns *:*\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::interactive();

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

    let out_str = String::from_utf8(output).unwrap();
    let err_str = String::from_utf8(err_out).unwrap();
    assert!(out_str.contains("Clear indexed files? [y/N]: "));
    assert!(out_str.contains("File indexes cleared successfully."));
    assert!(!err_str.contains("Error"));
}

#[test]
fn test_interactive_clear_index_with_prompt_cancelled() {
    let (store, registry, _dir, _root) = setup_env(&["a.txt"]);
    let input = Cursor::new(b"clear\nn\ns *:*\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::interactive();

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("Clear indexed files? [y/N]: "));
    assert!(out_str.contains("Cancelled."));
    assert!(out_str.contains("a.txt"));
}

#[test]
fn test_interactive_tag_quoted_second_argument() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(
        b"s extension:txt\nt \"extension:txt\" \"project:alpha\"\nq\n",
    );
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("Updated tags"));
    assert!(out_str.contains("sample.txt"));
}

#[test]
fn test_interactive_tag_with_confirm_prompt() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(
        b"s extension:txt\nt \"extension:txt\" \"project:alpha\"\ny\nq\n",
    );
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let mut write_opts = WriteOptions::interactive();
    write_opts.confirm = ttfm::config::ConfirmMode::Always;

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("Apply these changes? [y/N]: "));
    assert!(out_str.contains("Updated tags"));
}

#[test]
fn test_interactive_quit_hierarchical_navigation() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(b"s extension:txt\nq\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("Welcome to ttfm interactive mode"));
    assert!(out_str.contains("q \x1b[2mquit to main menu\x1b[0m"));
    assert!(out_str.contains("clear : Clear indexed files (or `clear all`)"));
}

#[test]
fn test_interactive_clear_with_confirm_never_skips_prompt() {
    let (store, registry, _dir, _root) = setup_env(&["a.txt"]);
    let input = Cursor::new(b"clear\ns *:*\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let mut write_opts = WriteOptions::default();
    write_opts.confirm = ttfm::config::ConfirmMode::Never;

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("File indexes cleared successfully."));
    assert!(!out_str.contains("[y/N]"));
}

#[test]
fn test_interactive_search_internal_quotes_preserved() {
    let (store, registry, _dir, _root) = setup_env(&["sample.txt"]);
    let input = Cursor::new(b"s \"extension:txt\"\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("sample.txt"));
}

#[test]
fn test_interactive_completer_no_matches_returns_empty() {
    use reedline::Completer;
    use ttfm::cli::interactive::runner::InteractiveCompleter;

    let fetcher = Box::new(|_line: &str, _pos: usize| vec![]);
    let mut completer = InteractiveCompleter::new(fetcher, "");
    let res = completer.complete("s nonexistent_query", 19);
    assert!(res.is_empty());
}

#[test]
fn test_interactive_help_command() {
    let (store, registry, _dir, _root) = setup_env(&["a.txt"]);
    let input = Cursor::new(b"h\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("TTFM Interactive Mode Help"));
    assert!(out_str.contains("Syntax & Examples:"));
    assert!(out_str.contains("Basic Tags (type:label):"));
    assert!(out_str.contains("Set Operations (&, |, -):"));
    assert!(out_str.contains("Glob Patterns & Captures (*, {n}):"));
    assert!(out_str.contains("Comparisons & Ranges:"));
    assert!(out_str.contains("Show types/labels/tags definitions & Storing:"));
    assert!(out_str.contains("Aggregations (count, sum, avg, ...):"));
    assert!(out_str.contains("Nesting (&:):"));
    assert!(out_str.contains("Nesting with Aggregation (Combined):"));
    assert!(out_str.contains("path:^/mnt/*/: &: sum(size:)"));
    assert!(out_str.contains("Eval (q()):"));
    assert!(out_str.contains("# Show right columns"));
    assert!(out_str.contains("# Show left columns"));
}

#[test]
fn test_quit_to_init_restores_default_query_ghost() {
    use std::sync::{Arc, Mutex};
    use ttfm::cli::interactive::autocomplete::{
        InteractiveHinter, DEFAULT_QUERY,
    };
    use ttfm::cli::interactive::command::{dispatch_command, parse_command};
    use ttfm::cli::interactive::runner::sync_hinter_last_query;
    use ttfm::cli::interactive::state::State;

    let (store, registry, _dir, _root) = setup_env(&["a.txt"]);
    let mut state = State::new();
    state.to_searched("extension:txt".into(), None, 1, None, false);
    let hinter = Arc::new(Mutex::new(InteractiveHinter::new()));
    hinter.lock().unwrap().set_last_query("extension:txt");

    let cmd = parse_command("q", true).unwrap();
    let mut path = ".".to_string();
    let config = Config::default();
    let write_opts = WriteOptions::default();
    let mut input = Cursor::new(Vec::<u8>::new());
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let cont = dispatch_command(
        cmd,
        &mut state,
        &store,
        &registry,
        &config,
        write_opts,
        &mut path,
        &mut input,
        &mut output,
        &mut err_out,
    )
    .unwrap();
    assert!(cont);
    assert!(matches!(state, State::Init));
    assert_eq!(state.last_query(), None);

    sync_hinter_last_query(&hinter, state.last_query(), DEFAULT_QUERY);
    assert_eq!(hinter.lock().unwrap().handle_line("s", 1), " *:*");

    state.to_searched("extension:txt".into(), None, 1, None, false);
    hinter.lock().unwrap().set_last_query("extension:txt");
    let cmd = parse_command("q", true).unwrap();
    dispatch_command(
        cmd,
        &mut state,
        &store,
        &registry,
        &config,
        WriteOptions::default(),
        &mut path,
        &mut input,
        &mut output,
        &mut err_out,
    )
    .unwrap();
    let initial = "filename:a.txt";
    sync_hinter_last_query(&hinter, state.last_query(), initial);
    assert_eq!(
        hinter.lock().unwrap().handle_line("s", 1),
        " filename:a.txt"
    );
}

#[test]
fn test_interactive_prev_paging_and_column_scroll() {
    let files: Vec<String> =
        (0..45).map(|i| format!("file_{i:02}.rs")).collect();
    let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let (store, registry, _dir, _root) = setup_env(&file_refs);
    let input = Cursor::new(b"s extension:rs\np\nn\np\n>\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(out_str.contains("Already at first page."));
    assert!(out_str.contains("items matched."));
    assert!(out_str.contains("file_"));
}

#[test]
fn test_interactive_searched_dynamic_hints_and_no_examples() {
    let files: Vec<String> =
        (0..45).map(|i| format!("file_{i:02}.rs")).collect();
    let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let (store, registry, _dir, _root) = setup_env(&file_refs);
    let input = Cursor::new(b"s extension:rs\nn\nq\n");
    let mut output = Vec::new();
    let mut err_out = Vec::new();
    let config = Config::default();
    let write_opts = WriteOptions::default();

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

    let out_str = String::from_utf8(output).unwrap();
    assert!(!out_str.contains("Examples:"));
    assert!(out_str.contains("s \x1b[2msearch\x1b[0m"));
    assert!(out_str.contains("n \x1b[2mnext\x1b[0m"));
    assert!(out_str.contains("p \x1b[2mprev\x1b[0m"));
    assert!(out_str.contains("h \x1b[2mhelp\x1b[0m"));
}
