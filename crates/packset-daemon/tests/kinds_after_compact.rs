//! A pack listed by kind answers from its records after LMDB compacts the
//! file and after a writer other than this build commits to it.
//!
//! A compacted copy starts the transaction id again at one, and an outside
//! writer's commits move it on from there, so the id can come back to the
//! value this build last saw over different records. A kind index keyed on
//! that id would serve the old listing. The writer keeps no such index:
//! each open reads the kinds from the records.

use std::collections::BTreeMap;
use std::path::Path;

use heed::types::Bytes;
use heed::{CompactionOption, Database, Env, EnvOpenOptions};
use packset_daemon::{store, Home, Service};
use serde_json::{json, Map, Value};

const WORKSPACE: &str = "w";

/// The database at `home` as the writer opens it. heed keeps an
/// environment open for the life of the process and hands it to a later
/// open only when the options match.
fn env(home: &Path) -> Env {
    let dir = home.join("memory.lmdb");
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: no writer has this home open while the test writes to it.
    unsafe {
        EnvOpenOptions::new()
            .map_size(store::MAP_SIZE)
            .max_dbs(1)
            .open(dir)
    }
    .unwrap()
}

/// Compact `home`'s database in place, as `mdb_copy -c` and a rename do
/// between two runs of the writer, and give the transaction id before.
/// The environment is closed first so the next open maps the compacted
/// file, not the one this process already has.
fn compact(home: &Path) -> usize {
    let source = env(home);
    let was = source.info().last_txn_id;
    let copy = home.join("compacted.mdb");
    source
        .copy_to_file(&copy, CompactionOption::Enabled)
        .unwrap();
    source.prepare_for_closing().wait();
    std::fs::rename(&copy, home.join("memory.lmdb/data.mdb")).unwrap();
    let now = env(home).info().last_txn_id;
    assert!(
        now < was,
        "the fixture did not move the transaction id back ({was} to {now})"
    );
    was
}

/// Write the claims a seat's `remember` and `prefer` would.
fn written_by_this_build(home: &Path) {
    let svc = Service::open(Home::new(home)).unwrap();
    for (kind, text) in [
        ("lesson", "Reviews open with a failing test."),
        ("lesson", "Bench numbers name the CPU they ran on."),
        ("lesson", "A flaky test is a bug in the test."),
        ("preference", "Prefer ripgrep over grep."),
    ] {
        let atom = json!({
            "schema": "inside.atom/v1",
            "kind": kind,
            "level": "explicit",
            "text": text,
            "workspace": WORKSPACE,
        });
        svc.add(atom.as_object().unwrap().clone()).unwrap();
    }
}

/// What a seat reads by kind: `live_by_kind` from status, and each kind's
/// atoms as `/v1/atoms?kind=` lists them, ordered by id.
fn by_kind(home: &Path) -> (Value, BTreeMap<String, Vec<Map<String, Value>>>) {
    let svc = Service::open(Home::new(home)).unwrap();
    let status = svc
        .status(Some(WORKSPACE), &packset_core::Panel::default())
        .unwrap();
    let mut listed: BTreeMap<String, Vec<Map<String, Value>>> = BTreeMap::new();
    for atom in svc.store().live(WORKSPACE).unwrap().iter() {
        let kind = atom["kind"].as_str().unwrap().to_string();
        listed.entry(kind).or_default().push((**atom).clone());
    }
    for atoms in listed.values_mut() {
        atoms.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    }
    (status["live_by_kind"].clone(), listed)
}

fn ids(atoms: &[Map<String, Value>]) -> Vec<&str> {
    atoms.iter().filter_map(|a| a["id"].as_str()).collect()
}

/// One record put the way a build without this one's code would: the key
/// and the JSON, one transaction each, nothing else touched.
fn put_from_outside(env: &Env, record: &Value) {
    let id = record["id"].as_str().unwrap();
    let mut wtxn = env.write_txn().unwrap();
    let db: Database<Bytes, Bytes> = env.open_database(&wtxn, None).unwrap().unwrap();
    db.put(
        &mut wtxn,
        &store::atom_key(WORKSPACE, id),
        &serde_json::to_vec(record).unwrap(),
    )
    .unwrap();
    wtxn.commit().unwrap();
}

#[test]
fn a_compacted_pack_lists_the_same_atoms_by_kind() {
    let home = tempfile::tempdir().unwrap();
    written_by_this_build(home.path());
    let before = by_kind(home.path());
    assert_eq!(before.0, json!({"lesson": 3, "preference": 1}));

    compact(home.path());
    let after = by_kind(home.path());
    // Whole records, stamps included: the open rewrote nothing.
    assert_eq!(after, before);
}

/// The outside records carry `updated`, so the next open restamps none of
/// them and the transaction id is where this build left it when the
/// listing is read.
#[test]
fn a_pack_written_from_outside_after_compaction_lists_by_kind_on_the_next_open() {
    let home = tempfile::tempdir().unwrap();
    written_by_this_build(home.path());
    let (_, before) = by_kind(home.path());
    let left_at = compact(home.path());

    let outside = env(home.path());
    let mut forgotten = before["lesson"][0].clone();
    forgotten.insert("tombstone".into(), json!(true));
    put_from_outside(&outside, &Value::Object(forgotten));
    let mut rules = Vec::new();
    while outside.info().last_txn_id < left_at {
        let id = format!("outside-rule-{}", rules.len());
        put_from_outside(
            &outside,
            &json!({
                "id": id, "workspace": WORKSPACE, "kind": "rule",
                "text": format!("Outside rule {}.", rules.len()),
                "ts": "2026-11-01T00:00:00.000Z", "updated": "2026-11-01T00:00:00.000Z",
                "tombstone": false
            }),
        );
        rules.push(id);
    }
    assert_eq!(
        outside.info().last_txn_id,
        left_at,
        "the fixture did not bring the transaction id back to where this build left it"
    );
    assert!(!rules.is_empty(), "the fixture wrote no rule from outside");
    rules.sort();

    let (counts, after) = by_kind(home.path());
    assert_eq!(
        counts,
        json!({"lesson": 2, "preference": 1, "rule": rules.len()})
    );
    assert_eq!(ids(&after["lesson"]), ids(&before["lesson"][1..]));
    assert_eq!(after["preference"], before["preference"]);
    assert_eq!(ids(&after["rule"]), rules);
}
