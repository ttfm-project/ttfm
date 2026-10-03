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

use anyhow::Result;
use std::time::Instant;
use tempfile::tempdir;
use ttfm::db::Store;
use ttfm::indexing::Indexer;
use ttfm::query::error::Warning;
use ttfm::search::{search_short, SearchOptions};
use ttfm::tag::TagRegistry;

fn setup_many_files() -> Result<(Store, TagRegistry, tempfile::TempDir)> {
    let dir = tempdir()?;
    let root = dir.path().join("files");
    std::fs::create_dir_all(&root)?;
    for i in 0..400 {
        std::fs::write(root.join(format!("f{i:03}.rs")), "x")?;
        std::fs::write(root.join(format!("g{i:03}.txt")), "x")?;
    }
    let registry = TagRegistry::with_standard();
    let store = Store::open(dir.path().join("db"))?;
    Indexer::new(&store, &registry).initialize_tables()?;
    Indexer::new(&store, &registry).run(&[&root], None, false)?;
    Ok((store, registry, dir))
}

fn short_options() -> SearchOptions {
    SearchOptions {
        n: Some(10),
        short: true,
        cache: false,
        ..Default::default()
    }
}

#[test]
fn test_baseline_prefix_projection_and_definition_tag_query_timing() {
    let (store, registry, _dir) = setup_many_files().unwrap();
    let mut sink: Vec<Warning> = Vec::new();
    let t0 = Instant::now();
    let proj = search_short(
        &store,
        &registry,
        "extension:r* & extension:",
        short_options(),
        &mut sink,
    )
    .unwrap();
    let proj_ms = t0.elapsed().as_millis();
    sink.clear();
    let t1 = Instant::now();
    let defn = search_short(
        &store,
        &registry,
        "tag:\"extension:r*\"",
        short_options(),
        &mut sink,
    )
    .unwrap();
    let defn_ms = t1.elapsed().as_millis();
    eprintln!(
        "baseline prefix_projection={proj_ms}ms n={}",
        proj.results.len(),
    );
    eprintln!(
        "baseline definition_tag={defn_ms}ms n={}",
        defn.results.len(),
    );
    assert!(!proj.results.is_empty());
    assert!(!defn.results.is_empty());
}
