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

use crate::db::{Col, Store, Tbl};
use crate::tag::TagRegistry;
use anyhow::Result;
use sea_query::{PostgresQueryBuilder, Query};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtqlRole {
    Search,
    Edit { is_untag: bool },
}

pub fn is_inside_quotes(sub: &str) -> bool {
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    for c in sub.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
        } else if c == '"' || c == '\'' {
            in_quote = Some(c);
        }
    }
    in_quote.is_some()
}

pub fn tokenize_query(input: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = None;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            if start.is_none() {
                start = Some(i);
            }
            continue;
        }
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            in_quote = Some(c);
            if start.is_none() {
                start = Some(i);
            }
            continue;
        }
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                tokens.push(&input[s..i]);
            }
            continue;
        }
        if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        tokens.push(&input[s..]);
    }
    tokens
}

fn unquote_prefix(raw: &str) -> &str {
    let mut chars = raw.chars();
    let Some(first) = chars.next() else {
        return raw;
    };
    if first != '"' && first != '\'' {
        return raw;
    }
    let rest = &raw[first.len_utf8()..];
    rest.strip_suffix(first).unwrap_or(rest)
}

fn is_operator_token(tok: &str) -> bool {
    matches!(
        tok,
        "&" | "|"
            | "-"
            | "-:"
            | "&:"
            | "("
            | ")"
            | ":>"
            | ":<"
            | ":="
            | ":>="
            | ":<="
            | ":^"
            | ":^="
            | "+"
            | "*"
            | "/"
            | "%"
    )
}

fn is_label_comparison_op(tok: &str) -> bool {
    matches!(tok, ":>" | ":<" | ":=" | ":>=" | ":<=" | ":^" | ":^=")
}

fn projection_tag_type(tok: &str) -> Option<&str> {
    let t = unquote_prefix(tok);
    if is_operator_token(t) || !t.ends_with(':') {
        return None;
    }
    let name = &t[..t.len() - 1];
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn strip_stuck_comparison(prefix: &str) -> &str {
    const OPS: &[&str] = &[">=", "<=", "^=", ">", "<", "=", "^"];
    for op in OPS {
        if let Some(rest) = prefix.strip_prefix(op) {
            return rest;
        }
    }
    prefix
}

fn split_leading_label_comparison(tok: &str) -> Option<&str> {
    const OPS: &[&str] = &[":>=", ":<=", ":^=", ":>", ":<", ":=", ":^"];
    for op in OPS {
        if let Some(rest) = tok.strip_prefix(op) {
            return Some(rest);
        }
    }
    None
}

fn query_has_projection(query_sub: &str) -> bool {
    tokenize_query(query_sub).iter().any(|tok| {
        let t = unquote_prefix(tok);
        t.contains(':') && !is_operator_token(t)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionContext {
    ExpressionStart {
        prefix: String,
        is_outermost: bool,
        span: (usize, usize),
    },
    TypeColon {
        tag_type: String,
        prefix: String,
        span: (usize, usize),
    },
    TypeWithSpace {
        tag_type: String,
        prefix: String,
        span: (usize, usize),
    },
    TermFinished {
        prefix: String,
        is_projection: bool,
        span: (usize, usize),
    },
    EditQuery {
        tag_type: String,
        prefix: String,
        has_colon: bool,
        in_quotes: bool,
        is_untag: bool,
        span: (usize, usize),
    },
    IndexPath {
        path_prefix: String,
        span: (usize, usize),
    },
    None,
}

fn detect_command_and_arg_start(raw_line: &str) -> Option<(&str, usize)> {
    let trimmed = raw_line.trim_start();
    let without_bin = trimmed
        .strip_prefix("ttfm ")
        .unwrap_or(trimmed)
        .trim_start();
    let bin_offset = raw_line.len() - without_bin.len();

    const COMMAND_PREFIXES: &[(&str, &str)] = &[
        ("search ", "s"),
        ("s ", "s"),
        ("tag ", "t"),
        ("t ", "t"),
        ("untag ", "u"),
        ("u ", "u"),
        ("index ", "i"),
        ("i ", "i"),
    ];

    for &(prefix, cmd) in COMMAND_PREFIXES {
        if without_bin.starts_with(prefix) {
            return Some((cmd, bin_offset + prefix.len()));
        }
    }
    None
}

fn parse_index_context(
    raw_line: &str,
    arg_start: usize,
    pos: usize,
) -> CompletionContext {
    let sub = &raw_line[arg_start..pos];
    let open_quote = sub.matches('"').count() % 2 == 1;
    let in_word =
        open_quote || !(sub.is_empty() || sub.ends_with(char::is_whitespace));
    match (in_word, crate::util::split_path_words(sub).pop()) {
        (true, Some(w)) => CompletionContext::IndexPath {
            path_prefix: w.text,
            span: (arg_start + w.start, pos),
        },
        _ => CompletionContext::IndexPath {
            path_prefix: String::new(),
            span: (pos, pos),
        },
    }
}

fn parse_edit_args_context(
    edit_sub: &str,
    _base: usize,
    pos: usize,
    is_untag: bool,
) -> CompletionContext {
    let (has_colon, tag_type, prefix, span_start) =
        if edit_sub.ends_with(' ') || edit_sub.is_empty() {
            (false, String::new(), String::new(), pos)
        } else {
            let tok = tokenize_query(edit_sub).last().copied().unwrap_or("");
            match tok.split_once(':') {
                Some((tag, val)) => {
                    let prefix = strip_stuck_comparison(unquote_prefix(val));
                    (
                        true,
                        unquote_prefix(tag).to_string(),
                        prefix.to_string(),
                        pos - prefix.len(),
                    )
                }
                None => {
                    let prefix = unquote_prefix(tok);
                    (
                        false,
                        prefix.to_string(),
                        prefix.to_string(),
                        pos - prefix.len(),
                    )
                }
            }
        };

    CompletionContext::EditQuery {
        tag_type,
        prefix,
        has_colon,
        in_quotes: false,
        is_untag,
        span: (span_start, pos),
    }
}

fn parse_edit_context(
    sub: &str,
    arg_start: usize,
    pos: usize,
    is_untag: bool,
) -> Option<CompletionContext> {
    let args_sub = &sub[arg_start..];
    let quote_count = args_sub.chars().filter(|&c| c == '"').count();
    let is_after_search_query = quote_count >= 2 && quote_count % 2 == 0;
    if !is_after_search_query {
        return None;
    }

    let last_quote_idx = sub.rfind('"')?;
    Some(parse_edit_args_context(
        &sub[last_quote_idx + 1..],
        last_quote_idx + 1,
        pos,
        is_untag,
    ))
}

fn is_outermost_scope(query_sub: &str) -> bool {
    let mut depth = 0;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    for c in query_sub.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => in_quote = Some(c),
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ => {}
        }
    }
    depth == 0
}

fn suffix_after_last_expression_paren(token: &str) -> Option<&str> {
    let mut last = None;
    let mut after_colon = false;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in token.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if let Some(q) = in_quote {
            if c == q {
                in_quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => in_quote = Some(c),
            ':' => after_colon = true,
            '(' if !after_colon => last = Some(i),
            _ => {}
        }
    }
    last.map(|i| &token[i + '('.len_utf8()..])
}

fn label_context_after_comparison(
    tokens: &[&str],
    current_prefix: &str,
    pos: usize,
    is_outermost: bool,
) -> Option<CompletionContext> {
    let op = tokens.last().copied().unwrap_or("");
    if !is_label_comparison_op(op) {
        return None;
    }
    let prev = if tokens.len() >= 2 {
        tokens[tokens.len() - 2]
    } else {
        ""
    };
    if let Some(tag) = projection_tag_type(prev) {
        let prefix = current_prefix.to_string();
        let span = (pos - prefix.len(), pos);
        return Some(CompletionContext::TypeColon {
            tag_type: unquote_prefix(tag).to_string(),
            prefix,
            span,
        });
    }
    let prefix = current_prefix.to_string();
    let span = (pos - prefix.len(), pos);
    Some(CompletionContext::ExpressionStart {
        prefix,
        is_outermost,
        span,
    })
}

fn parse_spaced_query_context(
    query_sub: &str,
    trimmed: &str,
    pos: usize,
    is_outermost: bool,
) -> CompletionContext {
    let tokens = tokenize_query(trimmed);
    let last_token = tokens.last().copied().unwrap_or("");
    if let Some(inner) = suffix_after_last_expression_paren(last_token) {
        return parse_spaced_query_context(
            inner,
            inner.trim_end_matches(' '),
            pos,
            false,
        );
    }
    if let Some(ctx) =
        label_context_after_comparison(&tokens, "", pos, is_outermost)
    {
        return ctx;
    }
    match last_token {
        "" | "&" | "|" | "-" | "-:" | "&:" | "(" => {
            CompletionContext::ExpressionStart {
                prefix: String::new(),
                is_outermost,
                span: (pos, pos),
            }
        }
        token if token.ends_with(':') => CompletionContext::TypeWithSpace {
            tag_type: unquote_prefix(&token[..token.len() - 1]).to_string(),
            prefix: String::new(),
            span: (pos, pos),
        },
        _ => CompletionContext::TermFinished {
            prefix: String::new(),
            is_projection: query_has_projection(query_sub),
            span: (pos, pos),
        },
    }
}

fn parse_token_query_context(
    query_sub: &str,
    pos: usize,
    is_outermost: bool,
) -> CompletionContext {
    let tokens = tokenize_query(query_sub);
    let current = tokens.last().copied().unwrap_or("");
    if let Some(inner) = suffix_after_last_expression_paren(current) {
        return parse_token_query_context(inner, pos, false);
    }
    let prev = if tokens.len() >= 2 {
        Some(tokens[tokens.len() - 2])
    } else {
        None
    };
    let span = (pos - current.len(), pos);

    if current.is_empty() {
        return CompletionContext::ExpressionStart {
            prefix: String::new(),
            is_outermost,
            span: (pos, pos),
        };
    }

    if let Some(rest) = split_leading_label_comparison(current) {
        if let Some(tag) = prev.and_then(projection_tag_type) {
            if rest.is_empty() {
                return CompletionContext::TypeWithSpace {
                    tag_type: unquote_prefix(tag).to_string(),
                    prefix: current.to_string(),
                    span,
                };
            }
            let prefix = unquote_prefix(rest);
            return CompletionContext::TypeColon {
                tag_type: unquote_prefix(tag).to_string(),
                prefix: prefix.to_string(),
                span: (pos - prefix.len(), pos),
            };
        }
    }

    if let Some((tag_type, prefix_raw)) = current.split_once(':') {
        let tag_type = unquote_prefix(tag_type);
        if !tag_type.is_empty() {
            let prefix = strip_stuck_comparison(unquote_prefix(prefix_raw));
            return CompletionContext::TypeColon {
                tag_type: tag_type.to_string(),
                prefix: prefix.to_string(),
                span: (pos - prefix.len(), pos),
            };
        }
    }

    if let Some(op) = prev {
        if is_label_comparison_op(op) {
            let before = if tokens.len() >= 3 {
                tokens[tokens.len() - 3]
            } else {
                ""
            };
            if let Some(ctx) = label_context_after_comparison(
                &[before, op],
                current,
                pos,
                is_outermost,
            ) {
                return ctx;
            }
        }
    }

    match prev {
        Some(token) if token.ends_with(':') => {
            CompletionContext::TypeWithSpace {
                tag_type: unquote_prefix(&token[..token.len() - 1]).to_string(),
                prefix: current.to_string(),
                span,
            }
        }
        Some("&" | "|" | "-" | "-:" | "&:" | "(") | None => {
            CompletionContext::ExpressionStart {
                prefix: current.to_string(),
                is_outermost,
                span,
            }
        }
        Some(_) => CompletionContext::TermFinished {
            prefix: current.to_string(),
            is_projection: query_has_projection(query_sub),
            span,
        },
    }
}

fn parse_query_context(query_sub: &str, pos: usize) -> CompletionContext {
    let is_outermost = is_outermost_scope(query_sub);
    let trimmed = query_sub.trim_end_matches(' ');
    let has_trailing_space = query_sub.len() > trimmed.len();

    if has_trailing_space {
        parse_spaced_query_context(query_sub, trimmed, pos, is_outermost)
    } else {
        parse_token_query_context(query_sub, pos, is_outermost)
    }
}

pub fn parse_completion_context(
    raw_line: &str,
    raw_pos: usize,
) -> CompletionContext {
    let Some((cmd, arg_start)) = detect_command_and_arg_start(raw_line) else {
        return CompletionContext::None;
    };

    let pos = raw_pos.min(raw_line.len());
    if pos < arg_start {
        return CompletionContext::None;
    }

    if cmd == "i" {
        return parse_index_context(raw_line, arg_start, pos);
    }

    let sub = &raw_line[..pos];
    if matches!(cmd, "t" | "u") {
        if let Some(ctx) = parse_edit_context(sub, arg_start, pos, cmd == "u") {
            return ctx;
        }
    }

    let query_sub =
        if matches!(cmd, "t" | "u") && sub[arg_start..].starts_with('"') {
            &sub[arg_start + 1..]
        } else {
            &sub[arg_start..]
        };

    parse_query_context(query_sub, pos)
}

pub fn parse_ttql_context(ttql: &str, role: TtqlRole) -> CompletionContext {
    match role {
        TtqlRole::Search => parse_query_context(ttql, ttql.len()),
        TtqlRole::Edit { is_untag } => {
            parse_edit_args_context(ttql, 0, ttql.len(), is_untag)
        }
    }
}

pub fn fetch_type_and_func_candidates(
    store: &Store,
    reg: &TagRegistry,
    prefix: &str,
    is_outermost: bool,
) -> Result<Vec<String>> {
    if prefix == "*" {
        return Ok(vec!["*".to_string()]);
    }
    let mut types =
        search_short_names(store, reg, &definition_type_query(prefix), 0, None)
            .unwrap_or_default();

    for name in reg.get_excluded_from_tags() {
        if !types.contains(&name) {
            types.push(name);
        }
    }
    for f in reg.iter_arcs() {
        let name = f.name().to_string();
        if !types.contains(&name) {
            types.push(name);
        }
    }
    if !types.iter().any(|t| t == "*") {
        types.push("*".to_string());
    }

    let mut funcs = vec!["count", "sum", "avg", "max", "min", "q"];
    if is_outermost {
        funcs.push("random");
    }

    types.sort();
    types.dedup();

    let all = [types, funcs.iter().map(|s| s.to_string()).collect()].concat();
    Ok(all.into_iter().filter(|t| t.starts_with(prefix)).collect())
}

pub fn sanitize_tag_type_for_path(tag_type: &str) -> Option<&str> {
    if tag_type.is_empty()
        || tag_type.contains('/')
        || tag_type.contains('\\')
        || tag_type.contains("..")
    {
        None
    } else {
        Some(tag_type)
    }
}

pub fn fetch_tag_candidates(
    store: &Store,
    registry: &TagRegistry,
    tag_type: &str,
    prefix: &str,
    offset: usize,
    limit: usize,
) -> Result<Vec<String>> {
    let safe_type = match sanitize_tag_type_for_path(tag_type) {
        Some(t) => t,
        None => return Ok(vec![]),
    };

    if let Some(func) = registry.get(safe_type) {
        let suggestions = func.format_suggestions();
        if !suggestions.is_empty() {
            return Ok(suggestions
                .into_iter()
                .filter(|s| s.starts_with(prefix))
                .skip(offset)
                .take(limit)
                .collect());
        }
    }

    if safe_type == "type" {
        return search_short_names(
            store,
            registry,
            &definition_type_query(prefix),
            offset,
            Some(limit),
        );
    }

    let query_str = definition_label_query(safe_type, prefix);
    let names =
        search_short_names(store, registry, &query_str, offset, Some(limit))?;
    Ok(names
        .into_iter()
        .filter_map(|n| strip_type_prefix(&n, safe_type))
        .collect())
}

pub fn fetch_allowed_edit_types(
    store: &Store,
    reg: &TagRegistry,
    prefix: &str,
    is_untag: bool,
) -> Vec<String> {
    let mut types = Vec::new();
    for origin in
        crate::query::lens_resolver::DefinitionStorage::from_store(store)
            .valid_origins()
    {
        let Ok(entries) = std::fs::read_dir(store.tags_origin_dir(origin))
        else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if let Some(t) = name.strip_prefix("type=") {
                        if !types.iter().any(|x| x == t) {
                            types.push(t.to_string());
                        }
                    }
                }
            }
        }
    }

    for (tag_type, _) in reg.type_configs().iter() {
        let name = tag_type.as_str().to_string();
        if !types.contains(&name) {
            types.push(name);
        }
    }

    let ut_path = store.path_for_target(crate::db::TargetTable::UserTags);
    if ut_path.exists() {
        let ut_sub = crate::util::parquet_query(&ut_path.to_string_lossy());
        let mut q = Query::select();
        q.distinct()
            .column((Tbl::UserTags, Col::Type))
            .from_subquery(ut_sub, Tbl::UserTags);
        let sql = q.to_string(PostgresQueryBuilder);
        if let Ok(mut stmt) = store.conn.prepare(&sql) {
            if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0))
            {
                for r in rows.flatten() {
                    if !types.contains(&r) {
                        types.push(r);
                    }
                }
            }
        }
    }

    if types.is_empty() {
        let mut q = Query::select();
        q.distinct()
            .column((Tbl::OneView, Col::Type))
            .from(Tbl::OneView);
        let sql = q.to_string(PostgresQueryBuilder);
        if let Ok(mut stmt) = store.conn.prepare(&sql) {
            if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0))
            {
                for r in rows.flatten() {
                    if !types.contains(&r) {
                        types.push(r);
                    }
                }
            }
        }
    }

    for f in reg.iter_arcs() {
        let name = f.name().to_string();
        if !types.contains(&name) {
            types.push(name);
        }
    }

    let mut allowed = Vec::new();
    for t in types {
        if !t.starts_with(prefix) {
            continue;
        }
        let is_allowed = if let Some(func) = reg.get(&t) {
            if let Some(edit) = func.edit() {
                if is_untag {
                    edit.can_untag()
                } else {
                    matches!(
                        edit.strategy(),
                        crate::tag::EditStrategy::Append
                            | crate::tag::EditStrategy::Replace
                    )
                }
            } else {
                false
            }
        } else {
            true
        };

        if is_allowed {
            allowed.push(t);
        }
    }

    allowed.sort();
    allowed.dedup();
    allowed
}

pub fn fetch_operator_candidates(ctx: &CompletionContext) -> Vec<String> {
    match ctx {
        CompletionContext::TypeWithSpace { prefix, .. } => vec![
            "&", "|", "-", "-:", "&:", ":>", ":<", ":=", ":>=", ":<=", ":^",
            ":^=", "+", "*", "/", "%",
        ]
        .into_iter()
        .map(String::from)
        .filter(|op| op.starts_with(prefix))
        .collect(),
        CompletionContext::TermFinished {
            prefix,
            is_projection,
            ..
        } => {
            let mut ops = vec!["&", "|", "-", "&:"];
            if !is_projection {
                ops.retain(|&o| o != "&:");
            }
            ops.into_iter()
                .map(String::from)
                .filter(|op| op.starts_with(prefix))
                .collect()
        }
        _ => vec![],
    }
}

pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!(
                "{}/{}",
                home.to_string_lossy().trim_end_matches('/'),
                rest
            );
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().to_string();
        }
    }
    path.to_string()
}

pub fn fetch_path_candidates(path_prefix: &str) -> Vec<String> {
    let expanded = expand_tilde(path_prefix);
    let p = std::path::Path::new(&expanded);
    let (parent, file_prefix) =
        if path_prefix.ends_with('/') || path_prefix.ends_with('\\') {
            (p, "")
        } else {
            (
                p.parent().unwrap_or_else(|| std::path::Path::new(".")),
                p.file_name().and_then(|n| n.to_str()).unwrap_or(""),
            )
        };
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(file_prefix) {
                let is_dir = entry.path().is_dir();
                let dir_str = if parent == std::path::Path::new(".")
                    && !path_prefix.starts_with("./")
                {
                    String::new()
                } else {
                    let mut s = parent.display().to_string();
                    if !s.ends_with('/') {
                        s.push('/');
                    }
                    s
                };
                let suffix = if is_dir { "/" } else { "" };
                out.push(format!("{dir_str}{name}{suffix}"));
            }
        }
    }
    out.sort();
    out
}

pub fn format_index_path(p: &str) -> String {
    let spaced = p.chars().any(char::is_whitespace);
    match (p.ends_with('/'), spaced) {
        (true, true) => format!("\"{p}"),
        (true, false) => p.to_string(),
        (false, true) => format!("\"{p}\" "),
        (false, false) => format!("{p} "),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    PastQuery,
    Type,
    Function,
    Label,
    Format,
    Operator,
    FilePath,
    CloseParen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub raw: String,
    pub kind: CandidateKind,
    pub span: (usize, usize),
}

pub fn escape_ttql_special_chars(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        if matches!(
            c,
            '\\' | '"'
                | '['
                | ']'
                | '*'
                | '?'
                | '^'
                | '>'
                | '<'
                | '='
                | '{'
                | '}'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn escape_in_quote_glob(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        if matches!(c, '\\' | '"' | '*' | '?' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn unescape_completion_prefix(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(n) => out.push(n),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn definition_label_query(tag_type: &str, prefix: &str) -> String {
    format!(
        "tag:\"{}:{}*\"",
        escape_in_quote_glob(tag_type),
        escape_in_quote_glob(&unescape_completion_prefix(prefix))
    )
}

pub fn definition_type_query(prefix: &str) -> String {
    format!(
        "type:\"{}*\"",
        escape_in_quote_glob(&unescape_completion_prefix(prefix))
    )
}

pub fn strip_type_prefix(name: &str, tag_type: &str) -> Option<String> {
    name.strip_prefix(&format!("{tag_type}:"))
        .map(str::to_string)
}

fn search_short_names(
    store: &Store,
    registry: &TagRegistry,
    query: &str,
    offset: usize,
    limit: Option<usize>,
) -> Result<Vec<String>> {
    let options = crate::search::SearchOptions {
        n: limit,
        offset: Some(offset),
        short: true,
        cache: false,
        ..Default::default()
    };
    let mut sink = Vec::new();
    let resp = crate::search::search_short(
        store, registry, query, options, &mut sink,
    )?;
    Ok(resp
        .results
        .into_iter()
        .filter_map(|item| item.representative.tags.first().map(|t| t.as_str()))
        .collect())
}

pub fn apply_smart_suffix(
    raw: &str,
    kind: CandidateKind,
    in_quotes: bool,
) -> String {
    match kind {
        CandidateKind::PastQuery => raw.to_string(),
        CandidateKind::Type => format!("{raw}:"),
        CandidateKind::Function => format!("{raw}("),
        CandidateKind::FilePath => raw.to_string(),
        CandidateKind::CloseParen => format!("{raw} "),
        CandidateKind::Label | CandidateKind::Format => {
            let escaped = escape_ttql_special_chars(raw);
            let forbidden = [
                ':', '&', '|', '(', ')', '"', '\'', '=', '>', '<', '^', ',',
                ';', '{', '}', ' ',
            ];
            if in_quotes {
                format!("{escaped}\" ")
            } else if raw.chars().any(|c| forbidden.contains(&c))
                && !raw.starts_with('"')
            {
                format!("\"{escaped}\" ")
            } else {
                format!("{escaped} ")
            }
        }
        CandidateKind::Operator => format!("{raw} "),
    }
}

pub fn unique_ghost_suffix(
    line: &str,
    pos: usize,
    candidates: &[Candidate],
) -> Option<String> {
    let [only] = candidates else {
        return None;
    };
    if pos != only.span.1 {
        return None;
    }
    let start = only.span.0;
    let end = only.span.1;
    if start > end
        || end > line.len()
        || !line.is_char_boundary(start)
        || !line.is_char_boundary(end)
    {
        return None;
    }
    let typed = &line[start..end];
    let suffix = only.raw.strip_prefix(typed)?;
    if suffix.is_empty() {
        None
    } else {
        Some(suffix.to_string())
    }
}

fn candidates_for_context(
    store: &Store,
    registry: &TagRegistry,
    ctx: CompletionContext,
    in_quotes: bool,
    offset: usize,
    limit: usize,
) -> Vec<Candidate> {
    match ctx {
        CompletionContext::ExpressionStart {
            prefix,
            is_outermost,
            span,
        } => {
            let types = fetch_type_and_func_candidates(
                store,
                registry,
                &prefix,
                is_outermost,
            )
            .unwrap_or_default();
            types
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|t| {
                    let kind = match t.as_str() {
                        "count" | "sum" | "avg" | "max" | "min" | "q"
                        | "random" => CandidateKind::Function,
                        _ => CandidateKind::Type,
                    };
                    let raw = apply_smart_suffix(&t, kind, in_quotes);
                    Candidate { raw, kind, span }
                })
                .collect()
        }
        CompletionContext::TypeColon {
            tag_type,
            prefix,
            span,
        } => {
            let tags = fetch_tag_candidates(
                store, registry, &tag_type, &prefix, offset, limit,
            )
            .unwrap_or_default();
            let kind = if registry
                .get(&tag_type)
                .map(|f| !f.format_suggestions().is_empty())
                .unwrap_or(false)
            {
                CandidateKind::Format
            } else {
                CandidateKind::Label
            };
            tags.into_iter()
                .map(|t| {
                    let raw = apply_smart_suffix(&t, kind, in_quotes);
                    Candidate { raw, kind, span }
                })
                .collect()
        }
        CompletionContext::TypeWithSpace { span, .. }
        | CompletionContext::TermFinished { span, .. } => {
            let ops = fetch_operator_candidates(&ctx);
            ops.into_iter()
                .skip(offset)
                .take(limit)
                .map(|o| {
                    let raw = apply_smart_suffix(
                        &o,
                        CandidateKind::Operator,
                        in_quotes,
                    );
                    Candidate {
                        raw,
                        kind: CandidateKind::Operator,
                        span,
                    }
                })
                .collect()
        }
        CompletionContext::IndexPath { path_prefix, span } => {
            fetch_path_candidates(&path_prefix)
                .into_iter()
                .filter(|p| !p.contains('"'))
                .skip(offset)
                .take(limit)
                .map(|p| Candidate {
                    raw: format_index_path(&p),
                    kind: CandidateKind::FilePath,
                    span,
                })
                .collect()
        }
        CompletionContext::EditQuery {
            tag_type,
            prefix,
            has_colon,
            is_untag,
            span,
            ..
        } => {
            if has_colon {
                let tags = fetch_tag_candidates(
                    store, registry, &tag_type, &prefix, offset, limit,
                )
                .unwrap_or_default();
                let kind = if registry
                    .get(&tag_type)
                    .map(|f| !f.format_suggestions().is_empty())
                    .unwrap_or(false)
                {
                    CandidateKind::Format
                } else {
                    CandidateKind::Label
                };
                tags.into_iter()
                    .map(|t| {
                        let raw = apply_smart_suffix(&t, kind, in_quotes);
                        Candidate { raw, kind, span }
                    })
                    .collect()
            } else {
                let types = fetch_allowed_edit_types(
                    store, registry, &prefix, is_untag,
                );
                types
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .map(|t| {
                        let raw = apply_smart_suffix(
                            &t,
                            CandidateKind::Type,
                            in_quotes,
                        );
                        Candidate {
                            raw,
                            kind: CandidateKind::Type,
                            span,
                        }
                    })
                    .collect()
            }
        }
        CompletionContext::None => vec![],
    }
}

pub fn resolve_completion_candidates(
    store: &Store,
    registry: &TagRegistry,
    line: &str,
    pos: usize,
    offset: usize,
    limit: usize,
) -> Vec<Candidate> {
    let ctx = parse_completion_context(line, pos);
    let in_quotes = is_inside_quotes(&line[..pos.min(line.len())]);
    candidates_for_context(store, registry, ctx, in_quotes, offset, limit)
}

pub fn resolve_ttql_candidates(
    store: &Store,
    registry: &TagRegistry,
    ttql: &str,
    role: TtqlRole,
    offset: usize,
    limit: usize,
) -> Vec<Candidate> {
    let ctx = parse_ttql_context(ttql, role);
    candidates_for_context(
        store,
        registry,
        ctx,
        is_inside_quotes(ttql),
        offset,
        limit,
    )
}

pub fn fetch_candidates(
    store: &Store,
    registry: &TagRegistry,
    line: &str,
    pos: usize,
    offset: usize,
    limit: usize,
) -> Vec<Candidate> {
    resolve_completion_candidates(store, registry, line, pos, offset, limit)
}

#[cfg(test)]
mod lexer_tests {
    use super::*;

    #[test]
    fn test_context_expression_start() {
        let ctx = parse_completion_context("s su", 4);
        assert!(
            matches!(ctx, CompletionContext::ExpressionStart { prefix, is_outermost, span } if prefix == "su" && is_outermost && span == (2, 4))
        );
    }

    #[test]
    fn test_context_inside_aggregate_function_is_expression_start() {
        for line in ["s sum(", "s count(", "s avg(", "s max(", "s min(", "s q("]
        {
            let ctx = parse_completion_context(line, line.len());
            assert!(
                matches!(
                    ctx,
                    CompletionContext::ExpressionStart {
                        ref prefix,
                        is_outermost,
                        span,
                    } if prefix.is_empty()
                        && !is_outermost
                        && span == (line.len(), line.len())
                ),
                "{line} => {ctx:?}"
            );
        }

        let closed = "s sum()";
        let pos = closed.find(')').unwrap();
        let ctx = parse_completion_context(closed, pos);
        assert!(
            matches!(
                ctx,
                CompletionContext::ExpressionStart {
                    ref prefix,
                    is_outermost,
                    span,
                } if prefix.is_empty() && !is_outermost && span == (pos, pos)
            ),
            "{ctx:?}"
        );

        let prefixed = "s sum(si";
        let ctx = parse_completion_context(prefixed, prefixed.len());
        assert!(
            matches!(
                ctx,
                CompletionContext::ExpressionStart {
                    ref prefix,
                    is_outermost,
                    span,
                } if prefix == "si"
                    && !is_outermost
                    && span == (prefixed.len() - 2, prefixed.len())
            ),
            "{ctx:?}"
        );

        let typed = "s sum(size:";
        let ctx = parse_completion_context(typed, typed.len());
        assert!(
            matches!(
                ctx,
                CompletionContext::TypeColon {
                    ref tag_type,
                    ref prefix,
                    span,
                } if tag_type == "size"
                    && prefix.is_empty()
                    && span == (typed.len(), typed.len())
            ),
            "{ctx:?}"
        );

        let label_paren = "s name:foo(bar";
        let ctx = parse_completion_context(label_paren, label_paren.len());
        assert!(
            matches!(
                ctx,
                CompletionContext::TypeColon {
                    ref tag_type,
                    ref prefix,
                    ..
                } if tag_type == "name" && prefix == "foo(bar"
            ),
            "{ctx:?}"
        );
    }

    #[test]
    fn test_context_type_colon() {
        let ctx = parse_completion_context("s extension:r", 13);
        assert!(
            matches!(ctx, CompletionContext::TypeColon { tag_type, prefix, span } if tag_type == "extension" && prefix == "r" && span == (12, 13))
        );
    }

    #[test]
    fn test_context_type_with_space() {
        let ctx = parse_completion_context("s size: ", 8);
        assert!(
            matches!(ctx, CompletionContext::TypeWithSpace { tag_type, prefix, span } if tag_type == "size" && prefix.is_empty() && span == (8, 8))
        );
    }

    #[test]
    fn test_context_term_finished() {
        let ctx = parse_completion_context("s project:ttfm ", 15);
        assert!(
            matches!(ctx, CompletionContext::TermFinished { prefix, is_projection, span } if prefix.is_empty() && is_projection && span == (15, 15))
        );
    }

    #[test]
    fn test_context_spaced_label_comparison_opens_label() {
        let spaced = [
            ("s tag: :> ", "tag"),
            ("s tag: :< ", "tag"),
            ("s tag: := ", "tag"),
            ("s tag: :>= ", "tag"),
            ("s tag: :<= ", "tag"),
            ("s tag: :^ ", "tag"),
            ("s size: :^= ", "size"),
        ];
        for (line, tag) in spaced {
            let ctx = parse_completion_context(line, line.len());
            assert!(
                matches!(
                    ctx,
                    CompletionContext::TypeColon {
                        ref tag_type,
                        ref prefix,
                        span,
                    } if tag_type == tag
                        && prefix.is_empty()
                        && span == (line.len(), line.len())
                ),
                "{line} => {ctx:?}"
            );
        }
        let line = "s size: :>= 10";
        let ctx = parse_completion_context(line, line.len());
        assert!(
            matches!(
                ctx,
                CompletionContext::TypeColon {
                    ref tag_type,
                    ref prefix,
                    span,
                } if tag_type == "size"
                    && prefix == "10"
                    && span == (line.len() - 2, line.len())
            ),
            "{ctx:?}"
        );
    }

    #[test]
    fn test_context_compact_label_comparison_strips_operator() {
        let cases = [
            ("s tag:>", "tag", ""),
            ("s tag:>foo", "tag", "foo"),
            ("s tag:>=10", "tag", "10"),
            ("s tag:<=10", "tag", "10"),
            ("s tag:<foo", "tag", "foo"),
            ("s tag:^foo", "tag", "foo"),
            ("s tag:^=foo", "tag", "foo"),
            ("s tag:=foo", "tag", "foo"),
            ("s extension:>r", "extension", "r"),
        ];
        for (line, tag, prefix) in cases {
            let ctx = parse_completion_context(line, line.len());
            let expected_span = (line.len() - prefix.len(), line.len());
            assert!(
                matches!(
                    ctx,
                    CompletionContext::TypeColon {
                        ref tag_type,
                        prefix: ref got,
                        span,
                    } if tag_type == tag
                        && got == prefix
                        && span == expected_span
                ),
                "{line} => {ctx:?}"
            );
        }
    }

    #[test]
    fn test_context_typed_comparison_operator_stays_operators() {
        let line = "s tag: :>";
        let ctx = parse_completion_context(line, line.len());
        assert!(
            matches!(
                ctx,
                CompletionContext::TypeWithSpace {
                    ref tag_type,
                    ref prefix,
                    ..
                } if tag_type == "tag" && prefix == ":>"
            ),
            "{ctx:?}"
        );
    }

    #[test]
    fn test_context_scalar_comparison_starts_expression() {
        let line = "s 100 :< ";
        let ctx = parse_completion_context(line, line.len());
        assert!(
            matches!(
                ctx,
                CompletionContext::ExpressionStart {
                    ref prefix,
                    span,
                    ..
                } if prefix.is_empty() && span == (line.len(), line.len())
            ),
            "{ctx:?}"
        );
        let line = "s 100 :< si";
        let ctx = parse_completion_context(line, line.len());
        assert!(
            matches!(
                ctx,
                CompletionContext::ExpressionStart { ref prefix, .. } if prefix == "si"
            ),
            "{ctx:?}"
        );
    }

    #[test]
    fn test_tokenize_query_preserves_quoted_spaces() {
        assert_eq!(
            tokenize_query(r#"author:"John Doe" & ext"#),
            vec!["author:\"John Doe\"", "&", "ext"]
        );
        assert_eq!(
            tokenize_query("author:'John Doe'"),
            vec!["author:'John Doe'"]
        );
        assert_eq!(tokenize_query(r#"a\" b"#), vec![r#"a\""#, "b"]);
        assert_eq!(tokenize_query("  foo   bar  "), vec!["foo", "bar"]);
        assert_eq!(tokenize_query(""), Vec::<&str>::new());
    }

    #[test]
    fn test_context_type_colon_quoted_label_with_spaces() {
        let line = r#"s author:"John Doe"#;
        let ctx = parse_completion_context(line, line.len());
        assert!(
            matches!(ctx, CompletionContext::TypeColon { tag_type, prefix, span } if tag_type == "author" && prefix == "John Doe" && span == (line.len() - "John Doe".len(), line.len()))
        );

        let line_sq = "s author:'Jane Smith";
        let ctx_sq = parse_completion_context(line_sq, line_sq.len());
        assert!(
            matches!(ctx_sq, CompletionContext::TypeColon { tag_type, prefix, .. } if tag_type == "author" && prefix == "Jane Smith")
        );
    }

    #[test]
    fn test_is_inside_single_and_double_quotes() {
        assert!(is_inside_quotes(r#"s author:"John"#));
        assert!(!is_inside_quotes(r#"s author:"John""#));
        assert!(is_inside_quotes("s author:'Jane"));
        assert!(!is_inside_quotes("s author:'Jane'"));
        assert!(!is_inside_quotes(r#"s author:"John" extra"#));
        assert!(is_inside_quotes(r#"s 'a\'"#));
    }

    #[test]
    fn test_context_inside_quotes_and_edit_query() {
        let ctx = parse_completion_context("t \"project:ttfm\" path:", 22);
        assert!(
            matches!(ctx, CompletionContext::EditQuery { tag_type, in_quotes: false, is_untag: false, span, .. } if tag_type == "path" && span == (22, 22))
        );
    }

    #[test]
    fn test_context_index_path_command() {
        let ctx = parse_completion_context("i src/", 6);
        assert!(
            matches!(ctx, CompletionContext::IndexPath { path_prefix, span } if path_prefix == "src/" && span == (2, 6))
        );
    }

    #[test]
    fn test_context_cli_normalized_command() {
        let ctx1 = parse_completion_context("ttfm search extension:r", 23);
        assert!(
            matches!(ctx1, CompletionContext::TypeColon { tag_type, prefix, span } if tag_type == "extension" && prefix == "r" && span == (22, 23))
        );
        let ctx2 = parse_completion_context("search extension:r", 18);
        assert!(
            matches!(ctx2, CompletionContext::TypeColon { tag_type, prefix, span } if tag_type == "extension" && prefix == "r" && span == (17, 18))
        );
    }
}

#[cfg(test)]
mod candidate_tests {
    use super::*;
    use crate::db::Store;
    use crate::tag::TagRegistry;

    fn setup_test_store() -> (tempfile::TempDir, Store, TagRegistry) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let reg = TagRegistry::with_standard();
        crate::indexing::Indexer::new(&store, &reg)
            .initialize_tables()
            .unwrap();
        (dir, store, reg)
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
    fn test_fetch_type_candidates_synthesizes_excluded_types() {
        let (_dir, store, reg) = setup_test_store();
        let types =
            fetch_type_and_func_candidates(&store, &reg, "si", true).unwrap();
        assert!(types.contains(&"size".to_string()));
    }

    #[test]
    fn test_fetch_type_and_func_candidates_uses_type_glob() {
        let (_dir, store, reg) = setup_test_store_with_files(&["main.rs"]);
        let types =
            fetch_type_and_func_candidates(&store, &reg, "ext", true).unwrap();
        assert!(types.contains(&"extension".to_string()));
    }

    #[test]
    fn test_fetch_type_and_func_candidates_includes_wildcard_star() {
        let (_dir, store, reg) = setup_test_store();
        let starred =
            fetch_type_and_func_candidates(&store, &reg, "*", true).unwrap();
        assert_eq!(starred, vec!["*".to_string()]);
        let all =
            fetch_type_and_func_candidates(&store, &reg, "", true).unwrap();
        assert!(all.contains(&"*".to_string()));
    }

    #[test]
    fn test_s_star_unique_type_candidate_has_colon_suffix() {
        let (_dir, store, reg) = setup_test_store();
        let cands = fetch_candidates(&store, &reg, "s *", 3, 0, 10);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].raw, "*:");
        assert_eq!(cands[0].kind, CandidateKind::Type);
        assert_eq!(
            unique_ghost_suffix("s *", 3, &cands),
            Some(":".to_string())
        );
    }

    #[test]
    fn test_escaped_wip_prefix_matches_bracket_label() {
        let (_dir, store, reg) = setup_test_store_with_files(&["[WIP]_doc"]);
        let items =
            fetch_tag_candidates(&store, &reg, "filename", r"\[WIP", 0, 10)
                .unwrap();
        assert!(items.iter().any(|s| s == "[WIP]_doc"), "{items:?}");
    }

    #[test]
    fn test_fetch_tag_candidates_from_tags_partition() {
        let (_dir, store, reg) =
            setup_test_store_with_files(&["main.rs", "test.c"]);
        let items = fetch_tag_candidates(&store, &reg, "extension", "r", 0, 10)
            .unwrap();
        assert!(items.contains(&"rs".to_string()));
    }

    #[test]
    fn test_fetch_tag_candidates_includes_unused_registered_label() {
        let (_dir, store, reg) = setup_test_store_with_files(&["main.rs"]);
        let ir = store.path_for_target(crate::db::TargetTable::ItemReferences);
        let path = ir.display().to_string().replace('\'', "''");
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT * FROM read_parquet('{path}') \
                     UNION ALL \
                     SELECT 99::BIGINT AS item_id, 7::BIGINT AS rank, \
                     'project:orphan' AS name, 'tag' AS item_kind, \
                     'project:orphan' AS content) \
                     TO '{path}' (FORMAT PARQUET)"
                ),
                [],
            )
            .unwrap();
        crate::db::tags::generate_tags_hive_partitioned(
            &store,
            &store.conn,
            &reg,
        )
        .unwrap();
        let items = fetch_tag_candidates(&store, &reg, "project", "orp", 0, 10)
            .unwrap();
        assert!(items.contains(&"orphan".to_string()), "{items:?}");
    }

    #[test]
    fn test_fetch_tag_candidates_type_and_enum_suggestions() {
        let (_dir, store, reg) = setup_test_store_with_files(&["main.rs"]);
        let types =
            fetch_tag_candidates(&store, &reg, "type", "", 0, 50).unwrap();
        assert!(types.iter().any(|t| t == "extension"));
        assert!(types.iter().any(|t| t == "size"));

        let kinds =
            fetch_tag_candidates(&store, &reg, "item_kind", "", 0, 10).unwrap();
        assert!(kinds.iter().any(|k| k == "file"));
        assert!(kinds.iter().any(|k| k == "tag"));

        let origins =
            fetch_tag_candidates(&store, &reg, "origin", "", 0, 10).unwrap();
        assert!(origins.iter().any(|o| o == "file"));
        assert!(origins.iter().any(|o| o == "user"));
    }

    #[test]
    fn test_fetch_tag_candidates_composite_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("files");
        std::fs::create_dir_all(root.join("sub_dir")).unwrap();
        std::fs::write(root.join("regular_file.txt"), "hello").unwrap();

        let reg = TagRegistry::with_standard();
        let store = Store::open(base.join("db")).unwrap();
        let indexer = crate::indexing::Indexer::new(&store, &reg);
        indexer.initialize_tables().unwrap();
        indexer.run(&[&root], None, false).unwrap();

        let candidates =
            fetch_tag_candidates(&store, &reg, "directory", "", 0, 10).unwrap();
        assert!(candidates.iter().any(|c| c == "sub_dir"));
        assert!(!candidates.iter().any(|c| c == "regular_file.txt"));
    }

    #[test]
    fn test_fetch_tag_candidates_directory_and_filename_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("files");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("srcfile"), "x").unwrap();

        let reg = TagRegistry::with_standard();
        let store = Store::open(base.join("db")).unwrap();
        let indexer = crate::indexing::Indexer::new(&store, &reg);
        indexer.initialize_tables().unwrap();
        indexer.run(&[&root], None, false).unwrap();

        let dirs = fetch_tag_candidates(&store, &reg, "directory", "sr", 0, 10)
            .unwrap();
        assert_eq!(dirs, vec!["src".to_string()], "{dirs:?}");
        let files = fetch_tag_candidates(&store, &reg, "filename", "sr", 0, 10)
            .unwrap();
        assert_eq!(files, vec!["srcfile".to_string()], "{files:?}");
        let miss =
            fetch_tag_candidates(&store, &reg, "directory", "zzz", 0, 10)
                .unwrap();
        assert!(miss.is_empty(), "{miss:?}");
    }

    #[test]
    fn test_fetch_operator_candidates_projection_vs_flat() {
        let ops_proj =
            fetch_operator_candidates(&CompletionContext::TermFinished {
                prefix: "".into(),
                is_projection: true,
                span: (0, 0),
            });
        let ops_flat =
            fetch_operator_candidates(&CompletionContext::TermFinished {
                prefix: "".into(),
                is_projection: false,
                span: (0, 0),
            });
        assert!(ops_proj.contains(&"&:".to_string()));
        assert!(!ops_flat.contains(&"&:".to_string()));
    }

    #[test]
    fn test_label_comparison_completes_labels_not_set_operators() {
        let (_dir, store, reg) =
            setup_test_store_with_files(&["main.rs", "lib.c"]);
        let spaced = "s extension: :> ";
        let cands = fetch_candidates(&store, &reg, spaced, spaced.len(), 0, 20);
        assert!(cands.iter().any(|c| c.raw.trim() == "rs"), "{cands:?}");
        assert!(cands.iter().all(|c| c.kind != CandidateKind::Operator));

        let compact = "s extension:>r";
        let cands =
            fetch_candidates(&store, &reg, compact, compact.len(), 0, 20);
        assert!(cands.iter().any(|c| c.raw.trim() == "rs"), "{cands:?}");
        assert!(cands.iter().all(|c| c.kind != CandidateKind::Operator));

        let bare = "s extension:>";
        let cands = fetch_candidates(&store, &reg, bare, bare.len(), 0, 20);
        assert!(cands.iter().any(|c| c.raw.trim() == "rs"), "{cands:?}");

        let sized = "s size:>";
        let cands = fetch_candidates(&store, &reg, sized, sized.len(), 0, 20);
        assert!(
            cands.iter().any(|c| c.kind == CandidateKind::Format),
            "{cands:?}"
        );
        assert!(cands.iter().any(|c| c.raw.contains("MB")), "{cands:?}");
        assert!(cands.iter().all(|c| c.kind != CandidateKind::Operator));
    }

    fn assert_inner_aggregate_candidates(cands: &[Candidate]) {
        assert!(!cands.is_empty(), "{cands:?}");
        assert!(
            cands.iter().any(|c| c.kind == CandidateKind::Type),
            "{cands:?}"
        );
        assert!(
            cands.iter().any(|c| c.kind == CandidateKind::Function),
            "{cands:?}"
        );
        assert!(
            cands
                .iter()
                .any(|c| c.raw == "sum(" || c.raw.ends_with(':')),
            "{cands:?}"
        );
        assert!(cands.iter().all(|c| c.raw != "random("), "{cands:?}");
    }

    #[test]
    fn test_fetch_candidates_inside_aggregate_function() {
        let (_dir, store, reg) = setup_test_store_with_files(&["main.rs"]);
        let open = "s sum(";
        let cands = fetch_candidates(&store, &reg, open, open.len(), 0, 50);
        assert_inner_aggregate_candidates(&cands);
        assert!(cands.iter().all(|c| c.span == (open.len(), open.len())));

        let closed = "s sum()";
        let pos = closed.find(')').unwrap();
        let cands = fetch_candidates(&store, &reg, closed, pos, 0, 50);
        assert_inner_aggregate_candidates(&cands);
        assert!(cands.iter().all(|c| c.span == (pos, pos)));

        let sized = "s sum(si";
        let cands = fetch_candidates(&store, &reg, sized, sized.len(), 0, 20);
        assert!(cands.iter().any(|c| c.raw == "size:"), "{cands:?}");
        assert_eq!(cands[0].span, (sized.len() - 2, sized.len()));
    }

    #[test]
    fn test_fetch_tag_candidates_path_uses_prefix_glob_not_substring() {
        let (_dir, store, reg) =
            setup_test_store_with_files(&["other/alpha.rs"]);
        let all =
            fetch_tag_candidates(&store, &reg, "path", "", 0, 10).unwrap();
        let full = all
            .iter()
            .find(|p| p.ends_with("alpha.rs"))
            .cloned()
            .unwrap();
        let mid =
            fetch_tag_candidates(&store, &reg, "path", "alpha", 0, 10).unwrap();
        assert!(!mid.iter().any(|p| p == &full));
        let start = full
            .char_indices()
            .map(|(i, _)| i)
            .nth(full.find("other").unwrap())
            .map(|i| full[..i + "other".len()].to_string())
            .unwrap();
        let pre =
            fetch_tag_candidates(&store, &reg, "path", &start, 0, 10).unwrap();
        assert!(pre.iter().any(|p| p == &full));
        let pd =
            fetch_tag_candidates(&store, &reg, "parentdir", "alpha", 0, 10)
                .unwrap();
        assert!(!pd.iter().any(|p| p.contains("alpha")));
    }

    #[test]
    fn test_resolve_ttql_search_matches_repl_search_line() {
        let (_dir, store, reg) =
            setup_test_store_with_files(&["main.rs", "my file.txt"]);
        for ttql in ["ext", "extension:r", "filename:\"my", "count("] {
            let repl_line = format!("s {ttql}");
            let repl: Vec<(String, usize)> = fetch_candidates(
                &store,
                &reg,
                &repl_line,
                repl_line.len(),
                0,
                50,
            )
            .into_iter()
            .map(|c| (c.raw, c.span.0 - 2))
            .collect();
            let ttql_side: Vec<(String, usize)> = resolve_ttql_candidates(
                &store,
                &reg,
                ttql,
                TtqlRole::Search,
                0,
                50,
            )
            .into_iter()
            .map(|c| (c.raw, c.span.0))
            .collect();
            assert_eq!(repl, ttql_side, "{ttql}");
        }
    }

    #[test]
    fn test_resolve_ttql_edit_matches_repl_edit_line() {
        let (_dir, store, reg) = setup_test_store_with_files(&["main.rs"]);
        for (ttql, is_untag) in [("ext", false), ("", false), ("ext", true)] {
            let cmd = if is_untag { "u" } else { "t" };
            let repl_line = format!("{cmd} \"x\" {ttql}");
            let base = repl_line.len() - ttql.len();
            let repl: Vec<(String, usize)> = fetch_candidates(
                &store,
                &reg,
                &repl_line,
                repl_line.len(),
                0,
                50,
            )
            .into_iter()
            .map(|c| (c.raw, c.span.0 - base))
            .collect();
            let ttql_side: Vec<(String, usize)> = resolve_ttql_candidates(
                &store,
                &reg,
                ttql,
                TtqlRole::Edit { is_untag },
                0,
                50,
            )
            .into_iter()
            .map(|c| (c.raw, c.span.0))
            .collect();
            assert_eq!(repl, ttql_side, "{ttql}");
        }
    }
}

#[cfg(test)]
mod suffix_tests {
    use super::*;

    #[test]
    fn test_apply_smart_suffix_ttql_grammar_escaping() {
        assert_eq!(
            apply_smart_suffix("project", CandidateKind::Type, false),
            "project:"
        );
        assert_eq!(
            apply_smart_suffix("sum", CandidateKind::Function, false),
            "sum("
        );
        assert_eq!(
            apply_smart_suffix("rs", CandidateKind::Label, false),
            "rs "
        );
        assert_eq!(
            apply_smart_suffix("my work", CandidateKind::Label, false),
            "\"my work\" "
        );
        assert_eq!(
            apply_smart_suffix("my work", CandidateKind::Label, true),
            "my work\" "
        );
        assert_eq!(
            apply_smart_suffix("a&b", CandidateKind::Label, false),
            "\"a&b\" "
        );
        assert_eq!(
            apply_smart_suffix("[WIP]_a.md", CandidateKind::Label, false),
            "\\[WIP\\]_a.md "
        );
        assert_eq!(
            apply_smart_suffix("10.0MB", CandidateKind::Format, false),
            "10.0MB "
        );
        assert_eq!(
            apply_smart_suffix("&", CandidateKind::Operator, false),
            "& "
        );
        assert_eq!(
            apply_smart_suffix(")", CandidateKind::CloseParen, false),
            ") "
        );
    }

    #[test]
    fn test_escape_ttql_special_chars_includes_backslash_and_quotes() {
        assert_eq!(escape_ttql_special_chars("a\\b"), "a\\\\b");
        assert_eq!(escape_ttql_special_chars("a\"b"), "a\\\"b");
        assert_eq!(escape_ttql_special_chars("[WIP]"), "\\[WIP\\]");
    }

    #[test]
    fn test_escape_in_quote_glob_escapes_glob_and_quote_meta() {
        assert_eq!(escape_in_quote_glob(r#"a\b"#), r#"a\\b"#);
        assert_eq!(escape_in_quote_glob(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_in_quote_glob("a*b"), r#"a\*b"#);
        assert_eq!(escape_in_quote_glob("a?b"), r#"a\?b"#);
        assert_eq!(escape_in_quote_glob("a[b]"), r#"a\[b\]"#);
    }

    #[test]
    fn test_definition_label_query_unescapes_inserted_prefix() {
        assert_eq!(
            definition_label_query("project", r"\[WIP"),
            "tag:\"project:\\[WIP*\""
        );
        assert_eq!(definition_type_query(r"\["), "type:\"\\[*\"");
        assert_eq!(
            definition_label_query("project", "al"),
            "tag:\"project:al*\""
        );
    }

    #[test]
    fn test_definition_label_query_and_strip_type_prefix() {
        assert_eq!(
            definition_label_query("extension", "r"),
            "tag:\"extension:r*\""
        );
        assert_eq!(
            definition_label_query("name", r#"a*b"#),
            "tag:\"name:a\\*b*\""
        );
        assert_eq!(
            strip_type_prefix("extension:rs", "extension"),
            Some("rs".into())
        );
    }

    #[test]
    fn test_sanitize_tag_type_rejects_traversal_in_autocomplete() {
        assert_eq!(sanitize_tag_type_for_path("../other"), None);
        assert_eq!(sanitize_tag_type_for_path("a/b"), None);
        assert_eq!(sanitize_tag_type_for_path("a\\b"), None);
        assert_eq!(sanitize_tag_type_for_path("extension"), Some("extension"));
    }

    #[test]
    fn test_parse_index_context_multiple_words_isolates_last_word() {
        let ctx = parse_completion_context("i /path/one /path/two", 21);
        match ctx {
            CompletionContext::IndexPath { path_prefix, span } => {
                assert_eq!(path_prefix, "/path/two");
                assert_eq!(span, (12, 21));
            }
            _ => panic!("expected IndexPath"),
        }
    }

    #[test]
    fn test_expand_tilde_resolves_home_path() {
        let p = expand_tilde("~/docs");
        assert!(!p.starts_with('~'));
    }

    #[test]
    fn test_unique_ghost_suffix_ttql_grammar_escaping() {
        let wip = [Candidate {
            raw: apply_smart_suffix("[WIP]_a.md", CandidateKind::Label, false),
            kind: CandidateKind::Label,
            span: (11, 13),
        }];
        assert_eq!(
            unique_ghost_suffix("s filename:\\[", 13, &wip),
            Some("WIP\\]_a.md ".into())
        );

        let spaced = [Candidate {
            raw: apply_smart_suffix("my work", CandidateKind::Label, false),
            kind: CandidateKind::Label,
            span: (11, 14),
        }];
        assert_eq!(
            unique_ghost_suffix("s filename:\"my", 14, &spaced),
            Some(" work\" ".into())
        );

        let amp = [Candidate {
            raw: apply_smart_suffix("a&b", CandidateKind::Label, false),
            kind: CandidateKind::Label,
            span: (11, 13),
        }];
        assert_eq!(
            unique_ghost_suffix("s filename:\"a", 13, &amp),
            Some("&b\" ".into())
        );
    }

    #[test]
    fn test_unique_ghost_suffix_none_unless_strictly_one() {
        let span = (2, 5);
        let one = [Candidate {
            raw: "extension:".into(),
            kind: CandidateKind::Type,
            span,
        }];
        assert_eq!(
            unique_ghost_suffix("s ext", 5, &one),
            Some("ension:".into())
        );
        assert_eq!(unique_ghost_suffix("s ext", 5, &[]), None);
        let two = [
            one[0].clone(),
            Candidate {
                raw: "external:".into(),
                kind: CandidateKind::Type,
                span,
            },
        ];
        assert_eq!(unique_ghost_suffix("s ext", 5, &two), None);
    }

    #[test]
    fn test_unique_ghost_suffix_label_function_and_cursor() {
        let label = [Candidate {
            raw: "rs ".into(),
            kind: CandidateKind::Label,
            span: (12, 13),
        }];
        assert_eq!(
            unique_ghost_suffix("s extension:r", 13, &label),
            Some("s ".into())
        );
        let func = [Candidate {
            raw: "count(".into(),
            kind: CandidateKind::Function,
            span: (2, 5),
        }];
        assert_eq!(unique_ghost_suffix("s cou", 5, &func), Some("nt(".into()));
        assert_eq!(unique_ghost_suffix("s cou", 4, &func), None);
        let past = [Candidate {
            raw: " *:*".into(),
            kind: CandidateKind::PastQuery,
            span: (1, 1),
        }];
        assert_eq!(unique_ghost_suffix("s", 1, &past), Some(" *:*".into()));
    }

    #[test]
    fn test_escape_ttql_special_chars_escapes_braces() {
        assert_eq!(escape_ttql_special_chars("{tag}"), "\\{tag\\}");
    }

    #[test]
    fn test_brace_label_candidate_is_literal_in_edit_query() {
        let reg = TagRegistry::with_standard();
        let raw = apply_smart_suffix("a{1}b", CandidateKind::Label, false);
        let q = crate::edit::parse::parse_edit_query(
            &format!("note:{}", raw.trim_end()),
            crate::edit::QueryType::Tag,
            &reg,
        )
        .unwrap();
        let leaf = q.nodes[0].label.clone().unwrap();
        assert!(!leaf.has_braced());
        assert_eq!(leaf.value(), "a{1}b");
    }

    #[test]
    fn test_brace_label_candidate_is_literal_in_search_query() {
        let raw = apply_smart_suffix("a{1}b", CandidateKind::Label, false);
        let node =
            crate::query::parse_nowarn(&format!("note:{}", raw.trim_end()))
                .unwrap();
        if let crate::query::QueryNode::TypedTag(tt) = node {
            assert_eq!(tt.label.as_str(), "a{1}b");
        } else {
            panic!("expected TypedTag, got {node:?}");
        }
    }
}
