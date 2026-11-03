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

use clap::CommandFactory;

use crate::autocomplete::{resolve_ttql_candidates, TtqlRole};
use crate::db::Store;
use crate::tag::TagRegistry;

pub const BASH_DEFAULT_WORDBREAKS: &str = " \t\n\"'><=;|&(:";
pub const DEFAULT_SHELL_COMPLETION_LIMIT: usize = 40;
pub const EXIT_SHELL_FALLBACK: i32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCompletionOutcome {
    Completed(Vec<String>),
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellQuote {
    None,
    Single,
    Double,
}

pub struct CurrentWord {
    pub text: String,
    pub quote: ShellQuote,
    pub text_before_rl: String,
    pub rl_text: String,
}

pub struct ShellLine {
    pub args: Vec<String>,
    pub current: CurrentWord,
}

pub fn lex_shell_line(line: &str, wordbreaks: &str) -> Option<ShellLine> {
    let mut args = Vec::new();
    let mut text = String::new();
    let mut in_word = false;
    let mut quote = ShellQuote::None;
    let mut closed_quote = false;
    let mut rl_raw = 0;
    let mut rl_deq = 0;
    let mut chars = line.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match quote {
            ShellQuote::Single => match c {
                '\'' => {
                    quote = ShellQuote::None;
                    closed_quote = true;
                }
                _ => text.push(c),
            },
            ShellQuote::Double => match c {
                '"' => {
                    quote = ShellQuote::None;
                    closed_quote = true;
                }
                '$' | '`' => return None,
                '\\' => match chars.peek().map(|&(_, n)| n) {
                    Some(n @ ('$' | '`' | '"' | '\\')) => {
                        text.push(n);
                        chars.next();
                    }
                    _ => text.push('\\'),
                },
                _ => text.push(c),
            },
            ShellQuote::None => {
                if !in_word && !c.is_whitespace() {
                    in_word = true;
                    closed_quote = false;
                    rl_raw = i;
                    rl_deq = 0;
                }
                match c {
                    ' ' | '\t' => {
                        if in_word {
                            args.push(std::mem::take(&mut text));
                            in_word = false;
                        }
                    }
                    '\\' => {
                        let (_, n) = chars.next()?;
                        text.push(n);
                    }
                    '\'' | '"' => {
                        quote = if c == '\'' {
                            ShellQuote::Single
                        } else {
                            ShellQuote::Double
                        };
                        rl_raw = i + 1;
                        rl_deq = text.len();
                    }
                    '$' | '`' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
                    | '\n' => return None,
                    c if wordbreaks.contains(c) => {
                        text.push(c);
                        rl_raw = i + c.len_utf8();
                        rl_deq = text.len();
                    }
                    _ => text.push(c),
                }
            }
        }
    }
    if quote == ShellQuote::None && closed_quote && in_word {
        return None;
    }
    if !in_word && quote == ShellQuote::None {
        rl_raw = line.len();
        rl_deq = 0;
    }
    Some(ShellLine {
        args,
        current: CurrentWord {
            text_before_rl: text[..rl_deq].to_string(),
            rl_text: line[rl_raw..].to_string(),
            text,
            quote,
        },
    })
}

const UNQUOTED_META: &[char] = &[
    ' ', '\\', '\'', '"', '`', '$', '&', '|', ';', '(', ')', '<', '>', '*',
    '?', '[', ']', '#', '~', '!', '{', '}',
];

pub fn quote_for_shell(text: &str, quote: ShellQuote) -> Option<String> {
    if text.chars().any(char::is_control) {
        return None;
    }
    let mut out = String::new();
    for c in text.chars() {
        match quote {
            ShellQuote::None => {
                if UNQUOTED_META.contains(&c) {
                    out.push('\\');
                }
                out.push(c);
            }
            ShellQuote::Single => match c {
                '\'' => out.push_str(r"'\''"),
                _ => out.push(c),
            },
            ShellQuote::Double => match c {
                '$' | '`' | '"' | '\\' => {
                    out.push('\\');
                    out.push(c);
                }
                '!' => out.push_str(r#""'!'""#),
                _ => out.push(c),
            },
        }
    }
    Some(out)
}

const GLOBAL_VALUE_OPTS: &[&str] = &[
    "--confirm",
    "--on-conflict",
    "--on-hardlink",
    "--skip-scope",
    "-i",
];

fn value_options(subcommand: &str) -> Vec<&'static str> {
    let mut opts = GLOBAL_VALUE_OPTS.to_vec();
    match subcommand {
        "search" => opts.extend(["-n", "--n", "--offset", "--cid"]),
        "untag" => opts.push("--condition"),
        _ => {}
    }
    opts
}

fn positionals<'a>(
    args: &'a [String],
    value_opts: &[&str],
) -> Option<Vec<&'a str>> {
    let mut out = Vec::new();
    let mut expects_value = false;
    for a in args {
        if expects_value {
            expects_value = false;
        } else if let Some((opt, _)) = a.split_once('=') {
            if !opt.starts_with('-') {
                out.push(a.as_str());
            }
        } else if value_opts.contains(&a.as_str()) {
            expects_value = true;
        } else if !a.starts_with('-') {
            out.push(a.as_str());
        }
    }
    (!expects_value).then_some(out)
}

pub fn query_arg_role(args: &[String]) -> Option<TtqlRole> {
    let top = positionals(args.get(1..)?, GLOBAL_VALUE_OPTS)?;
    let subcommand = *top.first()?;
    let sub_pos = args.iter().position(|a| a == subcommand)?;
    let rest = positionals(&args[sub_pos + 1..], &value_options(subcommand))?;
    match (subcommand, rest.len()) {
        ("search" | "tag" | "untag", 0) => Some(TtqlRole::Search),
        ("tag", 1) => Some(TtqlRole::Edit { is_untag: false }),
        ("untag", 1) => Some(TtqlRole::Edit { is_untag: true }),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

impl ShellKind {
    pub fn script(self) -> &'static str {
        match self {
            ShellKind::Bash => {
                include_str!("../../scripts/completion/ttfm.bash")
            }
            ShellKind::Zsh => include_str!("../../scripts/completion/_ttfm"),
            ShellKind::Fish => {
                include_str!("../../scripts/completion/ttfm.fish")
            }
        }
    }
}

fn clap_metadata() -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut cmd = crate::cli::args::Cli::command();
    cmd.build();
    let subcommands: Vec<String> = cmd
        .get_subcommands()
        .filter(|s| !s.is_hide_set())
        .map(|s| s.get_name().to_string())
        .collect();
    let mut global_opts = Vec::new();
    let mut global_val_opts = Vec::new();
    for a in cmd.get_arguments() {
        if a.is_positional() {
            continue;
        }
        let takes_val = a.get_action().takes_values()
            && a.get_num_args().map_or(true, |r| r.min_values() > 0);
        let mut names = Vec::new();
        if let Some(l) = a.get_long() {
            names.push(format!("--{l}"));
        }
        if let Some(s) = a.get_short() {
            names.push(format!("-{s}"));
        }
        for n in names {
            if takes_val {
                global_val_opts.push(n.clone());
            }
            global_opts.push(n);
        }
    }
    (subcommands, global_opts, global_val_opts)
}

fn clap_subcommand_opts(subcommand: &str) -> (Vec<String>, Vec<String>) {
    let mut cmd = crate::cli::args::Cli::command();
    cmd.build();
    let Some(sub) = cmd.find_subcommand(subcommand) else {
        return (vec![], vec![]);
    };
    let mut opts = Vec::new();
    let mut val_opts = Vec::new();
    for a in sub.get_arguments() {
        if a.is_positional() {
            continue;
        }
        let takes_val = a.get_action().takes_values()
            && a.get_num_args().map_or(true, |r| r.min_values() > 0);
        let mut names = Vec::new();
        if let Some(l) = a.get_long() {
            names.push(format!("--{l}"));
        }
        if let Some(s) = a.get_short() {
            names.push(format!("-{s}"));
        }
        for n in names {
            if takes_val {
                val_opts.push(n.clone());
            }
            opts.push(n);
        }
    }
    (opts, val_opts)
}

fn resolve_point_byte(line: &str, point: usize) -> usize {
    line.char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(line.len()))
        .nth(point)
        .unwrap_or_else(|| {
            if line.is_char_boundary(point) {
                point
            } else {
                line.len()
            }
        })
}

fn format_shell_replacements(
    word: &CurrentWord,
    replacements: Vec<String>,
    shell: ShellKind,
) -> Vec<String> {
    match shell {
        ShellKind::Zsh | ShellKind::Fish => replacements
            .into_iter()
            .filter(|r| !r.chars().any(char::is_control))
            .collect(),
        ShellKind::Bash => bash_replies(word, &replacements),
    }
}

pub fn cli_structural_candidates_from_parsed(
    parsed: &ShellLine,
    shell: ShellKind,
    offset: usize,
    limit: usize,
) -> Option<Vec<String>> {
    if parsed.current.quote != ShellQuote::None
        || parsed.args.iter().any(|a| a == "--")
    {
        return None;
    }
    let (subcommands, global_opts, global_val_opts) = clap_metadata();
    let val_refs: Vec<&str> =
        global_val_opts.iter().map(AsRef::as_ref).collect();
    let top = positionals(parsed.args.get(1..)?, &val_refs)?;
    let prefix = parsed.current.text.as_str();
    let pool: Vec<String> =
        match (top.first().copied(), prefix.starts_with('-')) {
            (None, true) => global_opts
                .into_iter()
                .filter(|o| o.starts_with(prefix))
                .collect(),
            (None, false) => subcommands
                .into_iter()
                .filter(|s| s.starts_with(prefix))
                .collect(),
            (Some(sub), true) => {
                let (sub_opts, _) = clap_subcommand_opts(sub);
                sub_opts
                    .into_iter()
                    .filter(|o| o.starts_with(prefix))
                    .collect()
            }
            (Some(_), false) => return None,
        };
    let mut page: Vec<String> =
        pool.iter().skip(offset).take(limit).cloned().collect();
    if page.is_empty() && offset > 0 {
        page = pool.iter().take(limit).cloned().collect();
    }
    Some(format_shell_replacements(&parsed.current, page, shell))
}

pub fn evaluate_shell_completion(
    store_and_reg: Option<(&Store, &TagRegistry)>,
    line: &str,
    point: usize,
    wordbreaks: &str,
    shell: ShellKind,
    offset: usize,
    limit: usize,
) -> ShellCompletionOutcome {
    let point_byte = resolve_point_byte(line, point);
    let Some(head) = line.get(..point_byte) else {
        return ShellCompletionOutcome::Fallback;
    };
    let Some(parsed) = lex_shell_line(head, wordbreaks) else {
        return ShellCompletionOutcome::Fallback;
    };
    if let Some(cands) =
        cli_structural_candidates_from_parsed(&parsed, shell, offset, limit)
    {
        return ShellCompletionOutcome::Completed(cands);
    }
    let Some(_role) = query_arg_role(&parsed.args) else {
        return ShellCompletionOutcome::Fallback;
    };
    let Some((store, registry)) = store_and_reg else {
        return ShellCompletionOutcome::Completed(vec![]);
    };
    let cands = shell_candidates(
        store, registry, line, point, wordbreaks, shell, offset, limit,
    );
    ShellCompletionOutcome::Completed(cands)
}

pub fn shell_candidates(
    store: &Store,
    registry: &TagRegistry,
    line: &str,
    point: usize,
    wordbreaks: &str,
    shell: ShellKind,
    offset: usize,
    limit: usize,
) -> Vec<String> {
    let point_byte = resolve_point_byte(line, point);
    let Some(head) = line.get(..point_byte) else {
        return vec![];
    };
    let Some(parsed) = lex_shell_line(head, wordbreaks) else {
        return vec![];
    };
    if parsed.current.text.starts_with('-') {
        return vec![];
    }
    let Some(role) = query_arg_role(&parsed.args) else {
        return vec![];
    };
    let ttql = parsed.current.text.as_str();
    let mut cand_list =
        resolve_ttql_candidates(store, registry, ttql, role, offset, limit);
    if cand_list.is_empty() && offset > 0 {
        cand_list =
            resolve_ttql_candidates(store, registry, ttql, role, 0, limit);
    }
    let replacements: Vec<String> = cand_list
        .into_iter()
        .filter_map(|c| {
            ttql.get(..c.span.0).map(|head| format!("{head}{}", c.raw))
        })
        .collect();
    format_shell_replacements(&parsed.current, replacements, shell)
}

fn common_prefix<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let mut iter = items.into_iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    iter.fold(first.to_string(), |acc, s| {
        acc.chars()
            .zip(s.chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a)
            .collect()
    })
}

fn with_escape_safe_prefix(
    pairs: Vec<(&str, String)>,
    quote: ShellQuote,
) -> Vec<String> {
    let mut replies: Vec<String> =
        pairs.iter().map(|(_, q)| q.clone()).collect();
    if replies.len() < 2 {
        return replies;
    }
    let unquoted_lcp = common_prefix(pairs.iter().map(|(u, _)| *u));
    let Some(safe) = quote_for_shell(&unquoted_lcp, quote) else {
        return replies;
    };
    if common_prefix(replies.iter().map(String::as_str)) != safe
        && !replies.contains(&safe)
    {
        replies.insert(0, safe);
    }
    replies
}

fn bash_replies(word: &CurrentWord, replacements: &[String]) -> Vec<String> {
    let pairs: Vec<(&str, String)> = replacements
        .iter()
        .filter_map(|r| r.strip_prefix(word.text_before_rl.as_str()))
        .map(|r| match word.quote {
            ShellQuote::None => r.strip_suffix(' ').unwrap_or(r),
            _ => r,
        })
        .filter_map(|r| quote_for_shell(r, word.quote).map(|q| (r, q)))
        .collect();
    match (pairs.as_slice(), word.quote) {
        (_, ShellQuote::None) => with_escape_safe_prefix(pairs, word.quote),
        ([(_, only)], _) if *only == word.rl_text => vec![],
        ([(_, only)], _) => vec![only.clone(), format!("{only} ")],
        _ => with_escape_safe_prefix(pairs, word.quote),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WB_DEFAULT: &str = BASH_DEFAULT_WORDBREAKS;

    #[test]
    fn test_lex_single_quoted_spans_spaces() {
        let l = lex_shell_line("ttfm search 'a:x & pro", WB_DEFAULT).unwrap();
        assert_eq!(l.args, ["ttfm", "search"]);
        assert_eq!(l.current.text, "a:x & pro");
        assert_eq!(l.current.quote, ShellQuote::Single);
        assert_eq!(l.current.text_before_rl, "");
        assert_eq!(l.current.rl_text, "a:x & pro");
    }

    #[test]
    fn test_lex_unquoted_splits_rl_text_at_wordbreak() {
        let l = lex_shell_line("ttfm search filename:te", WB_DEFAULT).unwrap();
        assert_eq!(l.current.text, "filename:te");
        assert_eq!(l.current.quote, ShellQuote::None);
        assert_eq!(l.current.text_before_rl, "filename:");
        assert_eq!(l.current.rl_text, "te");
    }

    #[test]
    fn test_lex_reopened_single_quote_after_escaped_quote() {
        let l = lex_shell_line(r"ttfm search 'it'\''s pr", WB_DEFAULT).unwrap();
        assert_eq!(l.current.text, "it's pr");
        assert_eq!(l.current.quote, ShellQuote::Single);
        assert_eq!(l.current.text_before_rl, "it'");
    }

    #[test]
    fn test_lex_double_quote_unescapes() {
        let l = lex_shell_line(r#"ttfm search "a\$b\"c"#, WB_DEFAULT).unwrap();
        assert_eq!(l.current.text, "a$b\"c");
        assert_eq!(l.current.quote, ShellQuote::Double);
    }

    #[test]
    fn test_lex_rejects_expansions_and_operators() {
        for line in [
            "ttfm search a;b",
            "ttfm search a|b",
            "ttfm search $(x",
            "ttfm search `x",
            "ttfm search \"a$(x",
            "ttfm search 'a'b",
        ] {
            assert!(lex_shell_line(line, WB_DEFAULT).is_none(), "{line}");
        }
    }

    #[test]
    fn test_lex_empty_current_word() {
        let l = lex_shell_line("ttfm search ", WB_DEFAULT).unwrap();
        assert_eq!(l.args, ["ttfm", "search"]);
        assert_eq!(l.current.text, "");
        assert_eq!(l.current.quote, ShellQuote::None);
    }

    #[test]
    fn test_quote_for_shell_by_context() {
        assert_eq!(
            quote_for_shell("a b\"$(x)*", ShellQuote::None).unwrap(),
            r#"a\ b\"\$\(x\)\*"#
        );
        assert_eq!(
            quote_for_shell("it's", ShellQuote::Single).unwrap(),
            r"it'\''s"
        );
        assert_eq!(
            quote_for_shell("a\"$`\\!", ShellQuote::Double).unwrap(),
            r#"a\"\$\`\\"'!'""#
        );
        assert!(quote_for_shell("a\nb", ShellQuote::Single).is_none());
        assert!(quote_for_shell("a\tb", ShellQuote::None).is_none());
    }

    #[test]
    fn test_query_arg_role_by_positional_index() {
        let args = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(
            query_arg_role(&args("ttfm search")),
            Some(TtqlRole::Search)
        );
        assert_eq!(
            query_arg_role(&args("ttfm -q search -n 5")),
            Some(TtqlRole::Search)
        );
        assert_eq!(query_arg_role(&args("ttfm search q")), None);
        assert_eq!(
            query_arg_role(&args("ttfm tag q")),
            Some(TtqlRole::Edit { is_untag: false })
        );
        assert_eq!(
            query_arg_role(&args("ttfm untag --condition c q")),
            Some(TtqlRole::Edit { is_untag: true })
        );
        assert_eq!(query_arg_role(&args("ttfm index")), None);
        assert_eq!(query_arg_role(&args("ttfm --confirm")), None);
        assert_eq!(
            query_arg_role(&args("ttfm --skip-scope x search")),
            Some(TtqlRole::Search)
        );
        assert_eq!(
            query_arg_role(&args("ttfm tag --on-conflict skip q")),
            Some(TtqlRole::Edit { is_untag: false })
        );
        assert_eq!(
            query_arg_role(&args("ttfm tag --on-conflict=skip q")),
            Some(TtqlRole::Edit { is_untag: false })
        );
    }

    fn setup_test_store_with_files(
        files: &[&str],
    ) -> (tempfile::TempDir, Store, TagRegistry) {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("files");
        for name in files {
            let p = root.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, *name).unwrap();
        }
        let reg = TagRegistry::with_standard();
        let store = Store::open(base.join("db")).unwrap();
        let indexer = crate::indexing::Indexer::new(&store, &reg);
        indexer.initialize_tables().unwrap();
        indexer.run(&[&root], None, false).unwrap();
        (dir, store, reg)
    }

    #[test]
    fn test_shell_candidates_type_and_label_unquoted() {
        let (_dir, store, reg) = setup_test_store_with_files(&["main.rs"]);
        let types = shell_candidates(
            &store,
            &reg,
            "ttfm search ext",
            15,
            WB_DEFAULT,
            ShellKind::Bash,
            0,
            10,
        );
        assert!(types.iter().any(|t| t == "extension:"));
        let labels = shell_candidates(
            &store,
            &reg,
            "ttfm search extension:r",
            23,
            WB_DEFAULT,
            ShellKind::Bash,
            0,
            10,
        );
        assert!(labels.iter().any(|l| l == "rs"));
    }

    #[test]
    fn test_shell_candidates_inside_aggregate_single_quoted() {
        let (_dir, store, reg) = setup_test_store_with_files(&["main.rs"]);
        let line = "ttfm search 'count(";
        let cli = shell_candidates(
            &store,
            &reg,
            line,
            line.len(),
            WB_DEFAULT,
            ShellKind::Bash,
            0,
            50,
        );
        assert!(cli.iter().all(|c| c.starts_with("count(")), "{cli:?}");
        assert!(cli.iter().any(|c| c.ends_with(':')), "{cli:?}");
        assert!(cli.iter().any(|c| c.ends_with('(')), "{cli:?}");
        assert!(!cli.iter().any(|c| c == "count(random("), "{cli:?}");
    }

    #[test]
    fn test_shell_candidates_single_quoted_unique_is_doubled() {
        let (_dir, store, reg) = setup_test_store_with_files(&["main.rs"]);
        let line = "ttfm search 'extension:r";
        let cli = shell_candidates(
            &store,
            &reg,
            line,
            line.len(),
            WB_DEFAULT,
            ShellKind::Bash,
            0,
            10,
        );
        assert_eq!(cli, ["extension:rs ", "extension:rs  "]);
    }

    #[test]
    fn test_shell_candidates_zsh_returns_dequoted_argument() {
        let (_dir, store, reg) = setup_test_store_with_files(&["my file.txt"]);
        let line = "ttfm search 'filename:my";
        let cli = shell_candidates(
            &store,
            &reg,
            line,
            line.len(),
            "",
            ShellKind::Zsh,
            0,
            10,
        );
        assert_eq!(cli, ["filename:\"my file.txt\" "]);
    }

    #[test]
    fn test_shell_candidates_fish_returns_dequoted_argument() {
        let (_dir, store, reg) = setup_test_store_with_files(&["my file.txt"]);
        let line = "ttfm search 'filename:my";
        let cli = shell_candidates(
            &store,
            &reg,
            line,
            line.len(),
            "",
            ShellKind::Fish,
            0,
            10,
        );
        assert_eq!(cli, ["filename:\"my file.txt\" "]);
    }

    #[test]
    fn test_shell_candidates_rejects_non_char_boundary_point() {
        let (_dir, store, reg) = setup_test_store_with_files(&["main.rs"]);
        let line = "ttfm search 'extension:あ";
        assert!(shell_candidates(
            &store,
            &reg,
            line,
            line.len() - 1,
            WB_DEFAULT,
            ShellKind::Bash,
            0,
            10,
        )
        .is_empty());
    }

    #[test]
    fn test_shell_candidates_cycles_offset_back_to_zero_when_empty() {
        let (_dir, store, reg) = setup_test_store_with_files(&["main.rs"]);
        let line = "ttfm search extension:r";
        let first = shell_candidates(
            &store,
            &reg,
            line,
            line.len(),
            WB_DEFAULT,
            ShellKind::Bash,
            0,
            10,
        );
        assert!(!first.is_empty());
        let cycled = shell_candidates(
            &store,
            &reg,
            line,
            line.len(),
            WB_DEFAULT,
            ShellKind::Bash,
            10,
            10,
        );
        assert_eq!(cycled, first);
    }

    #[test]
    fn test_shell_candidates_projection_operators_fit_in_default_limit() {
        let (_dir, store, reg) = setup_test_store_with_files(&["main.rs"]);
        let line = "ttfm search 'size: ";
        let cands = shell_candidates(
            &store,
            &reg,
            line,
            line.len(),
            WB_DEFAULT,
            ShellKind::Bash,
            0,
            DEFAULT_SHELL_COMPLETION_LIMIT,
        );
        assert!(
            cands.iter().any(|c| c.contains(":^=")),
            "expected :^= in {cands:?}"
        );
        assert!(
            cands.iter().any(|c| c.contains('+')),
            "expected + in {cands:?}"
        );
        assert!(
            cands.iter().any(|c| c.contains('*')),
            "expected * in {cands:?}"
        );
        assert!(
            cands.iter().any(|c| c.contains('/')),
            "expected / in {cands:?}"
        );
        assert!(
            cands.iter().any(|c| c.contains('%')),
            "expected % in {cands:?}"
        );
    }

    fn lcp(replies: &[String]) -> String {
        let first = replies.first().cloned().unwrap_or_default();
        first
            .chars()
            .enumerate()
            .take_while(|(i, c)| {
                replies.iter().all(|r| r.chars().nth(*i) == Some(*c))
            })
            .map(|(_, c)| c)
            .collect()
    }

    fn unquoted_word(text: &str, before_rl: &str) -> CurrentWord {
        CurrentWord {
            text: text.to_string(),
            quote: ShellQuote::None,
            text_before_rl: before_rl.to_string(),
            rl_text: text[before_rl.len()..].to_string(),
        }
    }

    #[test]
    fn test_bash_replies_common_prefix_never_ends_inside_escape() {
        let date = bash_replies(
            &unquoted_word("mtime:2026-10-26", "mtime:"),
            &[
                r#"mtime:"2026-10-26""#.to_string(),
                r#"mtime:"2026-10-26 12:36""#.to_string(),
                r#"mtime:"2026-10-26 12:36:15""#.to_string(),
            ],
        );
        assert_eq!(lcp(&date), r#"\"2026-10-26"#);
        assert!(date.contains(&r#"\"2026-10-26\ 12:36\""#.to_string()));

        let memo = bash_replies(
            &unquoted_word("memo:a", "memo:"),
            &[r#"memo:"a b""#.to_string(), r#"memo:"a\"b""#.to_string()],
        );
        assert_eq!(lcp(&memo), r#"\"a"#);
    }
}
