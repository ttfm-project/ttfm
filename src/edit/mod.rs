pub(crate) mod cast;
pub mod confirm;
pub mod error;
pub(crate) mod fs_operate;
pub(crate) mod glob_capture;
mod lens_schema;
pub mod modify;
pub mod parse;
mod search_and_apply_captures;
pub(crate) mod sql;
mod tag_filter;
pub mod write;

use crate::db::Store;
use crate::query::error::WarningSink;
use crate::response::Item;
use crate::tag::TagRegistry;
use crate::types::{
    Bitical, BiticalType, ItemId, ItemKind, Origin, SType, TagType, TypedTag,
};
use anyhow::{bail, Result};

pub use crate::config::{
    ConfirmMode, ConflictPolicy, HardlinkPolicy, SkipScope,
};
pub use crate::tag::{Edit, EditStrategy};
pub use fs_operate::FsMove;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    Tag,
    Untag,
}

#[derive(Clone, Debug)]
pub struct WriteOptions {
    pub confirm: ConfirmMode,
    pub on_conflict: Option<ConflictPolicy>,
    pub on_hardlink: Option<HardlinkPolicy>,
    pub skip_scope: SkipScope,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self::noconfirm()
    }
}

impl WriteOptions {
    pub fn noconfirm() -> Self {
        Self {
            confirm: ConfirmMode::Never,
            on_conflict: Some(ConflictPolicy::Abort),
            on_hardlink: Some(HardlinkPolicy::Abort),
            skip_scope: SkipScope::Item,
        }
    }

    pub fn interactive() -> Self {
        Self {
            confirm: ConfirmMode::Auto,
            on_conflict: None,
            on_hardlink: None,
            skip_scope: SkipScope::Item,
        }
    }

    pub fn on_confirm(mut self, mode: ConfirmMode) -> Self {
        self.confirm = mode;
        self
    }

    pub fn on_conflict(mut self, policy: ConflictPolicy) -> Self {
        self.on_conflict = Some(policy);
        self
    }

    pub fn on_hardlink(mut self, policy: HardlinkPolicy) -> Self {
        self.on_hardlink = Some(policy);
        self
    }

    pub fn skip_scope(mut self, scope: SkipScope) -> Self {
        self.skip_scope = scope;
        self
    }

    pub fn is_interactive(&self) -> bool {
        match self.confirm {
            ConfirmMode::Never => false,
            ConfirmMode::Always => true,
            ConfirmMode::Auto => {
                self.on_conflict.is_none() && self.on_hardlink.is_none()
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct EditResponse {
    pub updated: usize,
    pub deleted: usize,
    pub fs_ops: usize,
    pub new_items: usize,
    pub has_skipped: bool,
}

fn static_tag_types(parsed: Option<&parse::EditQuery>) -> Vec<TagType> {
    parsed
        .map(|q| q.nodes.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter(|n| !n.tag_type.has_braced())
        .map(|n| TagType::from(n.tag_type.value().as_str()))
        .collect()
}

pub fn create_note(
    store: &Store,
    registry: &TagRegistry,
    content: &str,
) -> Result<i64> {
    let resp = write::write_and_refresh(
        store,
        registry,
        vec![write::WriteAction::Add {
            item: ItemId::Volatile(0),
            tags: vec![
                write::TagOp::Append(TypedTag::new(
                    SType::ItemKind,
                    ItemKind::Note.as_str(),
                )),
                write::TagOp::Append(TypedTag::new(SType::Content, content)),
            ],
        }],
        None,
    )?;
    Ok(resp.new_item_ids[0])
}

pub fn edit(
    store: &Store,
    registry: &TagRegistry,
    search_query: &str,
    edit_query: Option<&str>,
    query_type: QueryType,
    tag_condition: Option<&str>,
    options: WriteOptions,
    sink: &mut dyn WarningSink,
) -> Result<EditResponse> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = std::io::stderr();
    edit_with_io(
        store,
        registry,
        search_query,
        edit_query,
        query_type,
        tag_condition,
        options,
        sink,
        &mut input,
        &mut output,
    )
}

pub fn edit_with_io<R: std::io::BufRead, W: std::io::Write>(
    store: &Store,
    registry: &TagRegistry,
    search_query: &str,
    edit_query: Option<&str>,
    query_type: QueryType,
    tag_condition: Option<&str>,
    options: WriteOptions,
    sink: &mut dyn WarningSink,
    input: &mut R,
    output: &mut W,
) -> Result<EditResponse> {
    let mut prompt = confirm::IoConfirmPrompt { input, output };
    edit_with_prompt(
        store,
        registry,
        search_query,
        edit_query,
        query_type,
        tag_condition,
        options,
        sink,
        &mut prompt,
    )
}

fn retain_untaggable(
    store: &Store,
    item_edits: Vec<(Item, Option<parse::EditQuery>)>,
) -> Result<Vec<(Item, Option<parse::EditQuery>)>> {
    let is_builtin =
        |item: &Item| Origin::within(item.id.as_i64()) == Origin::Builtin;
    let builtin_ids: Vec<i64> = item_edits
        .iter()
        .filter(|(item, _)| item.id.is_stored() && is_builtin(item))
        .map(|(item, _)| item.id.as_i64())
        .collect();
    let materialized = if builtin_ids.is_empty() {
        Default::default()
    } else {
        write::existing_item_ids(store, &builtin_ids)?
    };
    Ok(item_edits
        .into_iter()
        .filter(|(item, _)| {
            item.id.is_stored()
                && (!is_builtin(item)
                    || materialized.contains(&item.id.as_i64()))
        })
        .collect())
}

pub fn edit_with_prompt(
    store: &Store,
    registry: &TagRegistry,
    search_query: &str,
    edit_query: Option<&str>,
    query_type: QueryType,
    tag_condition: Option<&str>,
    options: WriteOptions,
    sink: &mut dyn WarningSink,
    prompt: &mut dyn confirm::ConfirmPrompt,
) -> Result<EditResponse> {
    registry.load_type_configs(store)?;
    if query_type == QueryType::Untag
        && !crate::search::returns_items(store, registry, search_query)?
    {
        bail!(
            "untag cannot use a Projection, Nest, or aggregation as the SearchQuery: '{search_query}'"
        );
    }
    let parsed = edit_query
        .map(|q| parse::parse_edit_query(q, query_type, registry))
        .transpose()?;
    fs_operate::check_location_tag(
        &static_tag_types(parsed.as_ref()),
        registry,
    )?;
    let item_edits = search_and_apply_captures::search_and_apply_captures(
        store,
        registry,
        search_query,
        parsed.as_ref(),
        sink,
    )?;
    let item_edits = if query_type == QueryType::Untag {
        retain_untaggable(store, item_edits)?
    } else {
        item_edits
    };
    let (mut fs_plan, actions, cast_summary) = plan(
        store,
        registry,
        item_edits.clone(),
        query_type,
        tag_condition,
    )?;
    let has_skipped_unsupported = fs_plan.warn_unsupported(sink);
    let Some((fs_plan, actions)) = confirm::confirm_with_prompt(
        fs_plan,
        actions,
        &item_edits,
        &options,
        cast_summary.clone(),
        prompt,
    )?
    else {
        return Ok(EditResponse {
            updated: 0,
            deleted: 0,
            fs_ops: 0,
            new_items: 0,
            has_skipped: false,
        });
    };

    let original_item_count = item_edits.len();
    let mut processed_items = std::collections::HashSet::new();
    for a in &actions {
        match a {
            write::WriteAction::Add { item, .. }
            | write::WriteAction::Delete { item, .. } => {
                processed_items.insert(item.clone());
            }
        }
    }

    let outcome = fs_operate::apply(store, registry, fs_plan)?;
    for m in &outcome.moved {
        processed_items.insert(m.item.clone());
    }
    for a in &outcome.attrs_set {
        processed_items.insert(a.item.clone());
    }
    let resp = write::write_and_refresh(
        store,
        registry,
        actions,
        cast_summary
            .filter(|c| c.count > 0)
            .map(|c| (c.tag_type, c.to_type)),
    )?;

    if outcome.count() > 0 {
        crate::search::clear_cache(&store.db_dir);
    }

    let has_skipped = has_skipped_unsupported
        || (processed_items.len() < original_item_count);

    Ok(EditResponse {
        updated: resp.updated,
        deleted: resp.deleted,
        fs_ops: outcome.count(),
        new_items: resp.new_item_ids.len(),
        has_skipped,
    })
}

fn detect_type_casts(
    item_edits: &[(crate::response::Item, Option<parse::EditQuery>)],
    registry: &TagRegistry,
) -> Result<
    Vec<(
        TagType,
        crate::types::BiticalType,
        crate::types::BiticalType,
    )>,
> {
    use std::str::FromStr;
    let mut casts = Vec::new();
    for (item, tag_query) in item_edits {
        if modify::is_type_item(item) {
            let type_name = item.raw_repr();
            if let Some(q) = tag_query {
                for node in &q.nodes {
                    if node.tag_type.value() == "bitical_type" {
                        if let Some(leaf) = &node.label {
                            if let Ok(new_type) =
                                crate::types::BiticalType::from_str(
                                    &leaf.value(),
                                )
                            {
                                if new_type == crate::types::BiticalType::Uuid {
                                    bail!("type cast to 'uuid' is not allowed");
                                }
                                let tag_type =
                                    TagType::from(type_name.as_str());
                                let current_bitical_type = registry
                                    .type_config(&tag_type)
                                    .and_then(|c| c.bitical_type)
                                    .unwrap_or(
                                        crate::types::BiticalType::String,
                                    );
                                if new_type != current_bitical_type {
                                    casts.push((
                                        tag_type.clone(),
                                        current_bitical_type,
                                        new_type,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(casts)
}

fn reject_mixed_type_inference(
    item_edits: &[(Item, Option<parse::EditQuery>)],
    registry: &TagRegistry,
) -> Result<()> {
    use crate::query::format::OperandFormat;
    use std::collections::HashMap;

    let mut batch_types: HashMap<TagType, BiticalType> = HashMap::new();
    for (_item, q) in item_edits {
        let Some(q) = q else { continue };
        for node in &q.nodes {
            let tt = TagType::from(node.tag_type.value().as_str());
            if registry.get(tt.as_str()).is_some() {
                continue;
            }
            if registry
                .type_config(&tt)
                .and_then(|c| c.bitical_type)
                .is_some()
            {
                continue;
            }
            let Some(leaf) = &node.label else { continue };
            let inferred = if leaf.quoted {
                BiticalType::String
            } else {
                match Bitical::parse(&leaf.value()) {
                    Some(Ok(v)) => v.name(),
                    _ => BiticalType::String,
                }
            };
            if let Some(&first) = batch_types.get(&tt) {
                if first != inferred {
                    return Err(crate::tag::type_cannot_assign(
                        &tt,
                        first,
                        &leaf.value(),
                    ));
                }
            } else {
                batch_types.insert(tt, inferred);
            }
        }
    }
    Ok(())
}

fn plan(
    store: &Store,
    registry: &TagRegistry,
    item_edits: Vec<(crate::response::Item, Option<parse::EditQuery>)>,
    query_type: QueryType,
    tag_condition: Option<&str>,
) -> Result<(
    fs_operate::FsPlan,
    Vec<write::WriteAction>,
    Option<confirm::CastSummary>,
)> {
    if matches!(query_type, QueryType::Tag) {
        reject_mixed_type_inference(&item_edits, registry)?;
    }
    let cast_targets = detect_type_casts(&item_edits, registry)?;
    if cast_targets.len() > 1 {
        bail!("multiple type cast is not supported in a single command");
    }
    let mut cast_summary = None;
    if let Some((target_type, old_type, new_type)) =
        cast_targets.into_iter().next()
    {
        if registry.get(target_type.as_str()).is_some() {
            bail!(
                "cannot modify bitical_type of built-in/plugin type '{}'",
                target_type
            );
        }
        let count = cast::pre_validate_cast(store, &target_type, new_type)?;
        cast_summary = Some(confirm::CastSummary {
            tag_type: target_type,
            from_type: old_type,
            to_type: new_type,
            count,
        });
    }

    let condition = tag_condition
        .map(tag_filter::parse_tag_condition)
        .transpose()?;

    let mut fs_inputs = Vec::new();
    let mut actions = Vec::new();
    for (item, tag_query) in item_edits {
        let all_nodes =
            modify::resolve_nodes(tag_query.as_ref(), query_type, registry)?;
        let mut fs_nodes = Vec::new();
        let mut tag_nodes = Vec::new();
        for n in all_nodes {
            if fs_operate::is_fs_strategy(n.strategy) {
                fs_nodes.push(n);
            } else {
                tag_nodes.push(n);
            }
        }
        if !fs_nodes.is_empty() {
            fs_inputs.push((item.clone(), fs_nodes));
        }
        if let Some(ref node) = condition {
            if tag_filter::eval_tag_predicate(node, None)? {
                actions.extend(modify::modify(
                    &item, &tag_nodes, query_type, registry,
                )?);
            }
        } else {
            actions.extend(modify::modify(
                &item, &tag_nodes, query_type, registry,
            )?);
        }
    }
    let fs_plan = fs_operate::plan_fs(registry, fs_inputs, query_type)?;
    Ok((fs_plan, actions, cast_summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::TagRegistry;

    #[test]
    fn forbidden_tags_return_none() {
        let reg = TagRegistry::with_standard();
        assert!(reg.get("size").and_then(|f| f.edit()).is_none());
        assert!(reg.get("hash").and_then(|f| f.edit()).is_none());
        assert!(reg.get("is_dir").and_then(|f| f.edit()).is_none());
        assert!(reg.get("file_id").and_then(|f| f.edit()).is_none());
    }

    #[test]
    fn editable_tags_have_correct_strategies() {
        let reg = TagRegistry::with_standard();
        let strategy = |name: &str| {
            reg.get(name).and_then(|f| f.edit()).map(|e| e.strategy())
        };
        assert!(matches!(strategy("rank"), Some(EditStrategy::Replace)));
        assert!(matches!(strategy("name"), Some(EditStrategy::Replace)));
        assert!(matches!(strategy("filename"), Some(EditStrategy::Relocate)));
        assert!(matches!(strategy("path"), Some(EditStrategy::Relocate)));
        assert!(matches!(
            strategy("extension"),
            Some(EditStrategy::Relocate)
        ));
        assert!(matches!(
            strategy("parentdir"),
            Some(EditStrategy::Relocate)
        ));
        assert!(matches!(strategy("mtime"), Some(EditStrategy::SetFileAttr)));
        assert!(matches!(
            strategy("item_kind"),
            Some(EditStrategy::ModifyInjection)
        ));
        assert!(matches!(
            strategy("content"),
            Some(EditStrategy::ModifyInjection)
        ));
    }

    #[test]
    fn test_write_options_is_interactive() {
        let noconfirm = WriteOptions::noconfirm();
        assert!(!noconfirm.is_interactive());

        let always = WriteOptions::noconfirm().on_confirm(ConfirmMode::Always);
        assert!(always.is_interactive());

        let never = WriteOptions::interactive().on_confirm(ConfirmMode::Never);
        assert!(!never.is_interactive());

        let policy_bypass =
            WriteOptions::interactive().on_conflict(ConflictPolicy::Serial);
        assert!(!policy_bypass.is_interactive());

        let hardlink_bypass =
            WriteOptions::interactive().on_hardlink(HardlinkPolicy::All);
        assert!(!hardlink_bypass.is_interactive());
    }
}
