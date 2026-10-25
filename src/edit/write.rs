use super::sql::{self, UserTagDelete};
use crate::db::{Col, Store, TargetTable, Tbl};
use crate::types::{BiticalType, ItemId, Origin, SType, TagType, TypedTag};
use crate::util::{parquet_query, IdenExt, ParquetExt, SelectExt};
use anyhow::Result;
use sea_query::{Expr, Order, PostgresQueryBuilder, Query, UnionType};
use strum::IntoEnumIterator;

#[derive(Clone, Debug, PartialEq)]
pub enum WriteAction {
    Add {
        item: ItemId,
        tags: Vec<TagOp>,
    },
    Delete {
        item: ItemId,
        tags: Vec<DeleteTarget>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum TagOp {
    Append(TypedTag),
    Replace(TypedTag),
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeleteTarget {
    Type(TagType),
    Tag(TypedTag),
}

pub struct WriteResponse {
    pub updated: usize,
    pub deleted: usize,
    pub new_item_ids: Vec<i64>,
}

// ──────────────────────────────────────────────
// rank 列の解決
// ──────────────────────────────────────────────

fn builtin_default_rank(registry: &crate::tag::TagRegistry, id: i64) -> i64 {
    if Origin::within(id) != Origin::Builtin {
        return 0;
    }
    let offset = (id - Origin::Builtin.block_lo()) as u32;
    registry
        .builtin_name_for_offset(offset)
        .map(|name| crate::rank::get_rank_by_name(registry, name))
        .unwrap_or(0)
}

fn extract_rank_op(op: &TagOp) -> Option<i64> {
    let tag = match op {
        TagOp::Append(t) | TagOp::Replace(t) => t,
    };
    if tag.tag_type() != TagType::Base(SType::Rank) {
        return None;
    }
    tag.as_str().parse::<i64>().ok()
}

fn is_rank_delete(target: &DeleteTarget) -> bool {
    match target {
        DeleteTarget::Type(tt) => tt.as_str() == "rank",
        DeleteTarget::Tag(t) => t.tag_type().as_str() == "rank",
    }
}

fn rank_target_map<'a>(
    id: i64,
    item_ranks: &'a mut rustc_hash::FxHashMap<i64, i64>,
    file_ranks: &'a mut rustc_hash::FxHashMap<i64, i64>,
) -> &'a mut rustc_hash::FxHashMap<i64, i64> {
    if Origin::within(id) == Origin::File {
        file_ranks
    } else {
        item_ranks
    }
}

fn record_rank_change(
    registry: &crate::tag::TagRegistry,
    action: &WriteAction,
    item_ranks: &mut rustc_hash::FxHashMap<i64, i64>,
    file_ranks: &mut rustc_hash::FxHashMap<i64, i64>,
) {
    match action {
        WriteAction::Delete { item, tags } => {
            if !tags.iter().any(is_rank_delete) {
                return;
            }
            let id = item.as_i64();
            let rank = builtin_default_rank(registry, id);
            rank_target_map(id, item_ranks, file_ranks).insert(id, rank);
        }
        WriteAction::Add { item, tags } => {
            let Some(val) = tags.iter().find_map(extract_rank_op) else {
                return;
            };
            let id = item.as_i64();
            let rank = builtin_default_rank(registry, id) + val;
            rank_target_map(id, item_ranks, file_ranks).insert(id, rank);
        }
    }
}

fn resolve_target_ranks(
    registry: &crate::tag::TagRegistry,
    actions: &[WriteAction],
) -> (
    rustc_hash::FxHashMap<i64, i64>,
    rustc_hash::FxHashMap<i64, i64>,
) {
    let mut item_ranks = rustc_hash::FxHashMap::default();
    let mut file_ranks = rustc_hash::FxHashMap::default();
    for action in actions {
        record_rank_change(registry, action, &mut item_ranks, &mut file_ranks);
    }
    (item_ranks, file_ranks)
}

fn determine_initial_rank(
    item_ranks: &rustc_hash::FxHashMap<i64, i64>,
    registry: &crate::tag::TagRegistry,
    id: i64,
) -> i64 {
    if let Some(&rank) = item_ranks.get(&id) {
        return rank;
    }
    builtin_default_rank(registry, id)
}

type IrInsertRow = (i64, Option<i64>, Option<String>, String, String);

fn write_item_references_set(
    store: &Store,
    inserts: Vec<IrInsertRow>,
    rank_updates: Vec<(i64, i64)>,
    cascade_ids: &[i64],
) -> Result<()> {
    let path = store.path_for_target(TargetTable::ItemReferences);
    let tmp = Tbl::Target;
    sql::item_references_write(&path.to_string_lossy(), inserts, cascade_ids)
        .create_table_as(&store.conn, tmp)?;
    if !rank_updates.is_empty() {
        let staging = Tbl::Item;
        sql::rank_staging_query(rank_updates)
            .create_table_as(&store.conn, staging)?;
        sql::update_table_ranks(&store.conn, tmp, staging)?;
        staging.drop_table(&store.conn)?;
    }
    Query::select()
        .column(sea_query::Asterisk)
        .from(tmp)
        .order_by(Col::ItemId, Order::Asc)
        .to_owned()
        .save_parquet(&store.conn, &path)?;
    tmp.drop_table(&store.conn)?;
    Ok(())
}

fn write_file_and_removed_ranks_set(
    store: &Store,
    rank_updates: Vec<(i64, i64)>,
) -> Result<()> {
    for target in [TargetTable::FileReferences, TargetTable::RemovedFiles] {
        let path = store.path_for_target(target);
        if !path.exists() {
            continue;
        }
        let tmp = Tbl::Target;
        let staging = Tbl::Item;
        sql::rank_staging_query(rank_updates.clone())
            .create_table_as(&store.conn, staging)?;
        parquet_query(&path.to_string_lossy())
            .create_table_as(&store.conn, tmp)?;
        sql::update_table_ranks(&store.conn, tmp, staging)?;
        Query::select()
            .column(sea_query::Asterisk)
            .from(tmp)
            .order_by(Col::ItemId, Order::Asc)
            .to_owned()
            .save_parquet(&store.conn, &path)?;
        tmp.drop_table(&store.conn)?;
        staging.drop_table(&store.conn)?;
    }
    Ok(())
}

// ──────────────────────────────────────────────
// 公開 API
// ──────────────────────────────────────────────

pub fn write(
    store: &Store,
    registry: &crate::tag::TagRegistry,
    actions: Vec<WriteAction>,
    cast_migration: Option<(TagType, BiticalType)>,
) -> Result<WriteResponse> {
    let mut modified_types = extract_action_tag_types(&actions);
    modified_types.extend(definition_partition_types(store, &actions));
    if let Some((cast_type, _)) = &cast_migration {
        modified_types.insert(cast_type.as_str().to_string());
    }

    let cascade_ids = extract_cascade_item_ids(&actions);
    if !cascade_ids.is_empty() {
        let cascade_types = collect_types_for_items(
            &store.conn,
            store,
            registry,
            &cascade_ids,
        )?;
        modified_types.extend(cascade_types);
    }
    modified_types.retain(|t| !registry.is_excluded_from_tags(t));

    // 1. Volatile/Settling → Stored 採番
    let (resolved, new_item_ids) = resolve_volatiles(store, actions)?;
    // 2. 未実体化の組み込み型定義（Sys 区画・行なし）に kind/content/name を補う
    let resolved = inject_builtin_definitions(store, registry, resolved)?;
    modified_types.extend(definition_partition_types(store, &resolved));
    modified_types.retain(|t| !registry.is_excluded_from_tags(t));

    let (item_ranks, file_ranks) = resolve_target_ranks(registry, &resolved);

    // 3. カウント
    let (updated, deleted) = {
        let mut add: std::collections::HashMap<i64, usize> =
            std::collections::HashMap::new();
        let mut del: std::collections::HashMap<i64, usize> =
            std::collections::HashMap::new();
        for action in &resolved {
            match action {
                WriteAction::Add { item, tags } => {
                    *add.entry(item.as_i64()).or_default() += tags.len()
                }
                WriteAction::Delete { item, tags } => {
                    *del.entry(item.as_i64()).or_default() += tags.len()
                }
            }
        }
        let u: usize = add.values().sum::<usize>();
        let d: usize = del
            .iter()
            .map(|(id, &n)| n.saturating_sub(*add.get(id).unwrap_or(&0)))
            .sum();
        (u, d)
    };

    // 4. 変更を収集
    type IrInsertRow = (i64, Option<i64>, Option<String>, String, String);
    let mut ir_inserts: Vec<IrInsertRow> = vec![];
    let mut ut_inserts: Vec<(i64, TypedTag)> = vec![];
    let mut ut_deletes: Vec<UserTagDelete> = vec![];
    let mut user_cascade: Vec<i64> = vec![]; // User/System item_id カスケード削除
    let mut file_cascade: Vec<i64> = vec![]; // File item_id カスケード削除

    for action in resolved {
        match action {
            WriteAction::Add { item, tags } => {
                let item_id = item.as_i64();
                let mut item_kind: Option<String> = None;
                let mut content: Option<String> = None;
                let mut name: Option<String> = None;

                for op in tags {
                    let tag = match op {
                        TagOp::Append(t) | TagOp::Replace(t) => t,
                    };
                    match tag.tag_type() {
                        TagType::Base(SType::ItemKind) => {
                            item_kind = Some(tag.as_str())
                        }
                        TagType::Base(SType::Content) => {
                            content = Some(tag.as_str())
                        }
                        TagType::Base(SType::Name) => {
                            name = Some(tag.as_str());
                            ut_inserts.push((item_id, tag));
                        }
                        _ => ut_inserts.push((item_id, tag)),
                    }
                }

                if item_kind.is_some() || content.is_some() {
                    let rank =
                        determine_initial_rank(&item_ranks, registry, item_id);
                    let name_str = name.or_else(|| {
                        (Origin::within(item_id) == Origin::Builtin)
                            .then(|| {
                                let offset = (item_id
                                    - Origin::Builtin.block_lo())
                                    as u32;
                                registry
                                    .builtin_name_for_offset(offset)
                                    .map(|s| s.to_string())
                            })
                            .flatten()
                    });
                    ir_inserts.push((
                        item_id,
                        Some(rank),
                        name_str,
                        item_kind.unwrap_or_default(),
                        content.unwrap_or_default(),
                    ));
                }
            }
            WriteAction::Delete { item, tags } => {
                let item_id = item.as_i64();
                for target in tags {
                    match target {
                        DeleteTarget::Type(TagType::Base(SType::ItemId)) => {
                            if Origin::within(item_id) == Origin::File {
                                file_cascade.push(item_id);
                            } else {
                                user_cascade.push(item_id);
                            }
                        }
                        DeleteTarget::Type(tt) => {
                            ut_deletes.push(UserTagDelete {
                                item_id,
                                tag_type: tt.to_string(),
                                value: None,
                            })
                        }
                        DeleteTarget::Tag(tag) => {
                            ut_deletes.push(UserTagDelete {
                                item_id,
                                tag_type: tag.tag_type().to_string(),
                                value: Some(tag.value()),
                            })
                        }
                    }
                }
            }
        }
    }

    let mut seen = rustc_hash::FxHashSet::default();
    ut_inserts.retain(|(id, tag)| {
        seen.insert((*id, tag.tag_type().as_str().to_string(), tag.as_str()))
    });

    let casts: Vec<(TagType, BiticalType)> =
        cast_migration.into_iter().collect();

    // 5. 書き込み（順序固定: item_references → user_tags → rank 更新）
    if !ir_inserts.is_empty()
        || !item_ranks.is_empty()
        || !user_cascade.is_empty()
    {
        let updates: Vec<(i64, i64)> = item_ranks.into_iter().collect();
        write_item_references_set(store, ir_inserts, updates, &user_cascade)?;
    }
    let all_cascade: Vec<i64> = user_cascade
        .iter()
        .chain(file_cascade.iter())
        .copied()
        .collect();
    if !ut_inserts.is_empty()
        || !ut_deletes.is_empty()
        || !all_cascade.is_empty()
        || !casts.is_empty()
    {
        let path = store.path_for_target(TargetTable::UserTags);
        sql::user_tags_write(
            &path.to_string_lossy(),
            ut_inserts,
            ut_deletes,
            &all_cascade,
            &casts,
        )
        .save_parquet(&store.conn, &path)?;
    }
    if !file_ranks.is_empty() {
        let updates: Vec<(i64, i64)> = file_ranks.into_iter().collect();
        write_file_and_removed_ranks_set(store, updates)?;
    }
    if !file_cascade.is_empty() {
        for target in [
            TargetTable::FileReferences,
            TargetTable::Locations,
            TargetTable::TagsByLocation,
            TargetTable::BaseTags,
            TargetTable::RemovedFiles,
        ] {
            cascade_delete_from(store, target, &file_cascade)?;
        }
    }

    if !modified_types.is_empty() {
        sync_tags_partitions(store, registry, &modified_types)?;
    }

    Ok(WriteResponse {
        updated,
        deleted,
        new_item_ids,
    })
}

pub fn write_and_refresh(
    store: &Store,
    registry: &crate::tag::TagRegistry,
    actions: Vec<WriteAction>,
    cast_migration: Option<(TagType, BiticalType)>,
) -> Result<WriteResponse> {
    let resp = write(store, registry, actions, cast_migration)?;
    registry.load_type_configs(store)?;
    let all_cols = registry.get_all_columns();
    let reader = crate::query::lens_reader::Reader::build(
        registry,
        crate::db::Tbl::_OneView,
    );
    crate::oneview::OneView::recreate(
        &store.conn,
        registry,
        &all_cols,
        reader,
        &store.db_dir,
    )?;
    if resp.updated > 0 || resp.deleted > 0 {
        crate::search::clear_cache(&store.db_dir);
    }
    Ok(resp)
}

// ──────────────────────────────────────────────
// Volatile/Settling 採番
// ──────────────────────────────────────────────

fn resolve_volatiles(
    store: &Store,
    actions: Vec<WriteAction>,
) -> Result<(Vec<WriteAction>, Vec<i64>)> {
    use std::collections::HashMap;

    if let Some(item) = actions.iter().find_map(|a| match a {
        WriteAction::Delete { item, .. } if !item.is_stored() => {
            Some(item.clone())
        }
        _ => None,
    }) {
        anyhow::bail!(
            "cannot delete an unresolved item ({item}) that has not been stored yet"
        );
    }

    // Volatile は User 区画、Settling は指定された区画へ、それぞれ採番する。
    // counter 空間は共有するが variant が違うため衝突しない。
    let mut by_origin: HashMap<Origin, Vec<u64>> = HashMap::new();
    for action in &actions {
        if let WriteAction::Add { item, .. } = action {
            match item {
                ItemId::Volatile(c) => {
                    by_origin.entry(Origin::User).or_default().push(*c)
                }
                ItemId::Settling(origin, c) => {
                    by_origin.entry(*origin).or_default().push(*c)
                }
                ItemId::Stored(_) => {}
            }
        }
    }

    if by_origin.is_empty() {
        return Ok((actions, vec![]));
    }

    let mut mapping: HashMap<u64, i64> = HashMap::new();
    let mut new_ids: Vec<i64> = vec![];
    for (origin, mut counters) in by_origin {
        counters.sort_unstable();
        counters.dedup();
        let ids = crate::db::identifier::next(store, origin, counters.len())?;
        new_ids.extend(ids.iter().copied());
        mapping.extend(counters.into_iter().zip(ids));
    }

    let resolved = actions
        .into_iter()
        .map(|action| match action {
            WriteAction::Add {
                item: ItemId::Volatile(c),
                tags,
            } => WriteAction::Add {
                item: ItemId::Stored(*mapping.get(&c).unwrap()),
                tags,
            },
            WriteAction::Add {
                item: ItemId::Settling(_, c),
                tags,
            } => WriteAction::Add {
                item: ItemId::Stored(*mapping.get(&c).unwrap()),
                tags,
            },
            other => other,
        })
        .collect();

    Ok((resolved, new_ids))
}

// ──────────────────────────────────────────────
// 組み込み型定義（Sys 区画）の初回実体化
// ──────────────────────────────────────────────

// Sys 区画の id を持つが item_references にまだ行が無い Add に、
// registry から導出した kind/content/name を補って行を作れるようにする。
// 行が既にあれば触らない（通常の rank/tag 更新に任せる）。
fn inject_builtin_definitions(
    store: &Store,
    registry: &crate::tag::TagRegistry,
    actions: Vec<WriteAction>,
) -> Result<Vec<WriteAction>> {
    let sys_ids: Vec<i64> = actions
        .iter()
        .filter_map(|a| match a {
            WriteAction::Add { item, .. } if item.is_stored() => {
                let id = item.as_i64();
                (Origin::within(id) == Origin::Builtin).then_some(id)
            }
            _ => None,
        })
        .collect();

    if sys_ids.is_empty() {
        return Ok(actions);
    }

    let existing = existing_item_ids(store, &sys_ids)?;

    Ok(actions
        .into_iter()
        .map(|action| match action {
            WriteAction::Add { item, mut tags } if item.is_stored() => {
                let id = item.as_i64();
                if Origin::within(id) == Origin::Builtin
                    && !existing.contains(&id)
                {
                    let offset = (id - Origin::Builtin.block_lo()) as u32;
                    if let Some(name) = registry.builtin_name_for_offset(offset)
                    {
                        tags.push(TagOp::Append(TypedTag::new(
                            SType::ItemKind,
                            "type",
                        )));
                        tags.push(TagOp::Append(TypedTag::new(
                            SType::Content,
                            name.to_string(),
                        )));
                    }
                }
                WriteAction::Add { item, tags }
            }
            other => other,
        })
        .collect())
}

// 指定 id のうち、既に item_references に行があるものの集合を返す。
pub(super) fn existing_item_ids(
    store: &Store,
    ids: &[i64],
) -> Result<std::collections::HashSet<i64>> {
    let path = store.path_for_target(TargetTable::ItemReferences);
    if !path.exists() {
        return Ok(Default::default());
    }
    let sql = Query::select()
        .column(Col::ItemId)
        .from_subquery(
            parquet_query(&path.to_string_lossy()),
            Tbl::ItemReferences,
        )
        .and_where(Expr::col(Col::ItemId).is_in(ids.to_vec()))
        .to_owned();
    Ok(crate::query::fetcher::fetch_ids(&store.conn, &sql)?
        .into_iter()
        .collect())
}

// ──────────────────────────────────────────────
// カスケード削除（File-origin）
// ──────────────────────────────────────────────

fn cascade_delete_from(
    store: &Store,
    target: TargetTable,
    ids: &[i64],
) -> Result<()> {
    let path = store.path_for_target(target);
    if !path.exists() {
        return Ok(());
    }
    let mut q = parquet_query(&path.to_string_lossy());
    q.and_where(Expr::col(Col::ItemId).is_not_in(ids.to_vec()));
    q.save_parquet(&store.conn, &path)
}

// ──────────────────────────────────────────────
// 単体テスト（型確認）
// ──────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::TagRegistry;
    use crate::types::{SType, TagType};

    #[test]
    fn write_action_variants_are_constructible() {
        let add = WriteAction::Add {
            item: ItemId::Volatile(0),
            tags: vec![
                TagOp::Append(TypedTag::new("project", "A")),
                TagOp::Append(TypedTag::new(SType::ItemKind, "tag")),
                TagOp::Append(TypedTag::new(SType::Content, "project:A")),
            ],
        };
        assert!(matches!(add, WriteAction::Add { .. }));

        let del = WriteAction::Delete {
            item: ItemId::Stored(42),
            tags: vec![
                DeleteTarget::Type(TagType::from("project")),
                DeleteTarget::Tag(TypedTag::new("project", "A")),
            ],
        };
        assert!(matches!(del, WriteAction::Delete { .. }));
    }

    #[test]
    fn write_response_tracks_new_ids() {
        let resp = WriteResponse {
            updated: 3,
            deleted: 1,
            new_item_ids: vec![0, 1],
        };
        assert_eq!(resp.new_item_ids.len(), 2);
        assert_eq!(resp.updated, 3);
        assert_eq!(resp.deleted, 1);
    }

    #[test]
    fn delete_target_type_item_id_for_cascade() {
        let target = DeleteTarget::Type(TagType::Base(SType::ItemId));
        assert!(matches!(
            target,
            DeleteTarget::Type(TagType::Base(SType::ItemId))
        ));
    }

    #[test]
    fn rank_tag_is_stored_as_a_user_tags_row() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::tag::TagRegistry::with_standard();
        let store = Store::open(dir.path().join("db")).unwrap();
        crate::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();

        write(
            &store,
            &registry,
            vec![WriteAction::Add {
                item: ItemId::Volatile(0),
                tags: vec![TagOp::Append(TypedTag::new(SType::Rank, 9i64))],
            }],
            None,
        )
        .unwrap();

        let path = store.path_for_target(TargetTable::UserTags);
        let count: i64 = store
            .conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM read_parquet('{}') \
                     WHERE type = 'rank' AND label_int = 9",
                    path.to_string_lossy()
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_collect_types_for_items() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::tag::TagRegistry::with_standard();
        let store = Store::open(dir.path().join("db")).unwrap();
        crate::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();

        write(
            &store,
            &registry,
            vec![WriteAction::Add {
                item: ItemId::Volatile(0),
                tags: vec![TagOp::Append(TypedTag::new("category", "docs"))],
            }],
            None,
        )
        .unwrap();

        let types = collect_types_for_items(
            &store.conn,
            &store,
            &registry,
            &[ItemId::from(0)],
        )
        .unwrap();
        assert!(types.contains(&"category".to_string()));
    }

    #[test]
    fn test_write_updates_tags_partition() {
        let dir = tempfile::tempdir().unwrap();
        let registry = crate::tag::TagRegistry::with_standard();
        let store = Store::open(dir.path().join("db")).unwrap();
        crate::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();
        crate::db::tags::generate_tags_hive_partitioned(
            &store,
            &store.conn,
            &registry,
        )
        .unwrap();

        write(
            &store,
            &registry,
            vec![WriteAction::Add {
                item: ItemId::Volatile(0),
                tags: vec![TagOp::Append(TypedTag::new("category", "docs"))],
            }],
            None,
        )
        .unwrap();

        let part_dir =
            store.tags_origin_dir(Origin::User).join("type=category");
        assert!(part_dir.exists());
        assert!(part_dir.join("data.parquet").exists());
    }

    #[test]
    fn test_extract_action_tag_types_and_cascade_ids() {
        let actions = vec![
            WriteAction::Add {
                item: ItemId::from(1),
                tags: vec![TagOp::Append(TypedTag::new("project", "alpha"))],
            },
            WriteAction::Delete {
                item: ItemId::from(1),
                tags: vec![
                    DeleteTarget::Tag(TypedTag::new("status", "done")),
                    DeleteTarget::Type(TagType::Custom("priority".to_string())),
                    DeleteTarget::Type(TagType::Base(SType::ItemId)),
                ],
            },
        ];
        let types = extract_action_tag_types(&actions);
        assert!(types.contains("project"));
        assert!(types.contains("status"));
        assert!(types.contains("priority"));
        assert!(!types.contains("item_id"));

        let cascade_ids = extract_cascade_item_ids(&actions);
        assert_eq!(cascade_ids, vec![ItemId::from(1)]);
    }

    #[test]
    fn test_write_filters_excluded_tags() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        crate::indexing::Indexer::new(&store, &TagRegistry::with_standard())
            .initialize_tables()
            .unwrap();

        let registry = TagRegistry::with_standard();
        let actions = vec![WriteAction::Add {
            item: ItemId::from(1),
            tags: vec![TagOp::Append(TypedTag::new("mtime", "2026-01-01"))],
        }];

        write(&store, &registry, actions, None).unwrap();
        assert!(!store
            .tags_origin_dir(Origin::File)
            .join("type=mtime")
            .exists());
    }

    #[test]
    fn test_sync_tags_partitions_cleanup_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        // create a dummy corrupted partition dir
        let part_dir = store.tags_origin_dir(Origin::User).join("type=broken");
        std::fs::create_dir_all(&part_dir).unwrap();
        assert!(part_dir.exists());

        let mut modified = rustc_hash::FxHashSet::default();
        modified.insert("broken".to_string());
        let reg = crate::tag::TagRegistry::with_standard();
        let res = sync_tags_partitions(&store, &reg, &modified);
        assert!(res.is_err());
        assert!(part_dir.exists());
        assert!(store.tags_dir().exists());
    }

    #[test]
    fn test_sync_tags_partitions_keeps_partition_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let part_dir = store.tags_origin_dir(Origin::User).join("type=broken");
        std::fs::create_dir_all(&part_dir).unwrap();
        std::fs::write(part_dir.join("data.parquet"), b"keep").unwrap();
        let tmp_part = store.temp_tags_dir().join("type=broken");
        std::fs::create_dir_all(&tmp_part).unwrap();
        std::fs::write(tmp_part.join("data.parquet.tmp"), b"tmp").unwrap();
        let mut modified = rustc_hash::FxHashSet::default();
        modified.insert("broken".to_string());
        let reg = crate::tag::TagRegistry::with_standard();
        let res = sync_tags_partitions(&store, &reg, &modified);
        assert!(res.is_err());
        assert!(part_dir.join("data.parquet").exists());
        assert!(store.tags_dir().exists());
        assert!(!tmp_part.exists());
    }

    #[test]
    fn test_definition_partition_types_from_actions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let actions = vec![
            WriteAction::Add {
                item: ItemId::Volatile(0),
                tags: vec![
                    TagOp::Append(TypedTag::new(SType::ItemKind, "tag")),
                    TagOp::Append(TypedTag::new(SType::Content, "project:foo")),
                ],
            },
            WriteAction::Add {
                item: ItemId::Volatile(1),
                tags: vec![
                    TagOp::Append(TypedTag::new(SType::ItemKind, "type")),
                    TagOp::Append(TypedTag::new(SType::Content, "score")),
                ],
            },
        ];
        let types = definition_partition_types(&store, &actions);
        assert!(types.contains("project"));
        assert!(types.contains("type"));
    }

    fn partition_labels(store: &Store, tag_type: &str) -> Vec<String> {
        let part = store
            .tags_origin_dir(Origin::User)
            .join(format!("type={tag_type}/data.parquet"));
        if !part.exists() {
            return Vec::new();
        }
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
    fn test_definition_item_write_refreshes_tag_partition() {
        let dir = tempfile::tempdir().unwrap();
        let registry = TagRegistry::with_standard();
        let store = Store::open(dir.path().join("db")).unwrap();
        crate::indexing::Indexer::new(&store, &registry)
            .initialize_tables()
            .unwrap();
        crate::db::tags::generate_tags_hive_partitioned(
            &store,
            &store.conn,
            &registry,
        )
        .unwrap();
        let resp = write(
            &store,
            &registry,
            vec![WriteAction::Add {
                item: ItemId::Volatile(0),
                tags: vec![
                    TagOp::Append(TypedTag::new(SType::ItemKind, "tag")),
                    TagOp::Append(TypedTag::new(
                        SType::Content,
                        "project:orphan",
                    )),
                ],
            }],
            None,
        )
        .unwrap();
        let labels = partition_labels(&store, "project");
        assert!(labels.contains(&"orphan".to_string()), "{labels:?}");
        let id = resp.new_item_ids[0];
        write(
            &store,
            &registry,
            vec![WriteAction::Delete {
                item: ItemId::Stored(id),
                tags: vec![DeleteTarget::Type(TagType::Base(SType::ItemId))],
            }],
            None,
        )
        .unwrap();
        let labels = partition_labels(&store, "project");
        assert!(!labels.contains(&"orphan".to_string()), "{labels:?}");
    }
}

pub(crate) fn collect_types_for_items(
    conn: &duckdb::Connection,
    store: &Store,
    registry: &crate::tag::TagRegistry,
    item_ids: &[ItemId],
) -> Result<Vec<String>> {
    if item_ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = item_ids.iter().map(|id| id.as_i64()).collect();

    let mut queries = Vec::new();

    let loc_path = store.path_for_target(TargetTable::TagsByLocation);
    if loc_path.exists() {
        let mut loc_q = Query::select();
        loc_q
            .column(Col::Type)
            .from_subquery(
                crate::util::parquet_query(&loc_path.to_string_lossy()),
                Tbl::TagsByLocation,
            )
            .and_where(Expr::col(Col::ItemId).is_in(ids.clone()));
        queries.push(loc_q);
    }

    let user_path = store.path_for_target(TargetTable::UserTags);
    if user_path.exists() {
        let mut user_q = Query::select();
        user_q
            .column(Col::Type)
            .from_subquery(
                crate::util::parquet_query(&user_path.to_string_lossy()),
                Tbl::UserTags,
            )
            .and_where(Expr::col(Col::ItemId).is_in(ids.clone()));
        queries.push(user_q);
    }

    let base_path = store.path_for_target(TargetTable::BaseTags);
    if base_path.exists() {
        let mut base_q = Query::select();
        base_q
            .column(Col::Type)
            .from_subquery(
                crate::util::parquet_query(&base_path.to_string_lossy()),
                Tbl::BaseTags,
            )
            .and_where(Expr::col(Col::ItemId).is_in(ids));
        let excluded = registry.get_excluded_from_tags();
        if !excluded.is_empty() {
            base_q.and_where(Expr::col(Col::Type).is_not_in(excluded));
        }
        queries.push(base_q);
    }

    if queries.is_empty() {
        return Ok(Vec::new());
    }

    let mut combined_q = queries.remove(0);
    for q in queries {
        combined_q.union(UnionType::All, q);
    }

    let mut q = Query::select();
    q.distinct()
        .column(Col::Type)
        .from_subquery(combined_q, Tbl::ItemTags);

    let sql = q.to_string(PostgresQueryBuilder);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect::<duckdb::Result<Vec<_>>>().map_err(Into::into)
}

fn push_definition_type(
    out: &mut rustc_hash::FxHashSet<String>,
    kind: &str,
    content: &str,
) {
    match kind {
        "tag" => {
            let Some((ty, label)) = content.split_once(':') else {
                return;
            };
            if !ty.is_empty() && !label.is_empty() {
                out.insert(ty.to_string());
            }
        }
        "type" if !content.is_empty() => {
            out.insert("type".to_string());
        }
        _ => {}
    }
}

fn push_stored_definition(
    store: &Store,
    id: i64,
    out: &mut rustc_hash::FxHashSet<String>,
) {
    let path = store.path_for_target(TargetTable::ItemReferences);
    if !path.exists() {
        return;
    }
    let mut q = Query::select();
    q.column(Col::ItemKind)
        .column(Col::Content)
        .from_subquery(
            parquet_query(&path.to_string_lossy()),
            Tbl::ItemReferences,
        )
        .and_where(Expr::col(Col::ItemId).eq(id));
    let sql = q.to_string(PostgresQueryBuilder);
    let Ok(mut stmt) = store.conn.prepare(&sql) else {
        return;
    };
    let Ok(mut rows) = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    }) else {
        return;
    };
    if let Some(Ok((kind, content))) = rows.next() {
        push_definition_type(out, &kind, &content);
    }
}

fn definition_kind_and_content(
    tags: &[TagOp],
) -> (Option<String>, Option<String>) {
    let mut kind = None;
    let mut content = None;
    for op in tags {
        let tag = match op {
            TagOp::Append(t) | TagOp::Replace(t) => t,
        };
        match tag.tag_type() {
            TagType::Base(SType::ItemKind) => kind = Some(tag.as_str()),
            TagType::Base(SType::Content) => content = Some(tag.as_str()),
            _ => {}
        }
    }
    (kind, content)
}

fn definition_partition_types(
    store: &Store,
    actions: &[WriteAction],
) -> rustc_hash::FxHashSet<String> {
    let mut out = rustc_hash::FxHashSet::default();
    for action in actions {
        match action {
            WriteAction::Add { item, tags } => {
                let (kind, content) = definition_kind_and_content(tags);
                if let (Some(k), Some(c)) = (kind, content) {
                    push_definition_type(&mut out, &k, &c);
                }
                if let ItemId::Stored(id) = item {
                    push_stored_definition(store, *id, &mut out);
                }
            }
            WriteAction::Delete { item, tags } => {
                for target in tags {
                    let DeleteTarget::Tag(tag) = target else {
                        continue;
                    };
                    if tag.tag_type() != TagType::Base(SType::Content) {
                        continue;
                    }
                    push_definition_type(&mut out, "tag", &tag.as_str());
                }
                if let ItemId::Stored(id) = item {
                    push_stored_definition(store, *id, &mut out);
                }
            }
        }
    }
    out
}

fn extract_action_tag_types(
    actions: &[WriteAction],
) -> rustc_hash::FxHashSet<String> {
    let mut types = rustc_hash::FxHashSet::default();
    for action in actions {
        match action {
            WriteAction::Add { tags, .. } => {
                for tag_op in tags {
                    let tag = match tag_op {
                        TagOp::Append(t) | TagOp::Replace(t) => t,
                    };
                    types.insert(tag.tag_type().as_str().to_string());
                }
            }
            WriteAction::Delete { tags, .. } => {
                for target in tags {
                    match target {
                        DeleteTarget::Tag(t) => {
                            types.insert(t.tag_type().as_str().to_string());
                        }
                        DeleteTarget::Type(tt) => {
                            if tt.as_str() != "item_id" {
                                types.insert(tt.as_str().to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    types
}

fn extract_cascade_item_ids(actions: &[WriteAction]) -> Vec<ItemId> {
    let mut ids = Vec::new();
    for action in actions {
        if let WriteAction::Delete { item, tags } = action {
            for target in tags {
                if matches!(
                    target,
                    DeleteTarget::Type(TagType::Base(SType::ItemId))
                ) {
                    ids.push(*item);
                }
            }
        }
    }
    ids
}

fn sync_tags_partitions(
    store: &Store,
    registry: &crate::tag::TagRegistry,
    modified_types: &rustc_hash::FxHashSet<String>,
) -> Result<()> {
    let types_vec: Vec<&str> =
        modified_types.iter().map(|s| s.as_str()).collect();
    let result = crate::db::tags::update_tags_partitions(
        store,
        &store.conn,
        registry,
        &types_vec,
    );
    if result.is_err() {
        if store.temp_tags_dir().exists() {
            let _ = std::fs::remove_dir_all(store.temp_tags_dir());
        }
        for origin in Origin::iter() {
            for tag_type in &types_vec {
                let tmp_file = store
                    .tags_origin_dir(origin)
                    .join(format!("type={tag_type}"))
                    .join("data.parquet.tmp");
                if tmp_file.exists() {
                    let _ = std::fs::remove_file(tmp_file);
                }
            }
        }
    }
    result
}
