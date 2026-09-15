//! `packset-embed`: the dense projection as a separate process. One JSON line
//! in, one out, flushed, until the input closes; keep it open, loading the
//! model is the cost.
//!
//! ```console
//! $ echo '{"id":"a","text":"Prefer ripgrep for search."}' | packset-embed
//! {"id":"a","v":[...]}
//! $ echo '{"id":"q","text":"which search tool","query":true}' | packset-embed
//! ```
//!
//! Documents and questions take different prefixes per model family (BGE
//! instructs the question, E5 prefixes both), applied here beside the name.

use std::io::{BufRead, Write};

use fastembed::{
    Bgem3Embedding, Bgem3InitOptions, Bgem3Model, EmbeddingModel, InitOptionsUserDefined, Pooling,
    RerankInitOptions, RerankerModel, SparseInitOptions, SparseModel, SparseTextEmbedding,
    TextEmbedding, TextInitOptions, TextRerank, TokenizerFiles, UserDefinedEmbeddingModel,
};
use serde::{Deserialize, Serialize};

/// One thing to encode.
#[derive(Deserialize)]
struct Item {
    id: String,
    #[serde(default)]
    text: String,
    /// Several texts, one forward pass. Empty means `text` is the only one.
    #[serde(default)]
    texts: Vec<String>,
    /// When true, the query prefix. One kept process can encode both sides.
    #[serde(default)]
    query: bool,
}

/// One thing encoded as a single vector.
#[derive(Serialize)]
struct Vector {
    id: String,
    v: Vec<f32>,
}

/// Several texts encoded in one forward pass.
#[derive(Serialize)]
struct Vectors {
    id: String,
    vs: Vec<Vec<f32>>,
}

/// A question and the candidates to put it against.
#[derive(Deserialize)]
struct Pairing {
    id: String,
    /// What was asked.
    q: String,
    /// The candidate texts, in the order the first stage returned them.
    d: Vec<String>,
}

/// Cross-encoder scores, in the caller's order.
#[derive(Serialize)]
struct Scored {
    id: String,
    s: Vec<f32>,
}

/// One text from one pass: a vector per token for late interaction, the
/// pooled vector, and the learned term weights.
#[derive(Serialize)]
struct Tokens {
    id: String,
    t: Vec<Vec<f32>>,
    v: Vec<f32>,
    /// Learned term weights from the same pass.
    s: Sparse,
}

/// Learned term weights, as the pair of arrays the model returns.
#[derive(Serialize, Default)]
struct Sparse {
    i: Vec<u32>,
    w: Vec<f32>,
}

/// A model, and the instructions its family wants in front of a text.
///
/// The prefixes are part of the model rather than decoration. BGE was trained
/// with a retrieval instruction on the question only; E5 was trained with a
/// word on both sides. Using the wrong pair costs recall silently, which is
/// why they live beside the name they belong to instead of being a default.
struct Choice {
    source: Source,
    query: &'static str,
    passage: &'static str,
}

fn prefix_of(choice: &Choice, query: bool) -> &'static str {
    if query {
        choice.query
    } else {
        choice.passage
    }
}

/// Where a model's weights come from.
///
/// The runtime ships a list of models it knows how to fetch, and the list does
/// not include everything a measurement has to be made against. The published
/// system on the benchmark this seat is measured on used English e5-large-v2,
/// which the runtime does not carry; it carries the multilingual e5-large,
/// which is a different model with a different training set and lower scores
/// on English retrieval. Comparing against the paper with the wrong one of the
/// two is comparing against nothing, so the one the paper used is loaded from
/// files a seat puts under its cache.
enum Source {
    Builtin(EmbeddingModel),
    /// `<cache>/user/<dir>/` holding `onnx/model.onnx`, `tokenizer.json`,
    /// `config.json`, `special_tokens_map.json` and `tokenizer_config.json`,
    /// as a Hub repository lays them out.
    Files {
        dir: &'static str,
        pooling: Pooling,
    },
}

/// BGE's instruction, on the question only.
const BGE_QUERY: &str = "Represent this sentence for searching relevant passages: ";

/// The models this binary will load, by the name a seat writes.
fn choose(name: &str) -> Option<Choice> {
    use Source::Builtin;
    let (source, query, passage) = match name {
        "bge-small" | "" => (Builtin(EmbeddingModel::BGESmallENV15), BGE_QUERY, ""),
        "bge-base" => (Builtin(EmbeddingModel::BGEBaseENV15), BGE_QUERY, ""),
        "bge-large" => (Builtin(EmbeddingModel::BGELargeENV15), BGE_QUERY, ""),
        // Multilingual, and named so. The English model the published system
        // used is `e5-large-v2` below; these two are not interchangeable and
        // a table that says one while running the other is wrong.
        "e5-large" | "multilingual-e5-large" => (
            Builtin(EmbeddingModel::MultilingualE5Large),
            "query: ",
            "passage: ",
        ),
        "e5-base" => (
            Builtin(EmbeddingModel::MultilingualE5Base),
            "query: ",
            "passage: ",
        ),
        "e5-large-v2" => (
            Source::Files {
                dir: "e5-large-v2",
                pooling: Pooling::Mean,
            },
            "query: ",
            "passage: ",
        ),
        "gte-large" => (Builtin(EmbeddingModel::GTELargeENV15), "", ""),
        "mxbai-large" => (
            Builtin(EmbeddingModel::MxbaiEmbedLargeV1),
            "Represent this sentence for searching relevant passages: ",
            "",
        ),
        _ => return None,
    };
    Some(Choice {
        source,
        query,
        passage,
    })
}

/// Every name [`choose`] answers to, for the error that lists them.
const KNOWN: &str = "bge-small, bge-base, bge-large, e5-base, e5-large (multilingual), \
                     e5-large-v2 (English, from files), gte-large, mxbai-large";

/// Where models live: `PACKSET_EMBED_CACHE`, else `$XDG_CACHE_HOME/packset/embed`,
/// else `~/.cache/packset/embed`. Never the working directory.
fn cache_dir() -> Option<std::path::PathBuf> {
    cache_dir_from(
        std::env::var_os("PACKSET_EMBED_CACHE"),
        std::env::var_os("XDG_CACHE_HOME"),
        std::env::var_os("HOME"),
    )
}

fn cache_dir_from(
    named: Option<std::ffi::OsString>,
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<std::path::PathBuf> {
    if let Some(dir) = named.filter(|d| !d.is_empty()) {
        return Some(std::path::PathBuf::from(dir));
    }
    let base = match xdg.filter(|d| !d.is_empty()) {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::PathBuf::from(home?).join(".cache"),
    };
    Some(base.join("packset").join("embed"))
}

/// Load the model a choice names.
fn load(choice: &Choice) -> anyhow::Result<TextEmbedding> {
    match &choice.source {
        Source::Builtin(model) => {
            let mut options =
                TextInitOptions::new(model.clone()).with_show_download_progress(false);
            if let Some(dir) = cache_dir() {
                options = options.with_cache_dir(dir);
            }
            Ok(TextEmbedding::try_new(options)?)
        }
        Source::Files { dir, pooling } => {
            let root = cache_dir()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "a model from files needs PACKSET_EMBED_CACHE, so the files have a place \
                         to be"
                    )
                })?
                .join("user")
                .join(dir);
            let read = |name: &str| {
                std::fs::read(root.join(name))
                    .map_err(|e| anyhow::anyhow!("{name} under {}: {e}", root.display()))
            };
            let files = TokenizerFiles {
                tokenizer_file: read("tokenizer.json")?,
                config_file: read("config.json")?,
                special_tokens_map_file: read("special_tokens_map.json")?,
                tokenizer_config_file: read("tokenizer_config.json")?,
            };
            let model = UserDefinedEmbeddingModel::new(read("onnx/model.onnx")?, files)
                .with_pooling(pooling.clone());
            Ok(TextEmbedding::try_new_from_user_defined(
                model,
                InitOptionsUserDefined::default(),
            )?)
        }
    }
}

fn main() -> anyhow::Result<()> {
    let mut query = false;
    let mut late = false;
    let mut rerank = false;
    let mut sparse = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--query" | "-q" => query = true,
            "--late" => late = true,
            "--rerank" => rerank = true,
            "--sparse" => sparse = true,
            "-V" | "--version" => {
                println!("packset-embed {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument: {other}\n\n{USAGE}"),
        }
    }

    // Named so a seat can put the weights where its policy allows, and so a
    // build machine and a run machine can share one copy.
    if rerank {
        return cross_encode();
    }
    if sparse {
        return learned_sparse();
    }
    if late {
        return late_interaction(query);
    }

    let name = std::env::var("PACKSET_EMBED_MODEL").unwrap_or_default();
    let choice = choose(name.trim())
        .ok_or_else(|| anyhow::anyhow!("unknown model `{name}`; known: {KNOWN}"))?;
    let mut model = load(&choice)?;

    // A line in, a line out, flushed. Loading the model is the expensive part
    // and a caller that has to pay it per question cannot afford to ask, so
    // this process is meant to be kept rather than spawned: it answers until
    // its input closes.
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let item: Item = serde_json::from_str(&line)?;
        let as_query = query || item.query;
        let prefix = prefix_of(&choice, as_query);
        let mut raw = item.texts;
        if raw.is_empty() && !item.text.is_empty() {
            raw.push(item.text);
        }
        let prepared: Vec<String> = raw
            .into_iter()
            .map(|t| {
                if prefix.is_empty() {
                    t
                } else {
                    format!("{prefix}{t}")
                }
            })
            .collect();
        let vectors = if prepared.is_empty() {
            Vec::new()
        } else {
            model.embed(&prepared, None)?
        };
        if vectors.len() == 1 {
            writeln!(
                out,
                "{}",
                serde_json::to_string(&Vector {
                    id: item.id,
                    v: vectors.into_iter().next().unwrap_or_default(),
                })?
            )?;
        } else {
            writeln!(
                out,
                "{}",
                serde_json::to_string(&Vectors {
                    id: item.id,
                    vs: vectors,
                })?
            )?;
        }
        out.flush()?;
    }
    Ok(())
}

/// BGE-M3, which returns a vector per token from the same pass.
///
/// A separate path rather than a model name, because what it emits is a
/// different shape and a caller that asked for one and got the other would
/// score nonsense rather than fail.
fn late_interaction(query: bool) -> anyhow::Result<()> {
    // The int8 quantisation is the only BGE-M3 on offer here, and it is worth
    // saying out loud: a number from it sits a little under what the
    // full-precision model would give, so it bounds late interaction from
    // below rather than measuring it exactly.
    let mut options = Bgem3InitOptions::new(Bgem3Model::BGEM3Q).with_show_download_progress(false);
    if let Some(dir) = cache_dir() {
        options = options.with_cache_dir(dir);
    }
    let mut model = Bgem3Embedding::try_new(options)?;

    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let item: Item = serde_json::from_str(&line)?;
        // BGE-M3 takes no instruction prefix of its own; the flag stays so one
        // caller drives both paths the same way.
        let _ = query;
        let mut encoded = model.embed(&[item.text], None)?;
        let t = if encoded.colbert.is_empty() {
            Vec::new()
        } else {
            encoded.colbert.remove(0)
        };
        // The pooled vector from the same forward pass, so a caller can compare
        // the two scorings without changing the model underneath them.
        let v = if encoded.dense.is_empty() {
            Vec::new()
        } else {
            encoded.dense.remove(0)
        };
        // The learned sparse weights, also from that pass. BGE-M3 is trained to
        // emit all three, and a caller that already paid for the forward pass
        // has this for nothing.
        let s = if encoded.sparse.is_empty() {
            Sparse::default()
        } else {
            let raw = encoded.sparse.remove(0);
            Sparse {
                i: raw.indices.iter().map(|index| *index as u32).collect(),
                w: raw.values,
            }
        };
        writeln!(
            out,
            "{}",
            serde_json::to_string(&Tokens {
                id: item.id,
                t,
                v,
                s
            })?
        )?;
        out.flush()?;
    }
    Ok(())
}

/// The cross-encoder that reads a question and a candidate together.
///
/// Every other path here is a bi-encoder: a text is embedded once, without the
/// question, and the score is a geometry between two vectors made in ignorance
/// of each other. That is what makes a first stage cheap, and it is also its
/// ceiling. A cross-encoder reads the pair in one forward pass, so it can
/// answer whether this text answers this question rather than whether the two
/// are about the same subject.
///
/// The cost is the other half of the trade and it is not small: a bi-encoder
/// embeds a corpus once and answers every question from the stored vectors,
/// where this runs a forward pass per candidate per question. That is why it
/// is a second stage over the top of a ranking rather than a scorer over a
/// pack, and why a seat that turns it on is buying accuracy with latency.
///
/// Nogueira and Cho (doi:10.48550/arXiv.1901.04085) is the result this is;
/// monoT5 (doi:10.18653/v1/2020.findings-emnlp.63) is the same structure with
/// a sequence-to-sequence model.
fn cross_encode() -> anyhow::Result<()> {
    let name = std::env::var("PACKSET_RERANK_MODEL").unwrap_or_default();
    let model = match name.trim() {
        "bge-reranker-base" | "" => RerankerModel::BGERerankerBase,
        "bge-reranker-v2-m3" => RerankerModel::BGERerankerV2M3,
        "jina-turbo" => RerankerModel::JINARerankerV1TurboEn,
        "jina-v2" => RerankerModel::JINARerankerV2BaseMultiligual,
        other => anyhow::bail!("unknown reranker `{other}`; known: {RERANKERS}"),
    };
    let mut options = RerankInitOptions::new(model).with_show_download_progress(false);
    if let Some(dir) = cache_dir() {
        options = options.with_cache_dir(dir);
    }
    let mut reranker = TextRerank::try_new(options)?;

    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let pairing: Pairing = serde_json::from_str(&line)?;
        // An empty candidate list is a question the first stage answered with
        // nothing, which is not an error and must not cost a model call.
        let mut scores = vec![0.0f32; pairing.d.len()];
        if !pairing.d.is_empty() {
            let texts: Vec<&str> = pairing.d.iter().map(String::as_str).collect();
            for result in reranker.rerank(pairing.q.as_str(), &texts, false, None)? {
                if let Some(slot) = scores.get_mut(result.index) {
                    *slot = result.score;
                }
            }
        }
        writeln!(
            out,
            "{}",
            serde_json::to_string(&Scored {
                id: pairing.id,
                s: scores
            })?
        )?;
        out.flush()?;
    }
    Ok(())
}

/// Learned sparse weights from a model trained to produce them.
///
/// BGE-M3 hands back a sparse head from the same pass as its dense vector,
/// and that head measured below BM25 here. It is not the sparse model the
/// literature means. SPLADE (doi:10.1145/3404835.3463098, and the distilled
/// hard-negative form at doi:10.1145/3477495.3531857) is trained for the
/// weights themselves, with a regularizer that keeps them sparse enough to
/// live in an inverted index. Measuring "learned sparse" against BM25 with a
/// side output of a dense model is measuring the wrong thing, and this is the
/// right one.
///
/// Same line shape as the sparse field the late path emits, so a caller reads
/// both with one parser.
fn learned_sparse() -> anyhow::Result<()> {
    let mut options =
        SparseInitOptions::new(SparseModel::SPLADEPPV1).with_show_download_progress(false);
    if let Some(dir) = cache_dir() {
        options = options.with_cache_dir(dir);
    }
    let mut model = SparseTextEmbedding::try_new(options)?;

    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let item: Item = serde_json::from_str(&line)?;
        let mut encoded = model.embed(&[item.text], None)?;
        let s = encoded.pop().map_or_else(Sparse::default, |raw| Sparse {
            i: raw.indices.iter().map(|index| *index as u32).collect(),
            w: raw.values,
        });
        writeln!(
            out,
            "{}",
            serde_json::to_string(
                &serde_json::json!({ "id": item.id, "s": { "i": s.i, "w": s.w } })
            )?
        )?;
        out.flush()?;
    }
    Ok(())
}

/// Every reranker [`cross_encode`] answers to, for the error that lists them.
const RERANKERS: &str = "bge-reranker-base, bge-reranker-v2-m3, jina-turbo, jina-v2";

const USAGE: &str = "packset-embed: text in, vectors out\n\
    \n\
    reads JSON lines {\"id\",\"text\"} and writes {\"id\",\"v\"}\n\
    \n\
        --query   encode as a question rather than a document\n\
        --late    a vector per token, for late interaction (BGE-M3)\n\
        --rerank  a cross-encoder second stage: reads {\"id\",\"q\",\"d\":[..]}\n\
                  and writes {\"id\",\"s\":[..]}, one score a candidate in the\n\
                  order given\n\
        --sparse  learned sparse weights (SPLADE++): writes {\"id\",\"s\":{\"i\",\"w\"}}\n\
    \n\
        PACKSET_EMBED_CACHE   where the weights live\n\
        PACKSET_EMBED_MODEL   bge-small (default), bge-base, bge-large,\n\
                              e5-base, e5-large (multilingual), gte-large,\n\
                              mxbai-large, or e5-large-v2 from files under\n\
                              PACKSET_EMBED_CACHE/user/e5-large-v2/\n\
        PACKSET_RERANK_MODEL  bge-reranker-base (default), bge-reranker-v2-m3,\n\
                              jina-turbo, jina-v2";

#[cfg(test)]
mod tests {
    use super::cache_dir_from;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn a_model_never_lands_in_the_working_directory() {
        let named = Some(OsString::from("/models"));
        let xdg = Some(OsString::from("/xdg"));
        let home = Some(OsString::from("/home/seat"));
        assert_eq!(
            cache_dir_from(named, xdg.clone(), home.clone()),
            Some(PathBuf::from("/models"))
        );
        assert_eq!(
            cache_dir_from(None, xdg, home.clone()),
            Some(PathBuf::from("/xdg/packset/embed"))
        );
        assert_eq!(
            cache_dir_from(Some(OsString::new()), None, home),
            Some(PathBuf::from("/home/seat/.cache/packset/embed"))
        );
        assert_eq!(cache_dir_from(None, None, None), None);
    }

    #[test]
    fn one_process_picks_query_or_passage_per_line() {
        let choice = super::choose("").expect("bge-small");
        assert!(!super::prefix_of(&choice, true).is_empty());
        assert!(super::prefix_of(&choice, false).is_empty());
        let e5 = super::choose("e5-base").expect("e5-base");
        assert_eq!(super::prefix_of(&e5, true), "query: ");
        assert_eq!(super::prefix_of(&e5, false), "passage: ");
        let item: super::Item =
            serde_json::from_str(r#"{"id":"q","text":"which tool","query":true}"#).unwrap();
        assert!(item.query);
        let doc: super::Item = serde_json::from_str(r#"{"id":"d","text":"ripgrep"}"#).unwrap();
        assert!(!doc.query);
        let batch: super::Item =
            serde_json::from_str(r#"{"id":"b","query":true,"texts":["a","b"]}"#).unwrap();
        assert_eq!(batch.texts.len(), 2);
        assert!(batch.query);
    }
}
