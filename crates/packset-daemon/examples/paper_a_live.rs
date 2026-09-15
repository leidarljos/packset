//! Live Paper A table from published datasets on disk.
//!
//! Writes go through `admit_seat_write`. Each session or fact is offered
//! raw (counts as a refusal) and as `Remember: …` (the seat write).
//! Retrieval is lexical over admitted claims only. Hit@1 is answer-span
//! overlap in the top claim. This is a measured table, not a fixture and
//! not a reader-model SOTA number.
//!
//! ```console
//! $ PAPER_A_LME=~/data/longmemeval_s.json PAPER_A_MAB=~/data/mab \
//!     cargo run --release -p packset-daemon --example paper_a_live
//! ```

use std::path::{Path, PathBuf};

use packset_core::admit_seat_write;
use packset_core::bm25::Index;
use packset_core::search::atom_tokens;
use serde_json::Value;

const TOP: usize = 5;
const CHUNK_WORDS: usize = 384;

struct Row {
    admitted: usize,
    refused: usize,
    asked: usize,
    hit: usize,
}

impl Row {
    fn new() -> Self {
        Self {
            admitted: 0,
            refused: 0,
            asked: 0,
            hit: 0,
        }
    }
}

fn normal(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn bearing(claims: &[String], answers: &[String]) -> bool {
    let hay = format!(" {} ", claims.iter().map(|c| normal(c)).collect::<Vec<_>>().join(" "));
    answers
        .iter()
        .map(|a| normal(a))
        .filter(|a| !a.is_empty())
        .any(|a| hay.contains(&format!(" {a} ")))
}

fn ingest(raw: &str, claims: &mut Vec<String>, row: &mut Row) {
    if admit_seat_write(raw).is_none() {
        row.refused += 1;
    }
    let one_line: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let wrapped = format!("Remember: {one_line}");
    if admit_seat_write(&wrapped).is_some() {
        row.admitted += 1;
        claims.push(one_line);
    }
}

fn retrieve<'a>(question: &str, claims: &'a [String]) -> Vec<&'a str> {
    if claims.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Vec<String>> = claims
        .iter()
        .map(|c| atom_tokens(serde_json::json!({"text": c}).as_object().unwrap()))
        .collect();
    let index = Index::build(docs.iter().map(|d| d.as_slice()));
    let q = atom_tokens(serde_json::json!({"text": question}).as_object().unwrap());
    let mut ranked = index.score(&q);
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked
        .into_iter()
        .take(TOP)
        .map(|(i, _)| claims[i].as_str())
        .collect()
}

fn ask(question: &str, answers: &[String], claims: &[String], row: &mut Row) {
    row.asked += 1;
    let top: Vec<String> = retrieve(question, claims)
        .into_iter()
        .map(str::to_string)
        .collect();
    if bearing(&top, answers) {
        row.hit += 1;
    }
}

fn session_text(session: &Value) -> String {
    let Some(turns) = session.as_array() else {
        return session.as_str().unwrap_or("").to_string();
    };
    let mut out = String::new();
    for turn in turns {
        let role = turn["role"].as_str().unwrap_or("");
        let content = turn["content"].as_str().unwrap_or("");
        if !content.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(role);
            out.push_str(": ");
            out.push_str(content);
        }
    }
    out
}

fn run_lme(path: &Path) -> anyhow::Result<Row> {
    let items: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let mut row = Row::new();
    for item in &items {
        let kind = item["question_type"].as_str().unwrap_or("");
        if kind.ends_with("_abs") {
            continue;
        }
        let question = item["question"].as_str().unwrap_or("");
        let answer = item["answer"].as_str().unwrap_or("");
        if question.is_empty() || answer.is_empty() {
            continue;
        }
        let mut claims = Vec::new();
        if let Some(sessions) = item["haystack_sessions"].as_array() {
            for session in sessions {
                let text = session_text(session);
                if !text.is_empty() {
                    ingest(&text, &mut claims, &mut row);
                }
            }
        }
        ask(question, &[answer.to_string()], &claims, &mut row);
    }
    Ok(row)
}

fn facts(context: &str) -> Vec<String> {
    context
        .lines()
        .map(str::trim)
        .filter(|l| l.chars().next().is_some_and(|c| c.is_ascii_digit()) && l.contains(". "))
        .map(str::to_string)
        .collect()
}

fn chunks(context: &str) -> Vec<String> {
    let words: Vec<&str> = context.split_whitespace().collect();
    words
        .chunks(CHUNK_WORDS)
        .map(|w| w.join(" "))
        .filter(|s| !s.is_empty())
        .collect()
}

fn run_mab(path: &Path, fact_list: bool) -> anyhow::Result<Row> {
    let text = std::fs::read_to_string(path)?;
    let mut row = Row::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line)?;
        let context = v["context"].as_str().unwrap_or("");
        let units = if fact_list {
            facts(context)
        } else {
            chunks(context)
        };
        let mut claims = Vec::new();
        for unit in &units {
            ingest(unit, &mut claims, &mut row);
        }
        let questions = v["questions"].as_array().cloned().unwrap_or_default();
        let answers = v["answers"].as_array().cloned().unwrap_or_default();
        for (i, q) in questions.iter().enumerate() {
            let q = q.as_str().unwrap_or("");
            if q.is_empty() {
                continue;
            }
            let ans: Vec<String> = answers
                .get(i)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .or_else(|| answers.get(i).and_then(Value::as_str).map(|s| vec![s.to_string()]))
                .unwrap_or_default();
            ask(q, &ans, &claims, &mut row);
        }
    }
    Ok(row)
}

fn print_row(name: &str, row: &Row) {
    let rate = if row.asked == 0 {
        "—".into()
    } else {
        format!("{:.3}", row.hit as f64 / row.asked as f64)
    };
    println!(
        "| {name} | {} | {} | {} | {} | {rate} |",
        row.admitted, row.refused, row.asked, row.hit
    );
}

fn main() -> anyhow::Result<()> {
    let lme = std::env::var("PAPER_A_LME").map_err(|_| {
        anyhow::anyhow!("PAPER_A_LME must name the LongMemEval_S json")
    })?;
    let mab = PathBuf::from(std::env::var("PAPER_A_MAB").map_err(|_| {
        anyhow::anyhow!("PAPER_A_MAB must name the MemoryAgentBench jsonl directory")
    })?);
    println!("measured write-policy table (lexical hit@1 over Remember: admits)");
    println!("not a reader-model SOTA number; not the fixture table\n");
    println!("| bench | admitted | refused | asked | hit | hit@1 |");
    println!("|---|---:|---:|---:|---:|---:|");
    let lme_row = run_lme(Path::new(&lme))?;
    print_row("LongMemEval_S", &lme_row);
    let ar = run_mab(&mab.join("Accurate_Retrieval.jsonl"), false)?;
    print_row("MemoryAgentBench", &ar);
    let cr = run_mab(&mab.join("Conflict_Resolution.jsonl"), true)?;
    print_row("MemConflict", &cr);
    Ok(())
}
