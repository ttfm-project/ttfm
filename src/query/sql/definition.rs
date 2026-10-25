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

//! 定義アイテム（`type:*` / `tag:"X"` 等の完全一致検索・glob検索）の SQL 構築。
//! pick パスからは low_dispatcher、fetch / count パスからは lens_builder の
//! `add_definitions` / `count_definitions` 経由で使用する。

use crate::db::{
    BiticalType, Col, CustomFunc,
    Pronoun::{Agg, Representative, Scalar, Stored, Sub, Volatile},
    QueryResultCol, Src, Val,
};
use crate::query::ast::Candidate;
use crate::query::lens_resolver::{DefinitionStorage, ResolvedOrder};
use crate::types::{Bitical, ItemId, ItemKind, Origin, Rank, TypedTag};
use sea_query::{
    BinOper, CaseStatement, CommonTableExpression, Expr, ExprTrait, Func,
    JoinType, Query, SelectStatement, UnionType, WithClause,
};
use std::path::{Path, PathBuf};

const UNREGISTERED_DEFINITION_RANK_OFFSET: Rank = 20;

/// 定義アイテム参照の Resolved 段。展開後の木（定義型の `TypedTag` か
/// `QueryNode::OriginRef`）に registry 照合結果（candidates / reserved）と
/// name 絞り込み（pattern / exact）を加えたもので、本ファイルの SQL 構築関数群が
/// そのまま消費する。
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedDefinition {
    pub kind: ItemKind,
    pub value: TypedTag,
    pub pattern: String,
    pub exact: bool,
    pub candidates: Vec<Candidate>,
    pub origins: Vec<Origin>,
    pub reserved: Vec<String>,
    pub recorded: bool,
}

impl ResolvedDefinition {
    /// 定義型のタグ（`type:"X"` / `tag:"X"`）由来。
    pub(crate) fn for_tag(
        kind: ItemKind,
        value: TypedTag,
        candidates: Vec<Candidate>,
        reserved: Vec<String>,
        default_rank: Rank,
    ) -> Self {
        Self::build(
            kind,
            value,
            Vec::new(),
            true,
            candidates,
            reserved,
            default_rank,
        )
    }

    /// id 区画の参照（`origin:system`）由来。区画内の型定義を全て指す。
    pub(crate) fn for_origin(
        origin: Origin,
        candidates: Vec<Candidate>,
        reserved: Vec<String>,
        default_rank: Rank,
    ) -> Self {
        Self::build(
            ItemKind::Type,
            TypedTag::new(crate::types::SType::Type, "*"),
            vec![origin],
            false,
            candidates,
            reserved,
            default_rank,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        kind: ItemKind,
        value: TypedTag,
        origins: Vec<Origin>,
        recorded: bool,
        candidates: Vec<Candidate>,
        reserved: Vec<String>,
        default_rank: Rank,
    ) -> Self {
        // glob メタ文字を含まない名前は完全一致検索(未登録名なら定義作成の候補になる)。
        let (pattern, exact, candidates) =
            if let Bitical::String(pattern) = value.value() {
                if crate::util::is_glob_pattern(&pattern) {
                    (pattern, false, candidates)
                } else {
                    exact_match_candidate(pattern, &candidates, default_rank)
                }
            } else {
                let v = value.value().as_display_name();
                exact_match_candidate(v, &candidates, default_rank)
            };
        Self {
            kind,
            value,
            pattern,
            exact,
            candidates,
            origins,
            reserved,
            recorded,
        }
    }

    pub(crate) fn unregistered(&self) -> Self {
        Self {
            recorded: false,
            ..self.clone()
        }
    }
}

/// 完全一致検索用の (pattern, exact=true, candidates) を組み立てる。
/// candidates に無ければ、未登録・未使用の名前でも合成できるよう
/// User 区画の Volatile プレースホルダを合成する。
fn exact_match_candidate(
    pattern: String,
    candidates: &[Candidate],
    default_rank: Rank,
) -> (String, bool, Vec<Candidate>) {
    let entry = candidates
        .iter()
        .find(|c| c.name == pattern)
        .cloned()
        .unwrap_or_else(|| Candidate {
            name: pattern.clone(),
            rank: default_rank,
            id: ItemId::Settling(Origin::User, 0),
        });
    (pattern, true, vec![entry])
}

/// 定義アイテムの行本体（item_id, name, rank, item_kind）。
///
/// - Stored 定義行（ユーザー編集済み。同名の Volatile 合成に常に優先）
/// - 展開段階で candidates にされた登録型名＋default_rank
/// - データ中で使用中の型（type 定義）／タグ（tag 定義、user 由来）。
///   rank は既定値。candidates に同名があればそちらの default_rank を優先
///
/// 合成 Volatile 行の item_id は、candidates の3要素目（出所を表す ItemId）が
/// `Stored`（組み込み、with_standard 由来）ならその固定 Sys id を
/// そのままクエリに埋め込み、`Settling`（プラグイン登録、またはデータ中で使用中の未登録型/タグ）は
/// NULL。NULL の行は fetch 後、fetcher 側で `ItemId::new_volatile()` により
/// 一意な揮発 id を採番し、Settling の場合は続けて origin タグから
/// `settle()` される。
fn assemble_definition_rows(
    resolved: &ResolvedDefinition,
    mut with_clause: WithClause,
    used_q: Option<SelectStatement>,
) -> SelectStatement {
    let ResolvedDefinition {
        kind,
        candidates,
        recorded,
        ..
    } = resolved;
    let kind = *kind;
    let recorded = *recorded;

    // candidates（定数リスト）∪ データ中で使用中の型/タグ。組み込み（Stored）は
    // candidates の 3要素目に固定 Sys id を持つ（プラグイン登録・データ中で
    // 使用中の Settling は NULL）。
    let mut raw_q: Option<SelectStatement> = None;
    for c in candidates {
        let sys_id: Option<i64> = c.id.is_stored().then(|| c.id.as_i64());
        let row = Query::select()
            .expr_as(Expr::val(c.name.as_str()), Col::Name)
            .expr_as(Expr::val(c.rank), Col::Rank)
            .expr_as(Expr::val(sys_id), Col::ItemId)
            .to_owned();
        match &mut raw_q {
            Some(q) => {
                q.union(UnionType::All, row);
            }
            None => raw_q = Some(row),
        }
    }
    let raw_q = match (raw_q, used_q) {
        (Some(mut q), Some(used)) => {
            q.union(UnionType::All, used);
            q
        }
        (Some(q), None) => q,
        (None, Some(used)) => used,
        (None, None) => Query::select()
            .expr_as(Expr::val(""), Col::Name)
            .expr_as(Expr::val(0i64), Col::Rank)
            .expr_as(Expr::val(None::<i64>), Col::ItemId)
            .and_where(Expr::val(1).eq(0))
            .to_owned(),
    };

    // Stored に同名がある候補を除外（Stored 優先の anti-join）。item_id は
    // 組み込みなら固定 Sys id、それ以外（プラグイン登録・データ中で使用中）は NULL
    // （raw_q からそのまま引き継ぐ）。
    let volatile_q = Query::select()
        .column(Col::ItemId)
        .column(Col::Name)
        .column(Col::Rank)
        .from_subquery(raw_q, Sub)
        .and_where(Expr::col(Col::Name).not_in_subquery(
            Query::select().column(Col::Name).from(Stored).to_owned(),
        ))
        .to_owned();
    with_clause.cte(
        CommonTableExpression::new()
            .query(volatile_q)
            .table_name(Volatile)
            .to_owned(),
    );

    // 固定 Sys id を持つ組み込み（item_id 非 NULL）は仕様上 Stored なので
    // item_kind は kind そのもの。id 未確定（Settling/未登録）のみ volatile。
    let volatile_sel = Query::select()
        .column(Col::ItemId)
        .column(Col::Name)
        .expr_as(
            Expr::col(Col::Rank).sub(UNREGISTERED_DEFINITION_RANK_OFFSET),
            Col::Rank,
        )
        .expr_as(
            Expr::case(
                Expr::col(Col::ItemId).is_not_null(),
                Expr::val(kind.as_str()),
            )
            .finally(Expr::val(ItemKind::Volatile.as_str())),
            Col::ItemKind,
        )
        .from(Volatile)
        .to_owned();

    // recorded=false（OriginFn 経由）は Stored 行を出力しない。ColumnMatch 枝が
    // 物理化済み item を既に拾うため、DefinitionRef 枝は未物理化の Volatile 合成
    // のみ担えばよい（Stored CTE 自体は上の anti-join に必要なので残す）。
    let mut q = if recorded {
        let mut stored_sel = Query::select();
        stored_sel
            .column(Col::ItemId)
            .column(Col::Name)
            .column(Col::Rank)
            .expr_as(Expr::val(kind.as_str()), Col::ItemKind)
            .from(Stored);
        stored_sel.union(UnionType::All, volatile_sel);
        stored_sel
    } else {
        volatile_sel
    };
    q.with_cte(with_clause);
    q
}

/// 定義アイテムの name 列に対する絞り込み条件を返す。
/// メタ文字を含む名前は glob パターンマッチ、含まない名前は等値比較。
fn name_condition(pattern: &str, exact: bool) -> sea_query::SimpleExpr {
    name_condition_on(Col::Name, pattern, exact)
}

fn name_condition_on(
    col: Col,
    pattern: &str,
    exact: bool,
) -> sea_query::SimpleExpr {
    if exact {
        Expr::col(col).eq(pattern)
    } else {
        Expr::col(col).binary(BinOper::Custom("GLOB"), Expr::val(pattern))
    }
}

/// 登録済み定義アイテムだけを name で絞り込んだ pick SQL。
/// 未登録（Volatile）行は oneview のアイテムにしない。
pub(crate) fn build_registered_definition_pick_sql(
    resolved: &ResolvedDefinition,
    storage: &DefinitionStorage,
) -> SelectStatement {
    let mut q = Query::select()
        .column(Col::ItemId)
        .column(Col::Rank)
        .expr_as(Expr::val(resolved.kind.as_str()), Col::ItemKind)
        .from(Stored)
        .and_where(name_condition(&resolved.pattern, resolved.exact))
        .to_owned();
    if !resolved.recorded {
        q.and_where(Expr::val(1).eq(0));
    }
    q.with_cte(stored_cte(
        storage.item_references.as_deref(),
        resolved.kind,
    ));
    q
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DefinitionBranch {
    def: ResolvedDefinition,
    filters: Vec<NameFilter>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NameFilter {
    In(Vec<DefinitionBranch>),
    NotIn(Vec<DefinitionBranch>),
}

impl DefinitionBranch {
    pub(crate) fn new(def: ResolvedDefinition) -> Self {
        Self {
            def,
            filters: Vec::new(),
        }
    }

    pub(crate) fn filtered(mut self, filter: NameFilter) -> Self {
        self.filters.push(filter);
        self
    }

    pub(crate) fn names_sql(
        &self,
        storage: &DefinitionStorage,
    ) -> SelectStatement {
        let mut q = build_definition_name_sql(&self.def, storage);
        apply_name_filters(
            &mut q,
            Expr::col(Col::Name).into(),
            &self.filters,
            storage,
        );
        q
    }

    pub(crate) fn fetch_sql(
        &self,
        src: &Src,
        storage: &DefinitionStorage,
        short: bool,
    ) -> SelectStatement {
        let mut q = build_definition_fetch_sql(src, &self.def, storage, short);
        apply_name_filters(
            &mut q,
            Expr::col((Sub, Col::Name)).into(),
            &self.filters,
            storage,
        );
        q
    }
}

fn names_union_sql(
    branches: &[DefinitionBranch],
    storage: &DefinitionStorage,
) -> SelectStatement {
    branches
        .iter()
        .map(|b| b.names_sql(storage))
        .reduce(union_distinct)
        .unwrap()
}

fn union_distinct(
    mut acc: SelectStatement,
    next: SelectStatement,
) -> SelectStatement {
    acc.union(UnionType::Distinct, next);
    acc
}

fn apply_name_filters(
    q: &mut SelectStatement,
    col: sea_query::SimpleExpr,
    filters: &[NameFilter],
    storage: &DefinitionStorage,
) {
    for filter in filters {
        match filter {
            NameFilter::In(bs) => {
                q.and_where(
                    col.clone().in_subquery(names_union_sql(bs, storage)),
                );
            }
            NameFilter::NotIn(bs) => {
                q.and_where(
                    col.clone().not_in_subquery(names_union_sql(bs, storage)),
                );
            }
        }
    }
}

/// 定義アイテムを name で絞り込んだ fetch SQL（tags・representative・name 付き）。
/// 列順は src 由来の items fetch（definition union 用の列追加後）と位置を
/// 揃える: item_id, rank, item_kind, tags, representative, name。
/// name 列は UNION 後の並び替え専用（デコードには使わない）。
/// tags は DB にタグ行があるか（実タグの LEFT JOIN 結果が NULL か）で
/// 判別する。DBから取得した定義行は必ずタグ行（content 行）を持つため
/// 実 EAV タグを載せ、タグ行を持たない行（組み込みの固定 Sys id・
/// 使用中の型/タグ等）は type/name/origin タグをクエリ時に組み立てる。
/// limit/offset は適用しない（Or で他枝と UNION した後にまとめて適用するため、
/// 呼び出し側が責任を持つ）。
pub(crate) fn build_definition_fetch_sql(
    src: &Src,
    resolved: &ResolvedDefinition,
    storage: &DefinitionStorage,
    short: bool,
) -> SelectStatement {
    let mut q = Query::select();
    q.column((Sub, Col::ItemId))
        .column((Sub, Col::Rank))
        .column((Sub, Col::ItemKind));
    if short {
        q.expr_as(CustomFunc::empty_tag_list(), QueryResultCol::Tags);
    } else {
        let kind = resolved.kind;
        let candidates = &resolved.candidates;
        let union_val = CustomFunc::eav_union_value();
        let struct_expr = CustomFunc::struct_pack_tag(
            Expr::col(Col::Type).into(),
            union_val,
            Expr::col(Col::Origin).into(),
        );
        let tags_agg_sql = Query::select()
            .column(Col::ItemId)
            .expr_as(CustomFunc::list(struct_expr), QueryResultCol::Tags)
            .from(src)
            .group_by_col(Col::ItemId)
            .to_owned();
        let builtin: &'static str = Val::Builtin.into();
        let user: &'static str = Val::User.into();
        let plugin_names: Vec<&str> = candidates
            .iter()
            .filter(|c| {
                matches!(
                    c.id,
                    ItemId::Settling(crate::types::Origin::Plugin, _)
                )
            })
            .map(|c| c.name.as_str())
            .collect();
        let mut origin_value = CaseStatement::new();
        if !plugin_names.is_empty() {
            let plugin: &'static str = Val::Plugin.into();
            origin_value = origin_value
                .case(Expr::col((Sub, Col::Name)).is_in(plugin_names), plugin);
        }
        let origin_value = origin_value
            .case(Expr::col((Sub, Col::ItemId)).is_not_null(), builtin)
            .finally(user);
        let volatile_type_tag = CustomFunc::list_value([
            CustomFunc::struct_pack_tag(
                Expr::val(Col::Type.as_str()).into(),
                CustomFunc::union_value(
                    BiticalType::String,
                    Expr::val(kind.as_str()),
                ),
                Expr::val(builtin).into(),
            ),
            CustomFunc::struct_pack_tag(
                Expr::val(Col::Name.as_str()).into(),
                CustomFunc::union_value(
                    BiticalType::String,
                    Expr::col((Sub, Col::Name)),
                ),
                Expr::val(builtin).into(),
            ),
            CustomFunc::struct_pack_tag(
                Expr::val(Col::Origin.as_str()).into(),
                CustomFunc::union_value(BiticalType::String, origin_value),
                Expr::val(builtin).into(),
            ),
        ]);
        let attach_item_tags = storage.attach_item_tags
            && (storage.item_references.is_some()
                || storage.tags_dir.is_some());
        if !attach_item_tags {
            q.expr_as(volatile_type_tag, QueryResultCol::Tags);
            q.expr_as(
                CustomFunc::as_representative(Expr::col((Sub, Col::Name))),
                Representative,
            )
            .column((Sub, Col::Name))
            .from_subquery(build_definition_rows(resolved, storage), Sub)
            .and_where(name_condition(&resolved.pattern, resolved.exact));
            return q;
        }
        let stored_tags = Func::coalesce([
            Expr::col((Agg, QueryResultCol::Tags)).into(),
            CustomFunc::list_value([]),
        ]);
        q.expr_as(
            Expr::case(
                Expr::col((Agg, QueryResultCol::Tags)).is_null(),
                volatile_type_tag,
            )
            .finally(stored_tags),
            QueryResultCol::Tags,
        );
        q.expr_as(
            CustomFunc::as_representative(Expr::col((Sub, Col::Name))),
            Representative,
        )
        .column((Sub, Col::Name))
        .from_subquery(build_definition_rows(resolved, storage), Sub)
        .join_subquery(
            JoinType::LeftJoin,
            tags_agg_sql,
            Agg,
            Expr::col((Sub, Col::ItemId)).equals((Agg, Col::ItemId)),
        )
        .and_where(name_condition(&resolved.pattern, resolved.exact));
        return q;
    }
    q.expr_as(
        CustomFunc::as_representative(Expr::col((Sub, Col::Name))),
        Representative,
    )
    .column((Sub, Col::Name))
    .from_subquery(build_definition_rows(resolved, storage), Sub)
    .and_where(name_condition(&resolved.pattern, resolved.exact));
    q
}

/// 定義アイテムを name で絞り込んだ、name 1列のみの SQL（カウント用）。
/// 複数枝を UNION (Distinct) で束ねて枝間の name 重複を排除するために使う。
pub(crate) fn build_definition_name_sql(
    resolved: &ResolvedDefinition,
    storage: &DefinitionStorage,
) -> SelectStatement {
    Query::select()
        .column(Col::Name)
        .from_subquery(build_definition_rows(resolved, storage), Sub)
        .and_where(name_condition(&resolved.pattern, resolved.exact))
        .to_owned()
}

/// 定義枝（複数可）が指す定義アイテムの distinct 件数を返すスカラー SQL。
/// 各枝は build_definition_name_sql と同一の列（name 1列）であること。
/// UNION (Distinct) で枝間の name 重複を排除済みのため COUNT(*) でよい。
pub(crate) fn build_definitions_count_sql(
    branches: Vec<SelectStatement>,
) -> SelectStatement {
    let unioned = branches
        .into_iter()
        .reduce(|mut acc, next| {
            acc.union(UnionType::Distinct, next);
            acc
        })
        .expect("branches must not be empty");
    Query::select()
        .expr_as(CustomFunc::count_star(), Scalar)
        .from_subquery(unioned, Sub)
        .to_owned()
}

/// 定義枝（と src 由来枝）の fetch SQL 群を UNION で束ね、並び替えと
/// limit/offset を UNION 後に適用する。各枝は build_definition_fetch_sql と
/// 同一の列順であること。
///
/// 並び替えは `orders`（明示的な並び順）を先頭に適用し、既定の並び
/// （rank DESC, name ASC NULLS LAST, item_id DESC NULLS LAST）を
/// タイブレーカーとして残す。name は定義行では EAV タグではなく列として
/// 持つ（id 未確定の行はタグ行を持たない）ため、name キーは列に読み替える。
pub(crate) fn build_add_definitions_sql(
    src: &Src,
    branches: Vec<SelectStatement>,
    orders: &[crate::query::lens_resolver::ResolvedOrder],
    n: usize,
    offset: usize,
) -> SelectStatement {
    use super::util::wrap_in_subquery_star;
    use crate::query::lens_resolver::{ResolvedOrder, ResolvedOrderKey};
    use sea_query::{NullOrdering, Order};

    let unioned = branches
        .into_iter()
        .map(wrap_in_subquery_star)
        .reduce(|mut acc, next| {
            acc.union(UnionType::Distinct, next);
            acc
        })
        .expect("branches must not be empty");

    let mut q = if orders.is_empty() {
        unioned
    } else {
        // EAV タグ値の相関サブクエリから UNION 行の item_id を参照するため
        // サブクエリで包んで別名を付ける。
        use crate::db::Pronoun::View;
        let mut outer = Query::select()
            .column(sea_query::Asterisk)
            .from_subquery(unioned, View)
            .to_owned();
        let mapped: Vec<ResolvedOrder> = orders
            .iter()
            .map(|o| match &o.key {
                ResolvedOrderKey::Tag { tag_type, .. }
                    if tag_type == Col::Name.as_str() =>
                {
                    ResolvedOrder {
                        key: ResolvedOrderKey::Column(Col::Name),
                        desc: o.desc,
                    }
                }
                _ => o.clone(),
            })
            .collect();
        super::order::apply_resolved_order(
            &mut outer,
            &mapped,
            src,
            Expr::col((View, Col::ItemId)).into(),
        );
        outer
    };
    q.order_by(Col::Rank, Order::Desc)
        .order_by_with_nulls(Col::Name, Order::Asc, NullOrdering::Last)
        .order_by_with_nulls(Col::ItemId, Order::Desc, NullOrdering::Last);
    if n > 0 {
        q.limit((n + 1) as u64);
    }
    if offset > 0 {
        q.offset(offset as u64);
    }
    q
}

fn stored_cte(item_references: Option<&Path>, kind: ItemKind) -> WithClause {
    use crate::db::Tbl;
    use crate::util;

    let mut with_clause = WithClause::new();
    let stored_q = if let Some(ir_path) = item_references {
        Query::select()
            .column(Col::ItemId)
            .expr_as(Expr::col(Col::Content), Col::Name)
            .expr_as(
                Func::coalesce([
                    Expr::col(Col::Rank).into(),
                    Expr::val(0).into(),
                ]),
                Col::Rank,
            )
            .from_subquery(
                util::parquet_query(&ir_path.to_string_lossy()),
                Tbl::ItemReferences,
            )
            .and_where(Expr::col(Col::ItemKind).eq(kind.as_str()))
            .to_owned()
    } else {
        Query::select()
            .expr_as(Expr::val(None::<i64>), Col::ItemId)
            .expr_as(Expr::val(""), Col::Name)
            .expr_as(Expr::val(0i64), Col::Rank)
            .and_where(Expr::val(1).eq(0))
            .to_owned()
    };
    with_clause.cte(
        CommonTableExpression::new()
            .query(stored_q)
            .table_name(Stored)
            .to_owned(),
    );
    with_clause
}

fn used_names(tags: SelectStatement, name_col: Col) -> SelectStatement {
    use crate::db::Tbl;

    Query::select()
        .distinct()
        .expr_as(Expr::col(name_col), Col::Name)
        .expr_as(Expr::val(crate::rank::SystemRank::DEFAULT), Col::Rank)
        .expr_as(Expr::val(None::<i64>), Col::ItemId)
        .from_subquery(tags, Tbl::Tags)
        .to_owned()
}

pub(crate) fn build_definition_rows(
    resolved: &ResolvedDefinition,
    storage: &DefinitionStorage,
) -> SelectStatement {
    let with_clause =
        stored_cte(storage.item_references.as_deref(), resolved.kind);
    let name_col = if matches!(resolved.kind, ItemKind::Tag) {
        Col::TypedTag
    } else {
        Col::Type
    };
    let origins =
        (!resolved.origins.is_empty()).then_some(resolved.origins.as_slice());
    let used_q = storage.tags_query(None, origins).map(|t| {
        let mut q = used_names(t, name_col);
        let excluded: Vec<String> = resolved
            .candidates
            .iter()
            .map(|c| c.name.clone())
            .chain(resolved.reserved.iter().cloned())
            .collect();
        if !excluded.is_empty() {
            q.and_where(Expr::col(Col::Name).is_not_in(excluded));
        }
        q
    });
    assemble_definition_rows(resolved, with_clause, used_q)
}

fn single_concrete_tag_type(def: &ResolvedDefinition) -> Option<&str> {
    if def.kind != ItemKind::Tag {
        return None;
    }
    let (ty, _) = def.pattern.split_once(':')?;
    if ty.is_empty() || crate::util::is_glob_pattern(ty) {
        return None;
    }
    Some(ty)
}

pub(crate) fn try_build_physical_order_scan(
    _src: &Src,
    resolved: &ResolvedDefinition,
    storage: &DefinitionStorage,
    _orders: &[ResolvedOrder],
    n: usize,
    offset: usize,
) -> Option<SelectStatement> {
    use crate::db::Tbl;
    use crate::util;

    let tag_type = single_concrete_tag_type(resolved)?;
    if tag_type.contains('/')
        || tag_type.contains('\\')
        || tag_type.contains("..")
    {
        return None;
    }
    let [part] =
        <[PathBuf; 1]>::try_from(storage.type_origin_dirs(tag_type)).ok()?;
    let mut q = Query::select();
    q.column(Col::ItemId)
        .column(Col::Rank)
        .expr_as(
            Expr::case(
                Expr::col(Col::ItemId).is_not_null(),
                Expr::val(ItemKind::Tag.as_str()),
            )
            .finally(Expr::val(ItemKind::Volatile.as_str())),
            Col::ItemKind,
        )
        .expr_as(CustomFunc::empty_tag_list(), QueryResultCol::Tags)
        .expr_as(
            CustomFunc::as_representative(Expr::col(Col::TypedTag)),
            Representative,
        )
        .expr_as(Expr::col(Col::TypedTag), Col::Name)
        .from_subquery(
            util::parquet_query(&part.join("*.parquet").to_string_lossy()),
            Tbl::Tags,
        )
        .and_where(name_condition_on(
            Col::TypedTag,
            &resolved.pattern,
            resolved.exact,
        ));
    if n > 0 {
        q.limit((n + 1) as u64);
    }
    if offset > 0 {
        q.offset(offset as u64);
    }
    Some(q)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    use sea_query::PostgresQueryBuilder;
    use tempfile::tempdir;

    fn type_glob_resolved() -> ResolvedDefinition {
        ResolvedDefinition::for_tag(
            ItemKind::Type,
            TypedTag::new(crate::types::TagType::from("type"), "*"),
            Vec::new(),
            Vec::new(),
            crate::rank::SystemRank::DEFAULT,
        )
    }

    fn storage_with_item_references() -> (tempfile::TempDir, DefinitionStorage)
    {
        let dir = tempdir().unwrap();
        let path = dir.path().join("item_references.parquet");
        std::fs::File::create(&path).unwrap();
        let storage = DefinitionStorage {
            item_references: Some(path),
            ..DefinitionStorage::default()
        };
        (dir, storage)
    }

    #[test]
    fn definition_rows_offset_unregistered_rank_and_default_stored_rank() {
        let (_dir, storage) = storage_with_item_references();
        let sql = build_definition_rows(&type_glob_resolved(), &storage)
            .to_string(PostgresQueryBuilder);
        assert!(sql.contains(r#""rank" - 20"#), "{sql}");
        assert!(sql.contains(r#"COALESCE("rank", 0)"#), "{sql}");
    }

    #[test]
    fn registered_pick_reads_only_stored_rows() {
        let (_dir, storage) = storage_with_item_references();
        let sql = build_registered_definition_pick_sql(
            &type_glob_resolved(),
            &storage,
        )
        .to_string(PostgresQueryBuilder);
        assert!(!sql.contains(r#""volatile""#), "{sql}");
    }

    #[test]
    fn test_name_condition_caret_is_literal_not_prefix_glob() {
        let expr = name_condition("^foo", false);
        let sql = sea_query::Query::select()
            .expr(expr)
            .to_string(sea_query::PostgresQueryBuilder);
        assert!(
            sql.contains("'^foo'"),
            "value should stay literal '^foo': {}",
            sql
        );
        assert!(
            !sql.contains("'foo*'"),
            "must not convert to prefix glob 'foo*': {}",
            sql
        );
    }

    fn touch_origin_part(
        tags_dir: &std::path::Path,
        origin: &str,
        tag_type: &str,
    ) {
        let part = tags_dir.join(format!("origin={origin}/type={tag_type}"));
        std::fs::create_dir_all(&part).unwrap();
        std::fs::File::create(part.join("data.parquet")).unwrap();
    }

    fn indexed_storage(tags_dir: std::path::PathBuf) -> DefinitionStorage {
        DefinitionStorage {
            item_references: None,
            tags_dir: Some(tags_dir),
            indexed: true,
            attach_item_tags: true,
        }
    }

    #[test]
    fn test_build_definition_rows_user_tags_only_and_type_tags_dict() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        touch_origin_part(&store.tags_dir(), "file", "extension");
        let storage = indexed_storage(store.tags_dir());
        let tag_resolved = ResolvedDefinition::for_tag(
            ItemKind::Tag,
            TypedTag::new(crate::types::TagType::from("tag"), "*"),
            Vec::new(),
            Vec::new(),
            crate::rank::SystemRank::DEFAULT,
        );
        let tag_sql = build_definition_rows(&tag_resolved, &storage)
            .to_string(PostgresQueryBuilder);
        assert!(
            tag_sql.contains("hive_partitioning") || tag_sql.contains("tags"),
            "{tag_sql}"
        );
        assert!(!tag_sql.contains("user_tags"));

        let type_resolved = ResolvedDefinition::for_tag(
            ItemKind::Type,
            TypedTag::new(crate::types::TagType::from("type"), "*"),
            Vec::new(),
            Vec::new(),
            crate::rank::SystemRank::DEFAULT,
        );
        let type_sql = build_definition_rows(&type_resolved, &storage)
            .to_string(PostgresQueryBuilder);
        assert!(type_sql.contains("tags"));
    }

    #[test]
    fn test_build_definition_rows_optimized_type_projection_does_not_scan_rank_or_item_id(
    ) {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        touch_origin_part(&store.tags_dir(), "file", "extension");
        let storage = indexed_storage(store.tags_dir());
        let type_resolved = ResolvedDefinition::for_tag(
            ItemKind::Type,
            TypedTag::new(crate::types::TagType::from("type"), "*"),
            Vec::new(),
            Vec::new(),
            crate::rank::SystemRank::DEFAULT,
        );
        let sql = build_definition_rows(&type_resolved, &storage)
            .to_string(PostgresQueryBuilder);
        assert!(!sql.contains("\"tags\".\"rank\""));
        assert!(!sql.contains("\"tags\".\"item_id\""));
    }

    #[test]
    fn test_build_definition_rows_tag_uses_tags_dictionary() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        touch_origin_part(&store.tags_dir(), "file", "extension");
        let storage = indexed_storage(store.tags_dir());
        let tag_resolved = ResolvedDefinition::for_tag(
            ItemKind::Tag,
            TypedTag::new(crate::types::TagType::from("tag"), "extension:r*"),
            Vec::new(),
            Vec::new(),
            crate::rank::SystemRank::DEFAULT,
        );
        let sql = build_definition_rows(&tag_resolved, &storage)
            .to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("hive_partitioning") || sql.contains("type=extension"),
            "{sql}"
        );
        assert!(!sql.contains("user_tags"));
        assert!(!sql.contains("oneview"));
    }

    #[test]
    fn test_build_definition_fetch_sql_short_skips_oneview_join() {
        let resolved = ResolvedDefinition::for_tag(
            ItemKind::Tag,
            TypedTag::new(crate::types::TagType::from("tag"), "extension:r*"),
            Vec::new(),
            Vec::new(),
            crate::rank::SystemRank::DEFAULT,
        );
        let sql = build_definition_fetch_sql(
            &Src::OneView,
            &resolved,
            &DefinitionStorage::default(),
            true,
        )
        .to_string(PostgresQueryBuilder);
        assert!(!sql.contains("LEFT JOIN"), "{sql}");
        assert!(sql.contains("list_value()"), "{sql}");
    }

    #[test]
    fn test_physical_order_scan_has_limit_without_order_by() {
        use crate::query::lens_resolver::Resolver;
        use crate::tag::TagRegistry;

        let temp = tempfile::tempdir().unwrap();
        let tags_dir = temp.path().join("tags");
        touch_origin_part(&tags_dir, "file", "extension");
        let registry = TagRegistry::with_standard();
        let resolver = Resolver::new_nowarn("tag:\"extension:r*\"", &registry)
            .unwrap()
            .with_short(true)
            .with_store_tags_dir(Some(tags_dir));
        let sql =
            crate::query::sql::build_fetch_sql(&Src::OneView, &resolver, 10, 0)
                .unwrap()
                .to_string(PostgresQueryBuilder);
        assert!(sql.contains("type=extension"), "{sql}");
        assert!(sql.contains("LIMIT"), "{sql}");
        assert!(!sql.to_uppercase().contains("ORDER BY"), "{sql}");
        assert!(!sql.contains("oneview"), "{sql}");
        assert!(!sql.contains("LEFT JOIN"), "{sql}");
    }

    #[test]
    fn test_count_scans_dictionary_and_mixed_pick_reads_stored_rows() {
        use crate::query::lens_resolver::Resolver;
        use crate::tag::TagRegistry;

        let temp = tempfile::tempdir().unwrap();
        let tags_dir = temp.path().join("tags");
        touch_origin_part(&tags_dir, "user", "project");
        let registry = TagRegistry::with_standard();
        let count_resolver =
            Resolver::new_nowarn("count(tag:\"project:a*\")", &registry)
                .unwrap()
                .with_store_tags_dir(Some(tags_dir.clone()));
        let count_sql = crate::query::sql::build_fetch_sql(
            &Src::OneView,
            &count_resolver,
            0,
            0,
        )
        .unwrap()
        .to_string(PostgresQueryBuilder);
        assert!(
            count_sql.contains("hive_partitioning")
                || count_sql.contains("type=project"),
            "{count_sql}"
        );
        assert!(!count_sql.contains("user_tags"), "{count_sql}");
        let pick_resolver =
            Resolver::new_nowarn("tag:\"project:a*\" & filename:*", &registry)
                .unwrap()
                .with_store_tags_dir(Some(tags_dir));
        let pick_sql = crate::query::sql::build_fetch_sql(
            &Src::OneView,
            &pick_resolver,
            10,
            0,
        )
        .unwrap()
        .to_string(PostgresQueryBuilder);
        assert!(
            pick_sql.contains("\"stored\"") && pick_sql.contains("project:a*"),
            "{pick_sql}"
        );
        assert!(pick_sql.contains("oneview"), "{pick_sql}");
        assert!(!pick_sql.contains("user_tags"), "{pick_sql}");
        assert!(
            !pick_sql.contains("hive_partitioning")
                && !pick_sql.contains("type=project"),
            "{pick_sql}"
        );
    }

    #[test]
    fn test_parquet_src_definition_rows_use_tags_dictionary() {
        let temp = tempfile::tempdir().unwrap();
        let tags_dir = temp.path().join("tags");
        touch_origin_part(&tags_dir, "user", "project");
        let resolved = ResolvedDefinition::for_tag(
            ItemKind::Tag,
            TypedTag::new(crate::types::TagType::from("tag"), "project:a*"),
            Vec::new(),
            Vec::new(),
            crate::rank::SystemRank::DEFAULT,
        );
        let storage = indexed_storage(tags_dir);
        let sql = build_definition_rows(&resolved, &storage)
            .to_string(PostgresQueryBuilder);
        assert!(
            sql.contains("hive_partitioning") || sql.contains("type=project"),
            "{sql}"
        );
        assert!(!sql.contains("user_tags"), "{sql}");
    }

    fn storage_with(
        parts: &[&str],
        indexed: bool,
    ) -> (tempfile::TempDir, DefinitionStorage) {
        let dir = tempfile::tempdir().unwrap();
        let tags_dir = dir.path().join("tags");
        for part in parts {
            let part_dir = tags_dir.join(part);
            std::fs::create_dir_all(&part_dir).unwrap();
            std::fs::File::create(part_dir.join("data.parquet")).unwrap();
        }
        let storage = DefinitionStorage {
            item_references: None,
            tags_dir: Some(tags_dir),
            indexed,
            attach_item_tags: true,
        };
        (dir, storage)
    }

    #[test]
    fn test_definition_rows_read_dictionary_with_valid_origins() {
        let resolved = ResolvedDefinition::for_tag(
            ItemKind::Tag,
            TypedTag::new(crate::types::TagType::from("tag"), "*"),
            Vec::new(),
            Vec::new(),
            crate::rank::SystemRank::DEFAULT,
        );
        let (_d, unindexed) = storage_with(
            &["origin=user/type=project", "origin=file/type=extension"],
            false,
        );
        let sql = build_definition_rows(&resolved, &unindexed)
            .to_string(PostgresQueryBuilder);
        assert!(sql.contains("DISTINCT"), "{sql}");
        assert!(sql.contains("'user'"), "{sql}");
        assert!(!sql.contains("'file'"), "{sql}");
        assert!(!sql.contains("oneview"), "{sql}");

        let (_d, indexed) = storage_with(
            &["origin=user/type=project", "origin=file/type=extension"],
            true,
        );
        let sql = build_definition_rows(&resolved, &indexed)
            .to_string(PostgresQueryBuilder);
        assert!(sql.contains("'file'"), "{sql}");
    }

    #[test]
    fn test_origin_definition_rows_filter_dictionary_by_origin() {
        let (_d, indexed) = storage_with(
            &["origin=user/type=project", "origin=file/type=extension"],
            true,
        );
        let file = ResolvedDefinition::for_origin(
            Origin::File,
            vec![],
            vec![],
            crate::rank::SystemRank::DEFAULT,
        );
        let sql = build_definition_rows(&file, &indexed)
            .to_string(PostgresQueryBuilder);
        assert!(sql.contains("'file'"), "{sql}");
        assert!(!sql.contains("'user'"), "{sql}");
        assert!(!sql.contains("oneview"), "{sql}");

        let (_d, unindexed) = storage_with(
            &["origin=user/type=project", "origin=file/type=extension"],
            false,
        );
        let sql = build_definition_rows(&file, &unindexed)
            .to_string(PostgresQueryBuilder);
        assert!(!sql.contains("hive_partitioning"), "{sql}");
    }

    #[test]
    fn test_physical_order_scan_requires_single_origin() {
        let resolved = ResolvedDefinition::for_tag(
            ItemKind::Tag,
            TypedTag::new(crate::types::TagType::from("tag"), "name:foo*"),
            Vec::new(),
            Vec::new(),
            crate::rank::SystemRank::DEFAULT,
        );
        let (_d, both) = storage_with(
            &["origin=user/type=name", "origin=file/type=name"],
            true,
        );
        assert!(try_build_physical_order_scan(
            &Src::OneView,
            &resolved,
            &both,
            &[],
            10,
            0
        )
        .is_none());
        let (_d, single) = storage_with(&["origin=file/type=name"], true);
        assert!(try_build_physical_order_scan(
            &Src::OneView,
            &resolved,
            &single,
            &[],
            10,
            0
        )
        .is_some());
    }
}
