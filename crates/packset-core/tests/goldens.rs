//! The accept and reject boundary, frozen.
//!
//! `goldens.json` is checked-in data rather than generated output: it fixes
//! which records the writer stores, which it refuses, and the exact words it
//! refuses them with. Read a diff here as a change in what the daemon accepts,
//! not as a test being updated to match the code.

use serde_json::Value;

fn goldens() -> Value {
    let raw = include_str!("goldens.json");
    serde_json::from_str(raw).expect("goldens parse")
}

fn as_usize(v: &Value) -> usize {
    v.as_u64().expect("count") as usize
}

#[test]
fn prose_assessment_matches_the_corpus() {
    for case in goldens()["prose"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let got = packset_core::prose::assess(text);
        assert_eq!(got.words, as_usize(&case["words"]), "words for {text:?}");
        assert_eq!(
            got.sentences,
            as_usize(&case["sentences"]),
            "sentences for {text:?}"
        );
        assert_eq!(
            got.adverbs,
            as_usize(&case["adverbs"]),
            "adverbs for {text:?}"
        );
        assert_eq!(
            got.passives,
            as_usize(&case["passives"]),
            "passives for {text:?}"
        );
        assert_eq!(
            got.hard_sentences,
            as_usize(&case["hard_sentences"]),
            "hard for {text:?}"
        );
        assert_eq!(
            got.very_hard_sentences,
            as_usize(&case["very_hard_sentences"]),
            "very hard for {text:?}"
        );
        match case["grade"].as_f64() {
            Some(want) => {
                let got = got.grade.unwrap_or_else(|| panic!("grade for {text:?}"));
                assert!(
                    (got - want).abs() < 1e-6,
                    "grade for {text:?}: {got} {want}"
                );
            }
            None => assert_eq!(got.grade, None, "grade for {text:?}"),
        }
        match case["ease"].as_f64() {
            Some(want) => {
                let got = got.ease.unwrap_or_else(|| panic!("ease for {text:?}"));
                assert!((got - want).abs() < 1e-6, "ease for {text:?}: {got} {want}");
            }
            None => assert_eq!(got.ease, None, "ease for {text:?}"),
        }
        let want_ratio = case["adverb_ratio"].as_f64().unwrap();
        assert!(
            (got.adverb_ratio - want_ratio).abs() < 1e-6,
            "adverb ratio for {text:?}"
        );
    }
}

#[test]
fn remote_normalization_matches_the_corpus() {
    for case in goldens()["remote"].as_array().unwrap() {
        let remote = case["remote"].as_str().unwrap();
        let got = packset_core::identity::normalize_remote(remote);
        match case["workspace"].as_str() {
            Some(want) => assert_eq!(got.as_deref(), Ok(want), "{remote}"),
            None => assert!(got.is_err(), "{remote} should not normalize"),
        }
    }
}

#[test]
fn workspace_slugs_match_the_corpus() {
    for case in goldens()["slug"].as_array().unwrap() {
        let workspace = case["workspace"].as_str().unwrap();
        let want = case["slug"].as_str().unwrap();
        assert_eq!(
            packset_core::identity::workspace_slug(workspace),
            want,
            "{workspace:?}"
        );
    }
}

#[test]
fn set_names_match_the_corpus() {
    for case in goldens()["set"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let got = packset_core::set_name::check(name);
        match case["set"].as_str() {
            Some(want) => assert_eq!(got.as_deref(), Ok(want), "{name:?}"),
            None => assert!(got.is_err(), "{name:?} should be refused"),
        }
    }
}

#[test]
fn atom_validation_matches_the_corpus() {
    for case in goldens()["atom"].as_array().unwrap() {
        let mut atom = case["input"].as_object().unwrap().clone();
        let shown = serde_json::to_string(&case["input"]).unwrap();
        let got = packset_core::record::validate(&mut atom);
        match case.get("error").and_then(Value::as_str) {
            Some(want) => {
                let err = got.unwrap_err();
                assert_eq!(err.0, want, "for {shown}");
            }
            None => {
                got.unwrap_or_else(|e| panic!("for {shown}: {e}"));
                let want = case["ok"].as_object().unwrap();
                // The normalized fields are the ones validate is responsible
                // for; the rest it must leave alone.
                for key in ["kind", "level", "text", "workspace", "set", "entities"] {
                    assert_eq!(atom.get(key), want.get(key), "{key} for {shown}");
                }
                assert!(atom.contains_key("prose"), "prose report for {shown}");
            }
        }
    }
}

#[test]
fn entity_extraction_matches_the_corpus() {
    for case in goldens()["entities"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        let mut atom = serde_json::Map::new();
        atom.insert("text".into(), Value::String(text.to_string()));
        let got: Vec<String> = packset_core::record::entities_of(&atom)
            .into_iter()
            .collect();
        let want: Vec<String> = case["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(got, want, "entities for {text:?}");
    }
}

#[test]
fn review_scheduling_matches_the_corpus() {
    let all = goldens();
    let clock = all["review_clock"].as_str().unwrap();
    for case in all["review"].as_array().unwrap() {
        let mut atom = serde_json::Map::new();
        atom.insert("id".into(), Value::String("a".into()));
        let block = case["review_in"].as_object().unwrap();
        if !block.is_empty() {
            atom.insert("review".into(), case["review_in"].clone());
        }
        let grade = match case["grade"].as_str().unwrap() {
            "recalled" => packset_core::record::Grade::Recalled,
            "lapsed" => packset_core::record::Grade::Lapsed,
            _ => packset_core::record::Grade::Initial,
        };
        let interval = case["interval_s"].as_i64();
        packset_core::record::schedule_review(&mut atom, clock, grade, interval);

        let shown = serde_json::to_string(case).unwrap();
        assert_eq!(
            atom["due_at"].as_str(),
            case["due_at"].as_str(),
            "due_at for {shown}"
        );
        let want = case["review"].as_object().unwrap();
        let got = atom["review"].as_object().unwrap();
        for (key, wanted) in want {
            let mine = got.get(key).unwrap_or_else(|| panic!("{key} for {shown}"));
            match wanted.as_f64() {
                Some(w) if wanted.is_f64() => {
                    let m = mine.as_f64().unwrap();
                    assert!((m - w).abs() < 1e-9, "{key} for {shown}: {m} vs {w}");
                }
                _ => assert_eq!(mine, wanted, "{key} for {shown}"),
            }
        }
    }
}

#[test]
fn link_rewriting_matches_the_corpus() {
    let all = goldens();
    let case = &all["links"];
    let clock = case["clock"].as_str().unwrap();

    let live: Vec<serde_json::Map<String, Value>> = case["live"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_object().unwrap().clone())
        .collect();
    let mut subject = case["subject_in"].as_object().unwrap().clone();

    let rewritten = packset_core::record::apply_links(
        &mut subject,
        &live,
        packset_core::record::LINK_THRESHOLD,
        clock,
    );

    assert_eq!(
        subject["links"], case["subject_links"],
        "the subject's own links"
    );

    let mut got: Vec<(String, Value)> = rewritten
        .iter()
        .map(|p| (p["id"].as_str().unwrap().to_string(), p["links"].clone()))
        .collect();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    let want: Vec<(String, Value)> = case["rewritten"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap().to_string(), p["links"].clone()))
        .collect();
    assert_eq!(got, want, "the peers that changed");
}

#[test]
fn dropping_links_outside_the_live_set_matches_the_corpus() {
    let all = goldens();
    let want = all["links"]["filtered"].as_array().unwrap();
    let mut atoms: Vec<serde_json::Map<String, Value>> = vec![
        serde_json::from_str(r#"{"id":"a","links":["b","gone"]}"#).unwrap(),
        serde_json::from_str(r#"{"id":"b","links":["a"]}"#).unwrap(),
    ];
    packset_core::record::filter_live_links(&mut atoms);
    for (got, wanted) in atoms.iter().zip(want) {
        assert_eq!(got["links"], wanted["links"], "for {}", got["id"]);
    }
}

#[test]
fn tool_dump_detection_matches_the_corpus() {
    for case in goldens()["tool_dump"].as_array().unwrap() {
        let text = case["text"].as_str().unwrap();
        assert_eq!(
            packset_core::extract::is_tool_dump(text),
            case["is_dump"].as_bool().unwrap(),
            "for {text:?}"
        );
    }
}

#[test]
fn claim_extraction_matches_the_corpus() {
    for case in goldens()["claim"].as_array().unwrap() {
        let line = case["line"].as_str().unwrap();
        let got = packset_core::extract::claim_from_user(line);
        match case.get("kind").and_then(Value::as_str) {
            Some(kind) => {
                let (got_kind, got_claim) =
                    got.unwrap_or_else(|| panic!("expected a claim for {line:?}"));
                assert_eq!(got_kind, kind, "kind for {line:?}");
                assert_eq!(
                    got_claim,
                    case["claim"].as_str().unwrap(),
                    "claim for {line:?}"
                );
            }
            None => assert!(got.is_none(), "{line:?} should not be a claim"),
        }
    }
}
