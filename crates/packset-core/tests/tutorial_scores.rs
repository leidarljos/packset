//! The getting-started search, as the linear engine prints it.
//!
//! A fresh pack has no encoder, so search is the prefix scan fused with
//! BM25+. The pages quote that run. This checks the quoted scores, kinds
//! and texts against the engine, at the instant the two claims were written.

use packset_core::bm25::Index;
use packset_core::search::{
    atom_tokens, merge_ballots_ranked, search_bm25_ranked, search_linear_with_ranked, Ask, Rank,
    Record,
};
use packset_core::Panel;
use serde_json::{json, Value};

const NOW: &str = "2026-06-01T00:00:00.000Z";

const LESSON: &str = "The lexical default is BM25+. It scored 0.635 hit@1 against 0.615 for BM25.";
const PREFERENCE: &str = "CombMNZ over RRF for fusing two ballots.";

const PAGES: &[&str] = &[
    "docs/source/getting-started.rst",
    "docs/orgmode/getting-started.org",
    "docs/source/index.rst",
    "docs/orgmode/index.org",
];

fn atom(id: &str, kind: &str, text: &str) -> Record {
    json!({
        "id": id,
        "kind": kind,
        "text": text,
        "ts": NOW,
        "trust": 1.0,
    })
    .as_object()
    .expect("object")
    .clone()
}

/// Score, kind, text, in the order a search prints them.
fn fresh_run() -> Vec<(String, String, String)> {
    let atoms = [
        atom("lesson", "lesson", LESSON),
        atom("preference", "preference", PREFERENCE),
    ];
    let documents: Vec<Vec<String>> = atoms.iter().map(atom_tokens).collect();
    let index = Index::build(documents.iter().map(Vec::as_slice));
    let ask = Ask {
        user: "",
        memory: "",
        atoms: &atoms,
        query: "ballots BM25",
        limit: 10,
        set: None,
        kind: None,
        now: NOW,
    };
    let lexical = search_linear_with_ranked(&ask, &documents, Rank::Fused);
    let terms = search_bm25_ranked(&ask, &index, Rank::Fused);
    let hits = merge_ballots_ranked(&[lexical, terms], 10, &Panel::default(), NOW, Rank::Fused);
    hits.iter().map(shown).collect()
}

fn shown(hit: &Value) -> (String, String, String) {
    (
        format!("{:.4}", hit["score"].as_f64().unwrap_or(0.0)),
        hit["kind"].as_str().unwrap_or("").to_string(),
        hit["text"].as_str().unwrap_or("").to_string(),
    )
}

fn is_score(field: &str) -> bool {
    let Some((whole, frac)) = field.split_once('.') else {
        return false;
    };
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && frac.len() == 4
        && frac.bytes().all(|b| b.is_ascii_digit())
}

/// One documented hit line: score, kind, an id, then the claim.
fn documented_hit(line: &str) -> Option<(String, String, String)> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 4 || !is_score(fields[0]) {
        return None;
    }
    let text = fields[3..].join(" ");
    if text != LESSON && text != PREFERENCE {
        return None;
    }
    Some((fields[0].to_string(), fields[1].to_string(), text))
}

fn page(path: &str) -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|err| panic!("read {path}: {err}"))
}

fn place(kind: &str) -> &str {
    match kind {
        "preference" => "standing choice",
        "lesson" => "lesson",
        other => other,
    }
}

#[test]
fn the_pages_quote_the_fresh_search() {
    let fresh = fresh_run();
    assert_eq!(fresh.len(), 2, "{fresh:?}");
    assert_eq!(fresh[0].2, LESSON, "the lesson leads: {fresh:?}");
    assert_eq!(fresh[1].2, PREFERENCE, "{fresh:?}");

    let lesson = fresh.iter().find(|hit| hit.2 == LESSON).unwrap();
    let preference = fresh.iter().find(|hit| hit.2 == PREFERENCE).unwrap();
    let scores = format!(
        "the lesson scores {} and the preference scores {}",
        lesson.0, preference.0
    );
    let order = format!(
        "lists the {} first, then the {}.",
        place(&fresh[0].1),
        place(&fresh[1].1)
    );

    for path in PAGES {
        let text = page(path);
        let quoted: Vec<_> = text.lines().filter_map(documented_hit).collect();
        assert_eq!(&quoted, &fresh, "{path} does not quote the fresh search");
        if path.contains("getting-started") {
            assert!(text.contains(&scores), "{path} does not say {scores}");
        }
        if path.contains("index.") {
            assert!(text.contains(&order), "{path} does not say {order}");
        }
    }
}
