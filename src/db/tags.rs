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

use crate::db::{Col, DuckDbFunc, Store, TargetTable, Tbl};
use crate::query::ast::{Operand, QueryNode};
use crate::query::lens_reader::ReadRule;
use crate::tag::TagRegistry;
use crate::types::{Origin, TagType};
use crate::util;
use anyhow::Result;
use duckdb::Connection;
use sea_query::{
    CommonTableExpression, Expr, Func, JoinType, PostgresQueryBuilder, Query,
    SelectStatement, UnionType, WithClause,
};
use std::path::Path;
use strum::IntoEnumIterator;

fn sanitize_tag_type(tag_type: &str) -> Result<&str> {
    if tag_type.is_empty()
        || tag_type.contains('/')
        || tag_type.contains('\\')
        || tag_type.contains("..")
    {
        anyhow::bail!("Invalid tag_type for partition: {}", tag_type);
    }
    Ok(tag_type)
}

struct DictMapping {
    out_type: String,
    src_type: String,
    fallback: Option<String>,
    implies: Option<(String, String)>,
}

fn projection_source_type(node: &QueryNode) -> Option<String> {
    match node {
        QueryNode::Nest(n) => match &n.right {
            Operand::TypeRef(t) => Some(t.as_str().to_string()),
            Operand::Query(inner) => projection_source_type(inner),
            _ => None,
        },
        QueryNode::And(nodes) => nodes.iter().find_map(projection_source_type),
        _ => None,
    }
}

fn dictionary_mappings(registry: &TagRegistry) -> Vec<DictMapping> {
    registry
        .iter_arcs()
        .filter(|f| !f.exclude_in_tags_table())
        .map(|f| {
            let out_type = f.name().to_string();
            let node = f.query().expand_projection(&TagType::from(f.name()));
            let src_type = projection_source_type(&node)
                .unwrap_or_else(|| out_type.clone());
            let implies = f.query().implies().map(|(t, l)| {
                let s = match l.value {
                    crate::types::Bitical::Boolean(true) => "true".to_string(),
                    crate::types::Bitical::Boolean(false) => {
                        "false".to_string()
                    }
                    _ => l.as_str(),
                };
                (t.as_str().to_string(), s)
            });
            let fallback =
                f.query().read().rules().iter().find_map(|r| match r {
                    ReadRule::Fallback(t) => Some(t.as_str().to_string()),
                    ReadRule::Prefer(_) => None,
                });
            DictMapping {
                out_type,
                src_type,
                fallback,
                implies,
            }
        })
        .collect()
}

fn needed_physical_types(
    target_type: Option<&str>,
    mappings: &[DictMapping],
) -> Option<Vec<String>> {
    let t = target_type?;
    if let Some(m) = mappings.iter().find(|m| m.out_type == t) {
        let mut v = vec![m.src_type.clone()];
        let extra = m.fallback.iter().chain(m.implies.iter().map(|(i, _)| i));
        for t in extra {
            if !v.contains(t) {
                v.push(t.clone());
            }
        }
        Some(v)
    } else {
        Some(vec![t.to_string()])
    }
}

fn apply_type_filter(
    q: &mut SelectStatement,
    col: Col,
    types: Option<&[String]>,
) {
    if let Some(types) = types {
        q.and_where(Expr::col(col).is_in(types.to_vec()));
    }
}

fn build_physical_tags_query(
    store: &Store,
    registry: &TagRegistry,
    needed: Option<&[String]>,
) -> SelectStatement {
    let mut union_q = Query::select();
    let loc_label = Expr::col((Tbl::TagsByLocation, Col::LabelStr));
    union_q
        .column((Tbl::TagsByLocation, Col::Type))
        .expr_as(loc_label, Col::Label)
        .column((Tbl::TagsByLocation, Col::ItemId))
        .expr_as(Expr::val(Origin::File.as_str()), Col::Origin)
        .expr_as(Expr::val(true), Col::IsAttached)
        .from_subquery(
            util::parquet_query(
                &store
                    .path_for_target(TargetTable::TagsByLocation)
                    .to_string_lossy(),
            ),
            Tbl::TagsByLocation,
        );
    apply_type_filter(&mut union_q, Col::Type, needed);

    let mut user_q = Query::select();
    let user_label = crate::oneview::build_label_str_expr(Tbl::UserTags);
    user_q
        .column((Tbl::UserTags, Col::Type))
        .expr_as(user_label, Col::Label)
        .column((Tbl::UserTags, Col::ItemId))
        .expr_as(Expr::val(Origin::User.as_str()), Col::Origin)
        .expr_as(Expr::val(true), Col::IsAttached)
        .from_subquery(
            util::parquet_query(
                &store
                    .path_for_target(TargetTable::UserTags)
                    .to_string_lossy(),
            ),
            Tbl::UserTags,
        );
    apply_type_filter(&mut user_q, Col::Type, needed);
    union_q.union(UnionType::All, user_q);

    let mut base_q = Query::select();
    let base_label = crate::oneview::build_label_str_expr(Tbl::BaseTags);
    base_q
        .column((Tbl::BaseTags, Col::Type))
        .expr_as(base_label, Col::Label)
        .column((Tbl::BaseTags, Col::ItemId))
        .expr_as(Expr::val(Origin::File.as_str()), Col::Origin)
        .expr_as(Expr::val(true), Col::IsAttached)
        .from_subquery(
            util::parquet_query(
                &store
                    .path_for_target(TargetTable::BaseTags)
                    .to_string_lossy(),
            ),
            Tbl::BaseTags,
        );
    let excluded = registry.get_excluded_from_tags();
    if !excluded.is_empty() {
        base_q.and_where(
            Expr::col((Tbl::BaseTags, Col::Type)).is_not_in(excluded),
        );
    }
    apply_type_filter(&mut base_q, Col::Type, needed);
    union_q.union(UnionType::All, base_q);

    let include_rank = needed.is_none()
        || needed.is_some_and(|t| t.iter().any(|s| s == "rank"));
    if include_rank {
        let file_refs_path = store.path_for_target(TargetTable::FileReferences);
        if file_refs_path.exists() {
            let mut rank_q = Query::select();
            let rank_str = Expr::col((Tbl::FileReferences, Col::Rank))
                .cast_as(crate::db::BiticalType::String);
            let user_tags_path = store.path_for_target(TargetTable::UserTags);
            rank_q
                .expr_as(Expr::val("rank"), Col::Type)
                .expr_as(rank_str, Col::Label)
                .column((Tbl::FileReferences, Col::ItemId))
                .expr_as(Expr::val(Origin::File.as_str()), Col::Origin)
                .expr_as(Expr::val(true), Col::IsAttached)
                .from_subquery(
                    util::parquet_query(&file_refs_path.to_string_lossy()),
                    Tbl::FileReferences,
                )
                .and_where(
                    Expr::col((Tbl::FileReferences, Col::ItemId))
                        .not_in_subquery(
                            Query::select()
                                .column(Col::ItemId)
                                .from_subquery(
                                    util::parquet_query(
                                        &user_tags_path.to_string_lossy(),
                                    ),
                                    Tbl::UserTags,
                                )
                                .and_where(Expr::col(Col::Type).eq("rank"))
                                .to_owned(),
                        ),
                );
            union_q.union(UnionType::All, rank_q);
        }
    }
    union_q
}

fn typed_tag_expr(out_type: &str, label_tbl: Tbl) -> sea_query::SimpleExpr {
    Func::cust(DuckDbFunc::Concat)
        .args([
            Expr::val(out_type).into(),
            Expr::val(":").into(),
            Expr::col((label_tbl, Col::Label)).into(),
        ])
        .into()
}

fn mapping_branch(
    mapping: &DictMapping,
    include_type: bool,
) -> SelectStatement {
    let mut q = mapping_scan(mapping, &mapping.src_type, include_type);
    if let Some(fb) = &mapping.fallback {
        let mut arm = mapping_scan(mapping, fb, include_type);
        arm.and_where(
            Expr::col((Tbl::Scan, Col::ItemId)).not_in_subquery(
                Query::select()
                    .column(Col::ItemId)
                    .from(Tbl::ItemTags)
                    .and_where(
                        Expr::col(Col::Type).eq(mapping.src_type.as_str()),
                    )
                    .to_owned(),
            ),
        );
        q.union(UnionType::All, arm);
    }
    q
}

fn mapping_scan(
    mapping: &DictMapping,
    scan_type: &str,
    include_type: bool,
) -> SelectStatement {
    let mut q = Query::select();
    if include_type {
        q.expr_as(Expr::val(mapping.out_type.as_str()), Col::Type);
    }
    q.column((Tbl::Scan, Col::Label))
        .column((Tbl::Scan, Col::Origin))
        .expr_as(typed_tag_expr(&mapping.out_type, Tbl::Scan), Col::TypedTag)
        .column((Tbl::Scan, Col::IsAttached));
    q.from_subquery(
        Query::select()
            .column(sea_query::Asterisk)
            .from(Tbl::ItemTags)
            .to_owned(),
        Tbl::Scan,
    )
    .and_where(Expr::col((Tbl::Scan, Col::Type)).eq(scan_type));
    if let Some((imp_type, imp_label)) = &mapping.implies {
        q.join_subquery(
            JoinType::InnerJoin,
            Query::select()
                .column(sea_query::Asterisk)
                .from(Tbl::ItemTags)
                .to_owned(),
            Tbl::Live,
            Expr::col((Tbl::Scan, Col::ItemId))
                .eq(Expr::col((Tbl::Live, Col::ItemId))),
        )
        .and_where(Expr::col((Tbl::Live, Col::Type)).eq(imp_type.as_str()))
        .and_where(Expr::col((Tbl::Live, Col::Label)).eq(imp_label.as_str()));
    }
    q
}

fn unmapped_physical_branch(
    mapped_outs: &[String],
    target_type: Option<&str>,
    include_type: bool,
) -> SelectStatement {
    let mut q = Query::select();
    if include_type {
        q.column((Tbl::Scan, Col::Type));
    }
    q.column((Tbl::Scan, Col::Label))
        .column((Tbl::Scan, Col::Origin))
        .expr_as(
            Func::cust(DuckDbFunc::Concat).args([
                Expr::col((Tbl::Scan, Col::Type)).into(),
                Expr::val(":").into(),
                Expr::col((Tbl::Scan, Col::Label)).into(),
            ]),
            Col::TypedTag,
        )
        .column((Tbl::Scan, Col::IsAttached));
    q.from_subquery(
        Query::select()
            .column(sea_query::Asterisk)
            .from(Tbl::ItemTags)
            .to_owned(),
        Tbl::Scan,
    );
    if let Some(t) = target_type {
        q.and_where(Expr::col((Tbl::Scan, Col::Type)).eq(t));
    }
    if !mapped_outs.is_empty() {
        q.and_where(
            Expr::col((Tbl::Scan, Col::Type)).is_not_in(mapped_outs.to_vec()),
        );
    }
    q
}

fn build_tags_select_query(
    store: &Store,
    registry: &TagRegistry,
    target_type: Option<&str>,
) -> SelectStatement {
    let mappings = dictionary_mappings(registry);
    let needed = needed_physical_types(target_type, &mappings);
    let physical =
        build_physical_tags_query(store, registry, needed.as_deref());
    let include_type = target_type.is_none();
    let mut derived: Option<SelectStatement> = None;
    for mapping in &mappings {
        if let Some(t) = target_type {
            if mapping.out_type != t {
                continue;
            }
        }
        let branch = mapping_branch(mapping, include_type);
        match &mut derived {
            Some(q) => {
                q.union(UnionType::All, branch);
            }
            None => derived = Some(branch),
        }
    }
    let mapped_outs: Vec<String> =
        mappings.iter().map(|m| m.out_type.clone()).collect();
    let emit_unmapped = match target_type {
        Some(t) => !mapped_outs.iter().any(|m| m == t),
        None => true,
    };
    if emit_unmapped {
        let unmapped =
            unmapped_physical_branch(&mapped_outs, target_type, include_type);
        match &mut derived {
            Some(q) => {
                q.union(UnionType::All, unmapped);
            }
            None => derived = Some(unmapped),
        }
    }
    let derived = derived.unwrap_or_else(|| {
        let mut q = Query::select();
        if include_type {
            q.expr_as(Expr::val(""), Col::Type);
        }
        q.expr_as(Expr::val(""), Col::Label)
            .expr_as(Expr::val(""), Col::Origin)
            .expr_as(Expr::val(""), Col::TypedTag)
            .expr_as(Expr::val(true), Col::IsAttached)
            .and_where(Expr::cust("FALSE"));
        q
    });

    let mut with_clause = WithClause::new();
    with_clause.cte(
        CommonTableExpression::new()
            .query(physical)
            .table_name(Tbl::ItemTags)
            .to_owned(),
    );
    with_clause.cte(
        CommonTableExpression::new()
            .query(derived)
            .table_name(Tbl::Target)
            .to_owned(),
    );

    let mut item_refs_q = Query::select();
    item_refs_q
        .columns([Col::Content, Col::ItemId, Col::Rank])
        .from_subquery(
            util::parquet_query(
                &store
                    .path_for_target(TargetTable::ItemReferences)
                    .to_string_lossy(),
            ),
            Tbl::ItemReferences,
        )
        .and_where(Expr::col(Col::ItemKind).eq("tag"));

    let rank_expr = Func::cust(DuckDbFunc::Coalesce).args([
        Expr::col((Tbl::ItemReferences, Col::Rank)).into(),
        Expr::val(0i64).into(),
    ]);

    let used_src = Query::select()
        .column(Col::TypedTag)
        .from(Tbl::Target)
        .to_owned();
    let mut q = Query::select();
    q.distinct();
    if include_type {
        q.column((Tbl::Target, Col::Type));
    }
    q.column((Tbl::Target, Col::Label))
        .column((Tbl::Target, Col::Origin))
        .column((Tbl::Target, Col::TypedTag))
        .expr_as(Expr::col((Tbl::ItemReferences, Col::ItemId)), Col::ItemId)
        .expr_as(rank_expr, Col::Rank)
        .column((Tbl::Target, Col::IsAttached))
        .from(Tbl::Target)
        .join_subquery(
            JoinType::LeftJoin,
            item_refs_q,
            Tbl::ItemReferences,
            Expr::col((Tbl::Target, Col::TypedTag))
                .eq(Expr::col((Tbl::ItemReferences, Col::Content))),
        );
    let unused = unused_registered_tags_query(store, target_type, used_src);
    q.union(UnionType::All, unused)
        .order_by(Col::Rank, sea_query::Order::Desc)
        .order_by(Col::Label, sea_query::Order::Asc)
        .order_by(Col::ItemId, sea_query::Order::Asc);
    q.with_cte(with_clause);
    q
}

fn unused_registered_tags_query(
    store: &Store,
    target_type: Option<&str>,
    used_src: SelectStatement,
) -> SelectStatement {
    let content = Expr::col((Tbl::ItemReferences, Col::Content));
    let type_expr = Func::cust(DuckDbFunc::SplitPart).args([
        content.clone().into(),
        Expr::val(":").into(),
        Expr::val(1i64).into(),
    ]);
    let label_start = Expr::cust_with_exprs(
        "$1 + 2",
        [Func::cust(DuckDbFunc::Length).arg(type_expr.clone()).into()],
    );
    let label_expr = Func::cust(DuckDbFunc::Substr)
        .args([content.clone().into(), label_start]);
    let mut q = Query::select();
    if target_type.is_none() {
        q.expr_as(type_expr.clone(), Col::Type);
    }
    q.expr_as(label_expr, Col::Label)
        .expr_as(
            Expr::cust(crate::db::CustomFunc::item_id_origin_qualified(
                Tbl::ItemReferences,
                Col::ItemId,
            )),
            Col::Origin,
        )
        .expr_as(content.clone(), Col::TypedTag)
        .column(Col::ItemId)
        .column(Col::Rank)
        .expr_as(Expr::val(false), Col::IsAttached)
        .from_subquery(
            util::parquet_query(
                &store
                    .path_for_target(TargetTable::ItemReferences)
                    .to_string_lossy(),
            ),
            Tbl::ItemReferences,
        )
        .and_where(Expr::col(Col::ItemKind).eq("tag"))
        .and_where(content.not_in_subquery(used_src));
    if let Some(t) = target_type {
        q.and_where(Expr::expr(type_expr).eq(t));
    }
    q
}

fn rewrite_partition_files_sorted(
    conn: &Connection,
    tags_dir: &Path,
) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(tags_dir) else {
        return Ok(());
    };
    for entry in entries {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let has_parquet = std::fs::read_dir(&path)?.flatten().any(|e| {
            e.path().extension().and_then(|s| s.to_str()) == Some("parquet")
        });
        if !has_parquet {
            rewrite_partition_files_sorted(conn, &path)?;
            continue;
        }
        let glob = path.join("*.parquet");
        let dest = path.join("data.parquet");
        let tmp = path.join("data.parquet.tmp");
        if tmp.exists() {
            std::fs::remove_file(&tmp)?;
        }
        let copy_sql = format!(
            "COPY (SELECT * FROM read_parquet('{}', hive_partitioning = false) \
             ORDER BY rank DESC, label ASC, item_id ASC) \
             TO '{}' (FORMAT PARQUET)",
            glob.display().to_string().replace('\'', "''"),
            tmp.display().to_string().replace('\'', "''"),
        );
        conn.execute(&copy_sql, [])?;
        for old in std::fs::read_dir(&path)? {
            let old = old?.path();
            if old.extension().and_then(|s| s.to_str()) == Some("parquet") {
                std::fs::remove_file(old)?;
            }
        }
        std::fs::rename(&tmp, &dest)?;
    }
    Ok(())
}

fn valid_origins(store: &Store) -> Vec<Origin> {
    Origin::iter()
        .filter(|o| *o != Origin::File || store.is_indexed())
        .collect()
}

fn swap_partition(
    tmp_part: &Path,
    part_dir: &Path,
    old_part: &Path,
) -> Result<()> {
    if !tmp_part.exists() {
        if part_dir.exists() {
            std::fs::remove_dir_all(part_dir)?;
        }
        return Ok(());
    }
    std::fs::create_dir_all(part_dir.parent().unwrap())?;
    if part_dir.exists() {
        if old_part.exists() {
            std::fs::remove_dir_all(old_part)?;
        }
        std::fs::rename(part_dir, old_part)?;
        if let Err(e) = std::fs::rename(tmp_part, part_dir) {
            let _ = std::fs::rename(old_part, part_dir);
            let _ = std::fs::remove_dir_all(tmp_part);
            return Err(e.into());
        }
        let _ = std::fs::remove_dir_all(old_part);
    } else if let Err(e) = std::fs::rename(tmp_part, part_dir) {
        let _ = std::fs::remove_dir_all(tmp_part);
        return Err(e.into());
    }
    Ok(())
}

fn update_tag_type_partition(
    store: &Store,
    conn: &Connection,
    registry: &TagRegistry,
    tag_type: &str,
) -> Result<()> {
    let tag_type = sanitize_tag_type(tag_type)?;
    let origins = valid_origins(store);
    let mut inner = build_tags_select_query(store, registry, Some(tag_type));
    inner.limit(i64::MAX as u64);
    let q = Query::select()
        .column(sea_query::Asterisk)
        .from_subquery(inner, Tbl::Tags)
        .and_where(
            Expr::col(Col::Origin)
                .is_in(origins.iter().map(|o| o.as_str()).collect::<Vec<_>>()),
        )
        .to_owned();
    let tmp_type = store.temp_tags_dir().join(format!("type={tag_type}"));
    if tmp_type.exists() {
        std::fs::remove_dir_all(&tmp_type)?;
    }
    std::fs::create_dir_all(&tmp_type)?;
    conn.execute(
        &format!(
            "COPY ({}) TO '{}' (FORMAT PARQUET, PARTITION_BY (origin), OVERWRITE_OR_IGNORE)",
            q.to_string(PostgresQueryBuilder),
            tmp_type.display().to_string().replace('\'', "''")
        ),
        [],
    )?;
    rewrite_partition_files_sorted(conn, &tmp_type)?;
    for origin in origins {
        let tmp_part = tmp_type.join(format!("origin={}", origin.as_str()));
        let part_dir = store
            .tags_origin_dir(origin)
            .join(format!("type={tag_type}"));
        let old_part = store
            .db_dir
            .join(format!("tags_{}_{tag_type}.old", origin.as_str()));
        swap_partition(&tmp_part, &part_dir, &old_part)?;
    }
    let _ = std::fs::remove_dir_all(&tmp_type);
    Ok(())
}

fn expand_modified_types(
    registry: &TagRegistry,
    modified_types: &[&str],
) -> Vec<String> {
    let mut out: Vec<String> =
        modified_types.iter().map(|s| s.to_string()).collect();
    for mapping in dictionary_mappings(registry) {
        let sourced = modified_types.iter().any(|&t| {
            t == mapping.src_type || mapping.fallback.as_deref() == Some(t)
        });
        if sourced && !out.iter().any(|t| t == &mapping.out_type) {
            out.push(mapping.out_type);
        }
    }
    out
}

pub fn update_tags_partitions(
    store: &Store,
    conn: &Connection,
    registry: &TagRegistry,
    modified_types: &[&str],
) -> Result<()> {
    let expanded = expand_modified_types(registry, modified_types);
    for tag_type in &expanded {
        update_tag_type_partition(store, conn, registry, tag_type)?;
    }
    Ok(())
}

pub fn generate_tags_hive_partitioned(
    store: &Store,
    conn: &Connection,
    registry: &TagRegistry,
) -> Result<()> {
    let tmp_dir = store.temp_tags_dir();
    if tmp_dir.exists() {
        std::fs::remove_dir_all(&tmp_dir)?;
    }
    std::fs::create_dir_all(&tmp_dir)?;
    let q = build_tags_select_query(store, registry, None);
    let select_sql = q.to_string(PostgresQueryBuilder);
    let copy_sql = format!(
        "COPY ({select_sql}) TO '{}' \
         (FORMAT PARQUET, PARTITION_BY (origin, type), OVERWRITE_OR_IGNORE)",
        tmp_dir.display().to_string().replace('\'', "''")
    );
    conn.execute(&copy_sql, [])?;
    rewrite_partition_files_sorted(conn, &tmp_dir)?;
    if store.tags_dir().exists() {
        let backup = store.db_dir.join("tags.old");
        if backup.exists() {
            std::fs::remove_dir_all(&backup)?;
        }
        std::fs::rename(store.tags_dir(), &backup)?;
        std::fs::rename(&tmp_dir, store.tags_dir())?;
        let _ = std::fs::remove_dir_all(&backup);
    } else {
        std::fs::rename(&tmp_dir, store.tags_dir())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn insert_dummy_test_data(store: &Store) -> Result<()> {
        let loc_p = store.path_for_target(TargetTable::TagsByLocation);
        let base_p = store.path_for_target(TargetTable::BaseTags);
        let user_p = store.path_for_target(TargetTable::UserTags);
        let item_p = store.path_for_target(TargetTable::ItemReferences);

        store.conn.execute(
            &format!(
                "COPY (SELECT 1::BIGINT AS item_id, 'extension' AS type, \
                 'rs' AS label_str, NULL::BIGINT AS label_int, \
                 NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool) \
                 TO '{}' (FORMAT PARQUET)",
                loc_p.to_string_lossy().replace('\'', "''")
            ),
            [],
        )?;

        store.conn.execute(
            &format!(
                "COPY (SELECT 1::BIGINT AS item_id, 'size' AS type, \
                 NULL::VARCHAR AS label_str, 1024::BIGINT AS label_int, \
                 NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool, \
                 0::BIGINT AS rank \
                 UNION ALL \
                 SELECT 1::BIGINT, 'is_dir', NULL::VARCHAR, \
                 NULL::BIGINT, NULL::DOUBLE, false, 0::BIGINT) \
                 TO '{}' (FORMAT PARQUET)",
                base_p.to_string_lossy().replace('\'', "''")
            ),
            [],
        )?;

        store.conn.execute(
            &format!(
                "COPY (SELECT 1::BIGINT AS item_id, 'color' AS type, \
                 'blue' AS label_str, NULL::BIGINT AS label_int, \
                 NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool, \
                 0::BIGINT AS rank) \
                 TO '{}' (FORMAT PARQUET)",
                user_p.to_string_lossy().replace('\'', "''")
            ),
            [],
        )?;

        store.conn.execute(
            &format!(
                "COPY (SELECT 10::BIGINT AS item_id, 'tag' AS item_kind, \
                 'extension:rs' AS content, 'extension:rs' AS name, \
                 5::BIGINT AS rank) \
                 TO '{}' (FORMAT PARQUET)",
                item_p.to_string_lossy().replace('\'', "''")
            ),
            [],
        )?;
        Ok(())
    }

    #[test]
    fn test_generate_and_atomic_update_tags_hive() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();

        let mut q = Query::select();
        q.column((Tbl::Tags, Col::Type))
            .expr_as(Expr::col((Tbl::Tags, Col::Label)), Col::Name)
            .from_subquery(
                util::hive_parquet_query(&store.tags_dir()),
                Tbl::Tags,
            )
            .and_where(Expr::col((Tbl::Tags, Col::Type)).eq("extension"));
        let sql = q.to_string(PostgresQueryBuilder);
        let mut stmt = store.conn.prepare(&sql).unwrap();
        let labels: Vec<String> = stmt
            .query_map([], |r| r.get(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(labels.contains(&"rs".to_string()));

        let size_dir = store.tags_origin_dir(Origin::File).join("type=size");
        assert!(!size_dir.exists());
        assert!(store
            .tags_origin_dir(Origin::File)
            .join("type=extension")
            .exists());

        update_tags_partitions(&store, &store.conn, &registry, &["extension"])
            .unwrap();
        assert!(store
            .tags_origin_dir(Origin::File)
            .join("type=extension")
            .exists());
    }

    #[test]
    fn test_differential_update_preserves_hive_schema_uniformity() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();

        // 1つのパーティション（extension）を差分更新
        update_tags_partitions(&store, &store.conn, &registry, &["extension"])
            .unwrap();

        // 個々の parquet ファイルのカラム構成を直接検証（type 列が物理ファイル内に存在せず、全体生成と同一の4列であること）
        let ext_parquet = store
            .tags_origin_dir(Origin::File)
            .join("type=extension/data.parquet");
        let ext_file_cols: Vec<String> = store
            .conn
            .prepare(&format!(
                "SELECT column_name FROM (DESCRIBE SELECT * FROM read_parquet('{}', hive_partitioning = false))",
                ext_parquet.display().to_string().replace('\'', "''")
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(
            ext_file_cols,
            vec!["label", "tag", "item_id", "rank", "is_attached"]
        );

        // Hive glob 読み込みで全パーティションが正常に読めること
        let read_hive_sql = format!(
            "SELECT count(*) FROM read_parquet('{}/*/*/*.parquet', hive_partitioning = true)",
            store.tags_dir().display().to_string().replace('\'', "''")
        );
        let count: i64 = store
            .conn
            .query_row(&read_hive_sql, [], |r| r.get(0))
            .unwrap();
        assert!(count > 0);
    }

    #[test]
    fn test_update_tag_type_partition_unions_all_sources() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        insert_dummy_test_data(&store).unwrap();
        let base_p = store.path_for_target(TargetTable::BaseTags);
        let user_p = store.path_for_target(TargetTable::UserTags);

        // base_tags に mimetype: text/rust
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 'mimetype' AS type, \
                     'text/rust' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool, \
                     0::BIGINT AS rank) \
                     TO '{}' (FORMAT PARQUET)",
                    base_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();

        // user_tags に mimetype: text/plain
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 2::BIGINT AS item_id, 'mimetype' AS type, \
                     'text/plain' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool, \
                     0::BIGINT AS rank) \
                     TO '{}' (FORMAT PARQUET)",
                    user_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();

        let registry = TagRegistry::with_standard();
        std::fs::File::create(store.complete_path()).unwrap();
        update_tags_partitions(&store, &store.conn, &registry, &["mimetype"])
            .unwrap();

        let read_labels = |origin: Origin| -> Vec<String> {
            let mime_parquet = store
                .tags_origin_dir(origin)
                .join("type=mimetype/data.parquet");
            store
                .conn
                .prepare(&format!(
                    "SELECT label FROM read_parquet('{}', hive_partitioning = false) ORDER BY label ASC",
                    mime_parquet.display().to_string().replace('\'', "''")
                ))
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(read_labels(Origin::File), vec!["text/rust"]);
        assert_eq!(read_labels(Origin::User), vec!["text/plain"]);
    }

    #[test]
    fn test_tags_select_query_name_and_rank() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        let loc_p = store.path_for_target(TargetTable::TagsByLocation);
        let user_p = store.path_for_target(TargetTable::UserTags);
        let file_p = store.path_for_target(TargetTable::FileReferences);

        // TagsByLocation: filename = "sample.rs"
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 'filename' AS type, \
                     'sample.rs' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool) \
                     TO '{}' (FORMAT PARQUET)",
                    loc_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();

        // UserTags: name = "custom_sample"
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 2::BIGINT AS item_id, 'name' AS type, \
                     'custom_sample' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, NULL::BOOLEAN AS label_bool, \
                     0::BIGINT AS rank) \
                     TO '{}' (FORMAT PARQUET)",
                    user_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();

        // FileReferences: rank = 5
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 5::BIGINT AS rank, \
                     'file1' AS file_id, 100::BIGINT AS size, 1000::BIGINT AS mtime, \
                     NULL::VARCHAR AS hash, false AS is_dir) \
                     TO '{}' (FORMAT PARQUET)",
                    file_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();

        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();

        // Check name partition
        assert!(store
            .tags_origin_dir(Origin::File)
            .join("type=name")
            .exists());
        assert!(store
            .tags_origin_dir(Origin::User)
            .join("type=name")
            .exists());
        let mut q_name = Query::select();
        q_name
            .column((Tbl::Tags, Col::Label))
            .from_subquery(
                util::hive_parquet_query(&store.tags_dir()),
                Tbl::Tags,
            )
            .and_where(Expr::col((Tbl::Tags, Col::Type)).eq("name"));
        let name_sql = q_name.to_string(PostgresQueryBuilder);
        let name_labels: Vec<String> = store
            .conn
            .prepare(&name_sql)
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(name_labels.contains(&"sample.rs".to_string()));
        assert!(name_labels.contains(&"custom_sample".to_string()));

        // Check rank partition
        assert!(store
            .tags_origin_dir(Origin::File)
            .join("type=rank")
            .exists());
        let mut q_rank = Query::select();
        q_rank
            .column((Tbl::Tags, Col::Label))
            .from_subquery(
                util::hive_parquet_query(&store.tags_dir()),
                Tbl::Tags,
            )
            .and_where(Expr::col((Tbl::Tags, Col::Type)).eq("rank"));
        let rank_sql = q_rank.to_string(PostgresQueryBuilder);
        let rank_labels: Vec<String> = store
            .conn
            .prepare(&rank_sql)
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(rank_labels.contains(&"5".to_string()));
    }

    #[test]
    fn test_expand_modified_types_includes_directory_when_filename_changes() {
        let registry = TagRegistry::with_standard();
        let expanded = expand_modified_types(&registry, &["filename"]);
        assert!(expanded.contains(&"filename".to_string()));
        assert!(expanded.contains(&"directory".to_string()));
    }

    #[test]
    fn test_expand_modified_types_includes_name_when_filename_changes() {
        let registry = TagRegistry::with_standard();
        let expanded = expand_modified_types(&registry, &["filename"]);
        assert!(expanded.contains(&"name".to_string()), "{expanded:?}");
    }

    fn write_name_sources(store: &Store, user_name_of_item2: &str) {
        let loc_p = store.path_for_target(TargetTable::TagsByLocation);
        let user_p = store.path_for_target(TargetTable::UserTags);
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 'filename' AS type, \
                     'plain.rs' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, \
                     NULL::BOOLEAN AS label_bool \
                     UNION ALL \
                     SELECT 2::BIGINT, 'filename', 'renamed.rs', \
                     NULL::BIGINT, NULL::DOUBLE, NULL::BOOLEAN) \
                     TO '{}' (FORMAT PARQUET)",
                    loc_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 2::BIGINT AS item_id, 'name' AS type, \
                     '{}' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, \
                     NULL::BOOLEAN AS label_bool, 0::BIGINT AS rank) \
                     TO '{}' (FORMAT PARQUET)",
                    user_name_of_item2.replace('\'', "''"),
                    user_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();
    }

    #[test]
    fn test_name_partition_prefers_user_name_and_falls_back_to_filename() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        write_name_sources(&store, "custom");
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();
        assert_eq!(partition_labels(&store, Origin::User, "name"), ["custom"]);
        assert_eq!(
            partition_labels(&store, Origin::File, "name"),
            ["plain.rs"]
        );
    }

    #[test]
    fn test_name_partition_follows_filename_differential_update() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        write_name_sources(&store, "custom");
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();

        let loc_p = store.path_for_target(TargetTable::TagsByLocation);
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 'filename' AS type, \
                     'moved.rs' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, \
                     NULL::BOOLEAN AS label_bool \
                     UNION ALL \
                     SELECT 2::BIGINT, 'filename', 'renamed.rs', \
                     NULL::BIGINT, NULL::DOUBLE, NULL::BOOLEAN) \
                     TO '{}' (FORMAT PARQUET)",
                    loc_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();
        std::fs::File::create(store.complete_path()).unwrap();
        update_tags_partitions(&store, &store.conn, &registry, &["filename"])
            .unwrap();
        assert_eq!(partition_labels(&store, Origin::User, "name"), ["custom"]);
        assert_eq!(
            partition_labels(&store, Origin::File, "name"),
            ["moved.rs"]
        );
    }

    #[test]
    fn test_tags_select_query_orders_by_rank_desc_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let registry = TagRegistry::with_standard();
        let q = build_tags_select_query(&store, &registry, Some("project"));
        let sql = q.to_string(PostgresQueryBuilder);
        assert!(sql.contains("ORDER BY \"rank\" DESC"), "{sql}");
        assert!(sql.contains("\"label\" ASC"), "{sql}");
        assert!(sql.contains("\"item_id\" ASC"), "{sql}");
    }

    #[test]
    fn test_unused_registered_tag_definition_is_in_dictionary() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        let item_p = store.path_for_target(TargetTable::ItemReferences);
        let path = item_p.to_string_lossy().replace('\'', "''");
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT * FROM read_parquet('{path}') \
                     UNION ALL \
                     SELECT 99::BIGINT AS item_id, 'tag' AS item_kind, \
                     'project:orphan' AS content, \
                     'project:orphan' AS name, 7::BIGINT AS rank) \
                     TO '{path}' (FORMAT PARQUET)"
                ),
                [],
            )
            .unwrap();
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();
        let part = store
            .tags_origin_dir(Origin::User)
            .join("type=project/data.parquet");
        let labels: Vec<String> = store
            .conn
            .prepare(&format!(
                "SELECT label FROM read_parquet('{}')",
                part.display().to_string().replace('\'', "''")
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(labels.contains(&"orphan".to_string()), "{labels:?}");
    }

    #[test]
    fn test_tags_partition_physically_sorted_and_metadata_marker() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        let user_p = store.path_for_target(TargetTable::UserTags);
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 'color' AS type, \
                     'blue' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, \
                     NULL::BOOLEAN AS label_bool, 0::BIGINT AS rank \
                     UNION ALL \
                     SELECT 2::BIGINT, 'color', 'amber', \
                     NULL::BIGINT, NULL::DOUBLE, NULL::BOOLEAN, 0::BIGINT \
                     UNION ALL \
                     SELECT 3::BIGINT, 'color', 'crimson', \
                     NULL::BIGINT, NULL::DOUBLE, NULL::BOOLEAN, 0::BIGINT) \
                     TO '{}' (FORMAT PARQUET)",
                    user_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();
        let item_p = store.path_for_target(TargetTable::ItemReferences);
        let path = item_p.to_string_lossy().replace('\'', "''");
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT * FROM read_parquet('{path}') \
                     UNION ALL \
                     SELECT 20::BIGINT AS item_id, 'tag' AS item_kind, \
                     'color:crimson' AS content, \
                     'color:crimson' AS name, 9::BIGINT AS rank) \
                     TO '{path}' (FORMAT PARQUET)"
                ),
                [],
            )
            .unwrap();
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();
        let part = store
            .tags_origin_dir(Origin::User)
            .join("type=color/data.parquet");
        let rows: Vec<(i64, String, Option<i64>)> = store
            .conn
            .prepare(&format!(
                "SELECT rank, label, item_id FROM read_parquet('{}')",
                part.display().to_string().replace('\'', "''")
            ))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut expected = rows.clone();
        expected.sort_by(|a, b| {
            b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
        });
        assert_eq!(rows, expected);
        assert!(!store.tags_dir().join("metadata.parquet").exists());
    }

    #[test]
    fn test_generate_tags_does_not_write_metadata_parquet() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();
        assert!(store.tags_dir().exists());
        assert!(!store.tags_dir().join("metadata.parquet").exists());
    }

    fn partition_labels(
        store: &Store,
        origin: Origin,
        tag_type: &str,
    ) -> Vec<String> {
        let part = store
            .tags_origin_dir(origin)
            .join(format!("type={tag_type}/data.parquet"));
        store
            .conn
            .prepare(&format!(
                "SELECT label FROM read_parquet('{}')",
                part.display().to_string().replace('\'', "''")
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn test_directory_and_filename_partitions_split_by_is_dir() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        let loc_p = store.path_for_target(TargetTable::TagsByLocation);
        let base_p = store.path_for_target(TargetTable::BaseTags);
        let user_p = store.path_for_target(TargetTable::UserTags);
        let item_p = store.path_for_target(TargetTable::ItemReferences);
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 'filename' AS type, \
                     'srcfile' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, \
                     NULL::BOOLEAN AS label_bool \
                     UNION ALL \
                     SELECT 2::BIGINT, 'filename', 'src', \
                     NULL::BIGINT, NULL::DOUBLE, NULL::BOOLEAN) \
                     TO '{}' (FORMAT PARQUET)",
                    loc_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 'is_dir' AS type, \
                     NULL::VARCHAR AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, \
                     false AS label_bool, 0::BIGINT AS rank \
                     UNION ALL \
                     SELECT 2::BIGINT, 'is_dir', NULL::VARCHAR, \
                     NULL::BIGINT, NULL::DOUBLE, true, 0::BIGINT) \
                     TO '{}' (FORMAT PARQUET)",
                    base_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 'color' AS type, \
                     'blue' AS label_str, NULL::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, \
                     NULL::BOOLEAN AS label_bool, 0::BIGINT AS rank) \
                     TO '{}' (FORMAT PARQUET)",
                    user_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 10::BIGINT AS item_id, 'tag' AS item_kind, \
                     'extension:rs' AS content, 'extension:rs' AS name, \
                     5::BIGINT AS rank) \
                     TO '{}' (FORMAT PARQUET)",
                    item_p.to_string_lossy().replace('\'', "''")
                ),
                [],
            )
            .unwrap();
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();
        let dirs = partition_labels(&store, Origin::File, "directory");
        let files = partition_labels(&store, Origin::File, "filename");
        assert!(dirs.contains(&"src".to_string()), "{dirs:?}");
        assert!(!dirs.contains(&"srcfile".to_string()), "{dirs:?}");
        assert!(files.contains(&"srcfile".to_string()), "{files:?}");
        assert!(!files.contains(&"src".to_string()), "{files:?}");
    }

    #[test]
    fn test_rank_partition_prefers_user_rank_over_file_rank() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 5::BIGINT AS rank, \
                     'f1' AS file_id, 100::BIGINT AS size, \
                     1000::BIGINT AS mtime, NULL::VARCHAR AS hash, \
                     false AS is_dir \
                     UNION ALL \
                     SELECT 2::BIGINT, 3::BIGINT, 'f2', 100::BIGINT, \
                     1000::BIGINT, NULL::VARCHAR, false) \
                     TO '{}' (FORMAT PARQUET)",
                    store
                        .path_for_target(TargetTable::FileReferences)
                        .display()
                ),
                [],
            )
            .unwrap();
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT 1::BIGINT AS item_id, 'rank' AS type, \
                     NULL::VARCHAR AS label_str, 9::BIGINT AS label_int, \
                     NULL::DOUBLE AS label_double, \
                     NULL::BOOLEAN AS label_bool, 0::BIGINT AS rank) \
                     TO '{}' (FORMAT PARQUET)",
                    store.path_for_target(TargetTable::UserTags).display()
                ),
                [],
            )
            .unwrap();
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();
        assert_eq!(partition_labels(&store, Origin::File, "rank"), ["3"]);
        assert_eq!(partition_labels(&store, Origin::User, "rank"), ["9"]);
    }

    #[test]
    fn test_generate_tags_partitions_rows_by_origin() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();
        let file = store.tags_origin_dir(Origin::File);
        let user = store.tags_origin_dir(Origin::User);
        assert!(file.join("type=extension").exists());
        assert!(file.join("type=is_dir").exists());
        assert!(!file.join("type=color").exists());
        assert!(user.join("type=color").exists());
        assert!(!user.join("type=extension").exists());
    }

    #[test]
    fn test_unused_registered_definitions_partitioned_by_item_id_block() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        store
            .conn
            .execute(
                &format!(
                    "COPY (SELECT {}::BIGINT AS item_id, 'tag' AS item_kind, \
                     'project:a' AS content, 'project:a' AS name, \
                     0::BIGINT AS rank \
                     UNION ALL \
                     SELECT {}::BIGINT, 'tag', 'project:b', 'project:b', \
                     0::BIGINT \
                     UNION ALL \
                     SELECT {}::BIGINT, 'tag', 'project:c', 'project:c', \
                     0::BIGINT) \
                     TO '{}' (FORMAT PARQUET)",
                    Origin::User.block_lo() + 1,
                    Origin::Plugin.block_lo() + 1,
                    Origin::Builtin.block_lo() + 1,
                    store
                        .path_for_target(TargetTable::ItemReferences)
                        .display()
                ),
                [],
            )
            .unwrap();
        generate_tags_hive_partitioned(&store, &store.conn, &registry).unwrap();
        assert_eq!(partition_labels(&store, Origin::User, "project"), ["a"]);
        assert_eq!(partition_labels(&store, Origin::Plugin, "project"), ["b"]);
        assert_eq!(partition_labels(&store, Origin::Builtin, "project"), ["c"]);
    }

    #[test]
    fn test_update_partitions_writes_non_file_origins_without_index() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        update_tags_partitions(
            &store,
            &store.conn,
            &registry,
            &["color", "extension"],
        )
        .unwrap();
        assert!(store
            .tags_origin_dir(Origin::User)
            .join("type=color")
            .exists());
        assert!(!store.tags_origin_dir(Origin::File).exists());
    }

    #[test]
    fn test_update_partitions_writes_file_origin_when_indexed() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let registry = TagRegistry::with_standard();
        insert_dummy_test_data(&store).unwrap();
        std::fs::File::create(store.complete_path()).unwrap();
        update_tags_partitions(
            &store,
            &store.conn,
            &registry,
            &["color", "extension"],
        )
        .unwrap();
        assert!(store
            .tags_origin_dir(Origin::User)
            .join("type=color")
            .exists());
        assert!(store
            .tags_origin_dir(Origin::File)
            .join("type=extension")
            .exists());
        assert!(!store
            .tags_origin_dir(Origin::File)
            .join("type=color")
            .exists());
    }
}
