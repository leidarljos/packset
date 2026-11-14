//! A small labelled pack for the linear engine: the prefix scan fused with
//! BM25+, the order packsetd returns when no encoder is installed.
//!
//! A few dozen atoms and ten questions. Each question names one atom.
//! The window is five, which is the cut a client re-ranks inside.

use packset_core::bm25::Index;
use packset_core::search::{
    atom_tokens, merge_ballots_ranked, search_bm25_ranked, search_linear_with_ranked, Ask, Rank,
    Record,
};
use packset_core::Panel;
use serde_json::{json, Value};

const NOW: &str = "2026-06-01T00:00:00.000Z";
const K: usize = 5;

struct Question {
    query: &'static str,
    want: &'static str,
}

fn atom(id: &str, text: &str, ts: &str, trust: f64) -> Record {
    json!({
        "id": id,
        "kind": "lesson",
        "text": text,
        "ts": ts,
        "trust": trust,
    })
    .as_object()
    .expect("object")
    .clone()
}

fn pack() -> Vec<Record> {
    let mut atoms = vec![
        atom(
            "list-scans",
            "Run list_scans before the nightly window.",
            NOW,
            1.0,
        ),
        atom(
            "list-camel",
            "listScans walks the configured roots, records every path it opened, and leaves a note beside each root it could not read.",
            NOW,
            1.0,
        ),
        atom(
            "sha-hyphen",
            "SHA-1 still signs the legacy manifest.",
            NOW,
            1.0,
        ),
        atom("sha-joined", "The digest is sha1 of the payload.", NOW, 1.0),
        atom(
            "http-slash",
            "HTTP/2 is the transport for the seat.",
            NOW,
            1.0,
        ),
        atom(
            "http-joined",
            "Prefer http2 when the peer allows it.",
            NOW,
            1.0,
        ),
        atom(
            "config-json",
            "The thresholds live in config.json.",
            NOW,
            1.0,
        ),
        atom("utf-eight", "Files are stored as UTF-8.", NOW, 1.0),
        atom("file-txt", "Read the note from file.txt.", NOW, 1.0),
        atom("mint-word", "The mint sample sat on the bench.", NOW, 1.0),
    ];
    for n in 0..12 {
        atoms.push(atom(
            &format!("list-trap-{n:02}"),
            &format!("The list of scanxxxx output batch {n:02} is kept for audit."),
            NOW,
            1.0,
        ));
        // A fresh identifier held well above the word. A prefix scores
        // close to an exact token, and this trust closes the remaining
        // point, so the identifiers fill the window.
        atoms.push(atom(
            &format!("mint-trap-{n:02}"),
            &format!("mintxxxx_configuration_module_extra_{n:02} is loaded at boot."),
            NOW,
            15.0,
        ));
    }
    for n in 0..6 {
        atoms.push(atom(
            &format!("sha-trap-{n:02}"),
            &format!("The sha256 digest batch {n:02} is stored beside the blob."),
            NOW,
            1.0,
        ));
    }
    for n in 0..4 {
        atoms.push(atom(
            &format!("plain-{n:02}"),
            &format!("The parser reads a header record {n:02}."),
            NOW,
            1.0,
        ));
    }
    atoms
}

fn questions() -> Vec<Question> {
    vec![
        Question {
            query: "list scans",
            want: "list-scans",
        },
        Question {
            query: "listScans",
            want: "list-camel",
        },
        Question {
            query: "sha1",
            want: "sha-hyphen",
        },
        Question {
            query: "SHA-1",
            want: "sha-joined",
        },
        Question {
            query: "http2",
            want: "http-slash",
        },
        Question {
            query: "HTTP/2",
            want: "http-joined",
        },
        Question {
            query: "config json",
            want: "config-json",
        },
        Question {
            query: "utf8",
            want: "utf-eight",
        },
        Question {
            query: "file txt",
            want: "file-txt",
        },
        Question {
            query: "mint",
            want: "mint-word",
        },
    ]
}

/// The linear engine: prefix scan and BM25+, fused by the default panel.
fn fused(atoms: &[Record], query: &str) -> Vec<Value> {
    let documents: Vec<Vec<String>> = atoms.iter().map(atom_tokens).collect();
    let index = Index::build(documents.iter().map(Vec::as_slice));
    let ask = Ask {
        user: "",
        memory: "",
        atoms,
        query,
        limit: K,
        set: None,
        kind: None,
        now: NOW,
    };
    let lexical = search_linear_with_ranked(&ask, &documents, Rank::Fused);
    let terms = search_bm25_ranked(&ask, &index, Rank::Fused);
    merge_ballots_ranked(&[lexical, terms], K, &Panel::default(), NOW, Rank::Fused)
}

/// 1-based place of `id` in the window, or nothing when the cut dropped it.
fn place(hits: &[Value], id: &str) -> Option<usize> {
    hits.iter()
        .position(|hit| hit["id"].as_str() == Some(id))
        .map(|rank| rank + 1)
}

#[test]
fn labelled_atoms_survive_the_linear_cut() {
    let atoms = pack();
    let asked = questions();
    assert!(
        (24..=48).contains(&atoms.len()),
        "a few dozen atoms, got {}",
        atoms.len()
    );
    assert_eq!(asked.len(), 10);

    let mut top1 = 0usize;
    let mut top5 = 0usize;
    for question in &asked {
        let hits = fused(&atoms, question.query);
        let rank = place(&hits, question.want);
        let ids: Vec<&str> = hits.iter().filter_map(|hit| hit["id"].as_str()).collect();
        if rank == Some(1) {
            top1 += 1;
        }
        if rank.is_some_and(|r| r <= 5) {
            top5 += 1;
        }
        // The words, the camelCase spelling, the file names and the exact
        // word lead. The other spelling of a digest or a protocol is the
        // next hit, behind the atom that was written that way.
        match question.query {
            "list scans" | "listScans" | "config json" | "utf8" | "file txt" | "mint" => {
                assert_eq!(rank, Some(1), "{}: {ids:?}", question.query);
            }
            "sha1" => {
                assert_eq!(ids.first().copied(), Some("sha-joined"), "{ids:?}");
                assert_eq!(rank, Some(2), "{ids:?}");
            }
            "SHA-1" => {
                assert_eq!(ids.first().copied(), Some("sha-hyphen"), "{ids:?}");
                assert_eq!(rank, Some(2), "{ids:?}");
            }
            "http2" => {
                assert_eq!(ids.first().copied(), Some("http-joined"), "{ids:?}");
                assert_eq!(rank, Some(2), "{ids:?}");
            }
            "HTTP/2" => {
                assert_eq!(ids.first().copied(), Some("http-slash"), "{ids:?}");
                assert_eq!(rank, Some(2), "{ids:?}");
            }
            other => panic!("unexpected question {other}"),
        }
    }
    assert_eq!(top1, 6, "top-1 over the ten questions");
    assert_eq!(top5, 10, "top-5 over the ten questions");
}
