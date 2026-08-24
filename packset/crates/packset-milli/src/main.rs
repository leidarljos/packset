//! CLI over the embedded milli index.
//!
//!   packset-milli index  --index DIR [--replace] [--map-size N]   < docs.jsonl
//!   packset-milli search  --index DIR --q QUERY [--workspace WS] [--set SET] [--limit N]
//!   packset-milli delete  --index DIR [--ids a,b]                   < ids.json

use std::env;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{anyhow, Result};
use packset_milli::{delete_documents, index_documents, parse_map_size, read_jsonl, search};
use serde_json::{json, Value};

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn opt(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn require(args: &[String], name: &str) -> Result<String> {
    opt(args, name).ok_or_else(|| anyhow!("missing {name}"))
}

fn run() -> Result<Value> {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args
        .first()
        .map(String::as_str)
        .ok_or_else(|| anyhow!("usage: packset-milli index|search|delete"))?;
    let map_size = parse_map_size(opt(&args, "--map-size").as_deref())?;
    match cmd {
        "index" => {
            let path = PathBuf::from(require(&args, "--index")?);
            let docs = read_jsonl(io::stdin())?;
            let out = index_documents(&path, docs, flag(&args, "--replace"), map_size)?;
            Ok(serde_json::to_value(out)?)
        }
        "search" => {
            let path = PathBuf::from(require(&args, "--index")?);
            let query = require(&args, "--q")?;
            let workspace = opt(&args, "--workspace");
            let set = opt(&args, "--set");
            let limit = opt(&args, "--limit")
                .map(|raw| raw.parse::<usize>())
                .transpose()?
                .unwrap_or(16);
            let out = search(
                &path,
                &query,
                workspace.as_deref(),
                set.as_deref(),
                limit,
                map_size,
            )?;
            Ok(serde_json::to_value(out)?)
        }
        "delete" => {
            let path = PathBuf::from(require(&args, "--index")?);
            let mut ids: Vec<String> = opt(&args, "--ids")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            if ids.is_empty() {
                let mut buf = String::new();
                io::stdin().read_to_string(&mut buf)?;
                let trimmed = buf.trim();
                if !trimmed.is_empty() {
                    match serde_json::from_str::<Value>(trimmed)? {
                        Value::Array(items) => {
                            ids = items
                                .into_iter()
                                .filter_map(|v| v.as_str().map(str::to_owned))
                                .collect();
                        }
                        Value::String(one) => ids.push(one),
                        other => {
                            return Err(anyhow!("delete ids must be a JSON array, got {other}"))
                        }
                    }
                }
            }
            let deleted = delete_documents(&path, &ids, map_size)?;
            Ok(json!({"deleted": deleted}))
        }
        other => Err(anyhow!("unknown command {other}")),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(value) => {
            println!("{}", value);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{}", json!({"error": err.to_string()}));
            ExitCode::from(1)
        }
    }
}
