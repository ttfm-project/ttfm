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

use clap::Parser;
use std::io::IsTerminal;
use ttfm::cli::args::{build_write_options, Cli, Commands};
use ttfm::cli::format::{
    format_tag_result, format_untag_result, print_results_with_options,
    print_simple_results, ColorWarningSink, FormatOptions,
};
use ttfm::cli::progress::MultiStageProgressView;
use ttfm::config::Config;
use ttfm::db::Store;
use ttfm::edit::{edit, QueryType};
use ttfm::safe_println;
use ttfm::tag::TagRegistry;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if ttfm::search::maybe_run_worker()? {
        return Ok(());
    }

    let cli = Cli::parse();
    run(cli)
}

fn open_store_and_initialize(
    db_dir: &std::path::Path,
    registry: &TagRegistry,
) -> Result<Store, Box<dyn std::error::Error>> {
    let store = Store::open(db_dir)?;
    ttfm::indexing::Indexer::new(&store, registry).initialize_tables()?;
    registry.load_type_configs(&store)?;
    Ok(store)
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(Commands::Clear { all: true }) = cli.command {
        let home = ttfm::get_ttfm_home()?;
        Store::delete_database(home.join("db").as_path())?;
        safe_println!("Database cleared successfully.");
        return Ok(());
    }

    if let Some(Commands::Completion { shell }) = &cli.command {
        print!("{}", shell.script());
        return Ok(());
    }

    if let Some(Commands::Complete {
        offset,
        point,
        wordbreaks,
        shell,
        query,
    }) = &cli.command
    {
        let line = query.join(" ");
        let default_wb = ttfm::cli::shell_completion::BASH_DEFAULT_WORDBREAKS;
        let wb = wordbreaks.as_deref().unwrap_or(default_wb);
        let pt = point.unwrap_or(line.len());
        let off = offset.unwrap_or(0);
        let lim = ttfm::cli::shell_completion::DEFAULT_SHELL_COMPLETION_LIMIT;
        let dry_outcome =
            ttfm::cli::shell_completion::evaluate_shell_completion(
                None, &line, pt, wb, *shell, off, lim,
            );
        match dry_outcome {
            ttfm::cli::shell_completion::ShellCompletionOutcome::Completed(
                cands,
            ) if !cands.is_empty() => {
                for c in cands {
                    println!("{c}");
                }
                return Ok(());
            }
            ttfm::cli::shell_completion::ShellCompletionOutcome::Fallback => {
                std::process::exit(
                    ttfm::cli::shell_completion::EXIT_SHELL_FALLBACK,
                );
            }
            _ => {}
        }
        let home = ttfm::get_ttfm_home()?;
        let db_dir = home.join("db");
        if !db_dir.exists() {
            return Ok(());
        }
        let mut registry = TagRegistry::with_standard();
        let config = Config::load();
        if config.plugins.enabled {
            registry
                .load_from_dir(home.join("plugins"), &config.plugins.status)?;
            registry.load_builtins(&config.plugins.status)?;
        }
        let store = open_store_and_initialize(&db_dir, &registry)?;
        let outcome = ttfm::cli::shell_completion::evaluate_shell_completion(
            Some((&store, &registry)),
            &line,
            pt,
            wb,
            *shell,
            off,
            lim,
        );
        if let ttfm::cli::shell_completion::ShellCompletionOutcome::Completed(
            cands,
        ) = outcome
        {
            for c in cands {
                println!("{c}");
            }
        }
        return Ok(());
    }

    if let Some(Commands::Clear { all: false }) = cli.command {
        let home = ttfm::get_ttfm_home()?;
        let store = Store::open(home.join("db"))?;
        store.clear_index()?;
        safe_println!("File indexes cleared successfully.");
        return Ok(());
    }

    let home = ttfm::get_ttfm_home()?;
    let plugins_dir = home.join("plugins");
    if !plugins_dir.exists() {
        std::fs::create_dir_all(&plugins_dir)?;
    }

    let mut registry = TagRegistry::with_standard();
    let config = Config::load();

    if config.plugins.enabled {
        registry.load_from_dir(
            ttfm::get_ttfm_plugins_dir()?,
            &config.plugins.status,
        )?;
        registry.load_builtins(&config.plugins.status)?;
    }

    let store = open_store_and_initialize(&home.join("db"), &registry)?;

    let write_opts = build_write_options(&cli, &config);

    if let Some(opt_query) = cli.interactive {
        let initial_query = opt_query.as_deref();
        let mut interactive_write_options = write_opts.clone();
        if !cli.yes && cli.confirm != Some(ttfm::config::ConfirmMode::Never) {
            interactive_write_options.confirm =
                ttfm::config::ConfirmMode::Always;
        }
        if std::io::stdin().is_terminal() {
            return ttfm::cli::interactive::runner::run_interactive_terminal(
                &store,
                &registry,
                &config,
                interactive_write_options,
                initial_query,
                cli.quiet,
            );
        } else {
            return ttfm::cli::interactive::runner::run_interactive_stream(
                &store,
                &registry,
                &config,
                interactive_write_options,
                initial_query,
                std::io::stdin(),
                &mut std::io::stdout(),
                &mut std::io::stderr(),
                cli.quiet,
            )
            .map_err(|e| e.into());
        }
    }

    match &cli.command {
        Some(cmd) => match cmd {
            Commands::Index { paths, dry_run } => {
                safe_println!(
                    "Indexing directories: {:?} (dry-run: {})",
                    paths,
                    dry_run
                );

                let enabled = !cli.quiet && std::io::stderr().is_terminal();
                let view = MultiStageProgressView::for_stderr(enabled);
                let count = ttfm::indexing::Indexer::new(&store, &registry)
                    .run(paths, Some(&|p| view.handle_progress(p)), *dry_run)?;

                if *dry_run {
                    view.finish_dry_run();
                } else {
                    view.finish();
                }

                safe_println!("Done! Successfully indexed {} files.", count);
            }
            Commands::Search {
                query,
                short,
                wide,
                n,
                offset,
                cid,
            } => {
                let opts = ttfm::SearchOptions {
                    n: Some(n.unwrap_or(100)),
                    offset: *offset,
                    cid: cid.clone(),
                    ..Default::default()
                };
                let mut stdout = std::io::stdout();
                let mut sink = ColorWarningSink {
                    writer: &mut stdout,
                };
                if *short {
                    let response = ttfm::search::search_short(
                        &store, &registry, query, opts, &mut sink,
                    )?;
                    print_simple_results(&registry, &response);
                } else {
                    safe_println!("Searching for: '{}'", query);
                    let response = ttfm::search::search(
                        &store, &registry, query, opts, &mut sink,
                    )?;
                    print_results_with_options(
                        &store,
                        &registry,
                        &response,
                        query,
                        n.unwrap_or(100),
                        &mut std::io::stdout(),
                        FormatOptions {
                            is_interactive: false,
                            wide: *wide,
                            col_offset: 0,
                            ..Default::default()
                        },
                    );
                }
            }
            Commands::Tag {
                search_query,
                edit_query,
            } => {
                let edit_query_opt = if edit_query.is_empty() {
                    None
                } else {
                    Some(edit_query.as_str())
                };
                let mut stdout = std::io::stdout();
                let resp = edit(
                    &store,
                    &registry,
                    search_query,
                    edit_query_opt,
                    QueryType::Tag,
                    None,
                    write_opts,
                    &mut ColorWarningSink {
                        writer: &mut stdout,
                    },
                )?;
                safe_println!("{}", format_tag_result(&resp));
            }
            Commands::Untag {
                search_query,
                tag_query,
                condition,
            } => {
                let mut stdout = std::io::stdout();
                let resp = edit(
                    &store,
                    &registry,
                    search_query,
                    Some(tag_query.as_str()),
                    QueryType::Untag,
                    condition.as_deref(),
                    write_opts,
                    &mut ColorWarningSink {
                        writer: &mut stdout,
                    },
                )?;
                safe_println!("{}", format_untag_result(&resp));
            }
            Commands::Replace { old: _, new_tag: _ } => {
                return Err("replace は未実装です".into());
            }
            Commands::Decal { from: _, to: _ } => {
                return Err("decal は未実装です".into());
            }
            Commands::Clear { .. } => unreachable!("Handled early"),
            Commands::Complete { .. } => unreachable!("Handled early"),
            Commands::Completion { .. } => unreachable!("Handled early"),
            Commands::Note { content } => {
                let id = ttfm::edit::create_note(&store, &registry, content)?;
                safe_println!("Created note (ID: {})", id);
            }
        },
        None => {
            safe_println!("No command specified. Run with --help for usage.");
        }
    }

    Ok(())
}
