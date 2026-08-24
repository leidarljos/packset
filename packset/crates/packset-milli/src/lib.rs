//! Embedded milli index over the seat pack.
//!
//! Atoms stay in memory.lmdb. This crate is the search projection:
//! one heed env, inverted lists, prefix FST, typo automata. Query
//! cost follows the query, not the number of years in the store.

use std::collections::HashSet;
use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use milli::documents::{DocumentsBatchBuilder, DocumentsBatchReader};
use milli::heed::EnvOpenOptions;
use milli::update::{
    ClearDocuments, DeleteDocuments, IndexDocuments, IndexDocumentsConfig, IndexerConfig, Settings,
};
use milli::{Index, Search, SearchResult};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Default LMDB map size for the search projection. 4 GiB covers
/// hundreds of thousands of short atoms with room to grow.
pub const DEFAULT_MAP_SIZE: usize = 4 * 1024 * 1024 * 1024;

const SEARCHABLE: &[&str] = &["text", "entities", "kind"];
const FILTERABLE: &[&str] = &["workspace", "field", "kind", "set"];
const DISPLAYED: &[&str] = &[
    "id",
    "field",
    "kind",
    "text",
    "entities",
    "workspace",
    "set",
    "trust",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hit {
    pub id: Option<String>,
    pub field: String,
    pub kind: String,
    pub text: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchOut {
    pub hits: Vec<Hit>,
    pub engine: String,
    pub estimated_total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexOut {
    pub indexed: u64,
    pub documents: u64,
}

pub fn open_index(path: &Path, map_size: usize) -> Result<Index> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("create milli index {}", path.display()))?;
    let mut options = EnvOpenOptions::new();
    options.map_size(map_size);
    Index::new(options, path).map_err(|err| anyhow!(err.to_string()))
}

fn apply_settings(index: &Index) -> Result<()> {
    let config = IndexerConfig::default();
    // Primary key can only be set once. Re-applying it on every upsert
    // fails with "Index already has a primary key" and drops the write.
    let needs_primary = {
        let rtxn = index.read_txn().map_err(|err| anyhow!(err.to_string()))?;
        let primary = index
            .primary_key(&rtxn)
            .map_err(|err| anyhow!(err.to_string()))?;
        primary.is_none()
    };
    let mut wtxn = index.write_txn().map_err(|err| anyhow!(err.to_string()))?;
    let mut settings = Settings::new(&mut wtxn, index, &config);
    if needs_primary {
        settings.set_primary_key("id".to_owned());
    }
    settings.set_searchable_fields(SEARCHABLE.iter().map(|s| (*s).to_owned()).collect());
    settings.set_displayed_fields(DISPLAYED.iter().map(|s| (*s).to_owned()).collect());
    settings.set_filterable_fields(filterable_set());
    // Match the Python scorer: one-edit typos from length 4.
    settings.set_min_word_len_one_typo(4);
    settings.set_autorize_typos(true);
    settings
        .execute(|_| (), || false)
        .map_err(|err| anyhow!(err.to_string()))?;
    wtxn.commit().map_err(|err| anyhow!(err.to_string()))?;
    Ok(())
}

fn documents_reader(
    docs: impl IntoIterator<Item = Map<String, Value>>,
) -> Result<DocumentsBatchReader<Cursor<Vec<u8>>>> {
    let mut builder = DocumentsBatchBuilder::new(Vec::new());
    for doc in docs {
        builder
            .append_json_object(&doc)
            .map_err(|err| anyhow!(err.to_string()))?;
    }
    let bytes = builder
        .into_inner()
        .map_err(|err| anyhow!(err.to_string()))?;
    DocumentsBatchReader::from_reader(Cursor::new(bytes)).map_err(|err| anyhow!(err.to_string()))
}

fn object_from_value(value: Value) -> Result<Map<String, Value>> {
    match value {
        Value::Object(map) => Ok(map),
        other => Err(anyhow!("document must be a JSON object, got {other}")),
    }
}

/// Index JSON documents. `replace` clears the projection first.
pub fn index_documents(
    path: &Path,
    docs: impl IntoIterator<Item = Value>,
    replace: bool,
    map_size: usize,
) -> Result<IndexOut> {
    let index = open_index(path, map_size)?;
    apply_settings(&index)?;
    let objects: Vec<Map<String, Value>> = docs
        .into_iter()
        .map(object_from_value)
        .collect::<Result<_>>()?;
    let count_in = objects.len() as u64;
    let reader = documents_reader(objects)?;
    let config = IndexerConfig::default();
    let indexing = IndexDocumentsConfig::default();
    let mut wtxn = index.write_txn().map_err(|err| anyhow!(err.to_string()))?;
    if replace {
        ClearDocuments::new(&mut wtxn, &index)
            .execute()
            .map_err(|err| anyhow!(err.to_string()))?;
    }
    let builder = IndexDocuments::new(&mut wtxn, &index, &config, indexing, |_| (), || false)
        .map_err(|err| anyhow!(err.to_string()))?;
    let (builder, added) = builder
        .add_documents(reader)
        .map_err(|err| anyhow!(err.to_string()))?;
    added.map_err(|err| anyhow!(err.to_string()))?;
    let result = builder.execute().map_err(|err| anyhow!(err.to_string()))?;
    wtxn.commit().map_err(|err| anyhow!(err.to_string()))?;
    Ok(IndexOut {
        indexed: count_in,
        documents: result.number_of_documents,
    })
}

pub fn delete_documents(path: &Path, ids: &[String], map_size: usize) -> Result<u64> {
    let index = open_index(path, map_size)?;
    let mut wtxn = index.write_txn().map_err(|err| anyhow!(err.to_string()))?;
    let mut builder =
        DeleteDocuments::new(&mut wtxn, &index).map_err(|err| anyhow!(err.to_string()))?;
    for id in ids {
        builder.delete_external_id(id);
    }
    let result = builder.execute().map_err(|err| anyhow!(err.to_string()))?;
    wtxn.commit().map_err(|err| anyhow!(err.to_string()))?;
    Ok(result.deleted_documents)
}

fn quote_filter_value(raw: &str) -> String {
    format!("\"{}\"", raw.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn search(
    path: &Path,
    query: &str,
    workspace: Option<&str>,
    set: Option<&str>,
    limit: usize,
    map_size: usize,
) -> Result<SearchOut> {
    let index = open_index(path, map_size)?;
    let rtxn = index.read_txn().map_err(|err| anyhow!(err.to_string()))?;
    let mut search = Search::new(&rtxn, &index);
    search.query(query);
    search.limit(limit.max(1));
    search.authorize_typos(true);
    let mut clauses: Vec<String> = Vec::new();
    if let Some(workspace) = workspace {
        if !workspace.is_empty() {
            clauses.push(format!("workspace = {}", quote_filter_value(workspace)));
        }
    }
    if let Some(set) = set {
        if !set.is_empty() {
            clauses.push(format!("set = {}", quote_filter_value(set)));
        }
    }
    let filter_owned = clauses.join(" AND ");
    if !filter_owned.is_empty() {
        if let Some(filter) =
            milli::Filter::from_str(&filter_owned).map_err(|err| anyhow!(err.to_string()))?
        {
            search.filter(filter);
        }
    }
    let SearchResult {
        documents_ids,
        candidates,
        ..
    } = search.execute().map_err(|err| anyhow!(err.to_string()))?;
    let fields = index
        .fields_ids_map(&rtxn)
        .map_err(|err| anyhow!(err.to_string()))?;
    let displayed: Vec<_> = match index
        .displayed_fields_ids(&rtxn)
        .map_err(|err| anyhow!(err.to_string()))?
    {
        Some(ids) => ids,
        None => fields.iter().map(|(id, _)| id).collect(),
    };
    let docs = index
        .documents(&rtxn, documents_ids.clone())
        .map_err(|err| anyhow!(err.to_string()))?;
    let n = documents_ids.len().max(1) as f64;
    let mut hits = Vec::with_capacity(docs.len());
    for (rank, (_docid, obkv)) in docs.into_iter().enumerate() {
        let obj = milli::obkv_to_json(&displayed, &fields, obkv)
            .map_err(|err| anyhow!(err.to_string()))?;
        let field = obj
            .get("field")
            .and_then(Value::as_str)
            .unwrap_or("atom")
            .to_owned();
        let id = obj.get("id").and_then(Value::as_str).map(str::to_owned);
        let id = match field.as_str() {
            "user" | "memory" => None,
            _ => id,
        };
        hits.push(Hit {
            id,
            field,
            kind: obj
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            text: obj
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            score: (n - rank as f64) / n,
        });
    }
    Ok(SearchOut {
        hits,
        engine: "milli".to_owned(),
        estimated_total: candidates.len(),
    })
}

pub fn read_jsonl(mut input: impl Read) -> Result<Vec<Value>> {
    let mut buf = String::new();
    input.read_to_string(&mut buf)?;
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        let docs: Vec<Value> = serde_json::from_str(trimmed)?;
        return Ok(docs);
    }
    let mut docs = Vec::new();
    for (i, line) in buf.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        docs.push(serde_json::from_str(line).with_context(|| format!("jsonl line {}", i + 1))?);
    }
    Ok(docs)
}

pub fn parse_map_size(raw: Option<&str>) -> Result<usize> {
    match raw {
        None | Some("") => Ok(DEFAULT_MAP_SIZE),
        Some(value) => value
            .parse::<usize>()
            .with_context(|| format!("map size {value}")),
    }
}

fn filterable_set() -> HashSet<String> {
    FILTERABLE.iter().map(|s| (*s).to_owned()).collect()
}
