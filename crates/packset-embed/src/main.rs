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
//!
//! The binary links a system or source-built ONNX Runtime. The prebuilt
//! runtime from cdn.pyke.io is the `download-binaries` feature. A model
//! whose files are already on disk is read from there and not fetched.

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
    /// `<cache>/user/<seed>/`, or the directory `PACKSET_EMBED_MODEL_PATH` names.
    seed: &'static str,
    pooling: Pooling,
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
    /// No Hub fetch. The weights are the hub layout under the seed directory:
    /// `onnx/model.onnx`, `tokenizer.json`, `config.json`,
    /// `special_tokens_map.json` and `tokenizer_config.json`.
    Files,
}

/// BGE's instruction, on the question only.
const BGE_QUERY: &str = "Represent this sentence for searching relevant passages: ";

/// The models this binary will load, by the name a seat writes.
fn choose(name: &str) -> Option<Choice> {
    use EmbeddingModel as Model;
    use Pooling::{Cls, Mean};
    use Source::Builtin;
    let (source, query, passage, seed, pooling) = match name {
        "bge-small" | "" => (
            Builtin(Model::BGESmallENV15),
            BGE_QUERY,
            "",
            "bge-small",
            Cls,
        ),
        "bge-base" => (Builtin(Model::BGEBaseENV15), BGE_QUERY, "", "bge-base", Cls),
        "bge-large" => (
            Builtin(Model::BGELargeENV15),
            BGE_QUERY,
            "",
            "bge-large",
            Cls,
        ),
        // Multilingual, and named so. The English model the published system
        // used is `e5-large-v2` below; these two are not interchangeable and
        // a table that says one while running the other is wrong.
        "e5-large" | "multilingual-e5-large" => (
            Builtin(Model::MultilingualE5Large),
            "query: ",
            "passage: ",
            "e5-large",
            Mean,
        ),
        "e5-base" => (
            Builtin(Model::MultilingualE5Base),
            "query: ",
            "passage: ",
            "e5-base",
            Mean,
        ),
        "e5-large-v2" => (Source::Files, "query: ", "passage: ", "e5-large-v2", Mean),
        "gte-large" => (Builtin(Model::GTELargeENV15), "", "", "gte-large", Cls),
        "mxbai-large" => (
            Builtin(Model::MxbaiEmbedLargeV1),
            "Represent this sentence for searching relevant passages: ",
            "",
            "mxbai-large",
            Cls,
        ),
        _ => return None,
    };
    Some(Choice {
        source,
        query,
        passage,
        seed,
        pooling,
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

/// Hub layout a seeded model is read from. The same files a repository checkout holds.
const MODEL_FILES: &[&str] = &[
    "onnx/model.onnx",
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];

fn missing_weights(dir: &std::path::Path) -> Vec<&'static str> {
    MODEL_FILES
        .iter()
        .copied()
        .filter(|name| !dir.join(name).is_file())
        .collect()
}

/// `HF_HUB_OFFLINE=1` (or `true`) means the fetch must not run.
fn hub_is_offline(raw: Option<&str>) -> bool {
    matches!(
        raw.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1") | Some("true")
    )
}

/// Where the weights are, when a seat has already put them on disk.
///
/// `PACKSET_EMBED_MODEL_PATH` wins. Otherwise `<cache>/user/<seed>/` is used
/// when that directory exists. A directory that exists and is short a file is
/// an error: the fetch runs only when there is no directory. An explicit path
/// that is short a file is an error for the same reason.
fn model_dir_from(
    named: Option<&std::path::Path>,
    cache: Option<&std::path::Path>,
    seed: &str,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    if let Some(dir) = named {
        let missing = missing_weights(dir);
        if !missing.is_empty() {
            anyhow::bail!(
                "PACKSET_EMBED_MODEL_PATH {} is missing {}",
                dir.display(),
                missing.join(", ")
            );
        }
        return Ok(Some(dir.to_path_buf()));
    }
    let Some(cache) = cache else {
        return Ok(None);
    };
    let dir = cache.join("user").join(seed);
    if !dir.exists() {
        return Ok(None);
    }
    let missing = missing_weights(&dir);
    if missing.is_empty() {
        return Ok(Some(dir));
    }
    anyhow::bail!("{} is missing {}", dir.display(), missing.join(", "))
}

fn named_model_path() -> Option<std::path::PathBuf> {
    std::env::var_os("PACKSET_EMBED_MODEL_PATH")
        .filter(|path| !path.is_empty())
        .map(std::path::PathBuf::from)
}

/// Load ONNX Runtime before any session when this build dlopens it.
///
/// `load-dynamic` does not link the runtime. `ORT_DYLIB_PATH` is the shared
/// library, the variable ort reads when nothing has called `init_from` yet.
/// Calling it here turns a missing file into this process's error instead of
/// a panic inside the first session.
fn prepare_runtime() -> anyhow::Result<()> {
    #[cfg(feature = "load-dynamic")]
    {
        let raw = std::env::var("ORT_DYLIB_PATH").unwrap_or_default();
        let path = raw.trim();
        if path.is_empty() {
            anyhow::bail!(
                "this build loads ONNX Runtime at run time; set ORT_DYLIB_PATH to libonnxruntime.so"
            );
        }
        let _ = ort::init_from(path)
            .map_err(|err| anyhow::anyhow!("ONNX Runtime at {path}: {err}"))?
            .commit();
    }
    Ok(())
}

fn load_files(dir: &std::path::Path, pooling: Pooling) -> anyhow::Result<TextEmbedding> {
    let read = |name: &str| {
        std::fs::read(dir.join(name))
            .map_err(|err| anyhow::anyhow!("{name} under {}: {err}", dir.display()))
    };
    let files = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };
    let model =
        UserDefinedEmbeddingModel::new(read("onnx/model.onnx")?, files).with_pooling(pooling);
    Ok(TextEmbedding::try_new_from_user_defined(
        model,
        InitOptionsUserDefined::default(),
    )?)
}

fn load_builtin(model: &EmbeddingModel, seed: &str) -> anyhow::Result<TextEmbedding> {
    if hub_is_offline(std::env::var("HF_HUB_OFFLINE").ok().as_deref()) {
        anyhow::bail!(
            "HF_HUB_OFFLINE is set and {seed} is not on disk. Place onnx/model.onnx, \
             tokenizer.json, config.json, special_tokens_map.json and tokenizer_config.json \
             at PACKSET_EMBED_MODEL_PATH, or under the cache at user/{seed}/"
        );
    }
    let mut options = TextInitOptions::new(model.clone()).with_show_download_progress(false);
    if let Some(dir) = cache_dir() {
        options = options.with_cache_dir(dir);
    }
    Ok(TextEmbedding::try_new(options)?)
}

/// Load the model a choice names.
///
/// A complete seed is the model. The Hub fetch is the path taken when the
/// seed directory is absent and the model is one the runtime knows how to
/// retrieve.
fn load(choice: &Choice) -> anyhow::Result<TextEmbedding> {
    let seeded = model_dir_from(
        named_model_path().as_deref(),
        cache_dir().as_deref(),
        choice.seed,
    )?;
    if let Some(dir) = seeded {
        return load_files(&dir, choice.pooling.clone());
    }
    match &choice.source {
        Source::Builtin(model) => load_builtin(model, choice.seed),
        Source::Files => anyhow::bail!(
            "{} is loaded from files. Set PACKSET_EMBED_MODEL_PATH, or place the hub layout \
             under the cache at user/{}/",
            choice.seed,
            choice.seed
        ),
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

    prepare_runtime()?;

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
    // A batch pads to its longest pair, so one long card among twenty short
    // claims ran the whole batch at 512 tokens: 2.7 s a prompt on a laptop.
    // A claim is a sentence or two; 192 tokens holds it and the question.
    let max_length = std::env::var("PACKSET_RERANK_MAX_LENGTH")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n >= 32)
        .unwrap_or(RERANK_MAX_LENGTH);
    let mut options = RerankInitOptions::new(model)
        .with_show_download_progress(false)
        .with_max_length(max_length);
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

/// Tokens a question and a claim may take together in the cross-encoder;
/// `PACKSET_RERANK_MAX_LENGTH` sets another.
const RERANK_MAX_LENGTH: usize = 192;

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
        PACKSET_EMBED_CACHE        where models live; default $XDG_CACHE_HOME/packset/embed\n\
        PACKSET_EMBED_MODEL        bge-small (default), bge-base, bge-large,\n\
                                   e5-base, e5-large (multilingual), gte-large,\n\
                                   mxbai-large, or e5-large-v2 from files\n\
        PACKSET_EMBED_MODEL_PATH   hub layout of the named model; used as-is\n\
        HF_HUB_OFFLINE             1 refuses the fetch; a seeded directory still loads\n\
        ORT_LIB_PATH               build: directory of a source or system ONNX Runtime\n\
        ORT_LIB_LOCATION           same directory, the older name\n\
        ORT_PREFER_DYNAMIC_LINK    1 links the shared library in that directory\n\
        ORT_DYLIB_PATH             shared library, for --features load-dynamic\n\
        PACKSET_RERANK_MAX_LENGTH  tokens a question and a claim take together (192)\n\
        PACKSET_RERANK_MODEL       bge-reranker-base (default), bge-reranker-v2-m3,\n\
                                   jina-turbo, jina-v2\n\
    \n\
    The default build links ONNX Runtime you provide. --features download-binaries\n\
    fetches the prebuilt runtime from cdn.pyke.io. A model is read from\n\
    PACKSET_EMBED_MODEL_PATH, else from PACKSET_EMBED_CACHE/user/<model>/,\n\
    when those files are present.";

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
    fn a_seeded_model_is_the_directory_and_a_partial_one_is_an_error() {
        let root = std::env::temp_dir().join(format!(
            "packset-embed-seed-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let seed = root.join("user").join("bge-small");
        std::fs::create_dir_all(seed.join("onnx")).unwrap();
        for name in [
            "tokenizer.json",
            "config.json",
            "special_tokens_map.json",
            "tokenizer_config.json",
        ] {
            std::fs::write(seed.join(name), b"{}").unwrap();
        }
        let err = super::model_dir_from(None, Some(&root), "bge-small")
            .expect_err("a short directory is not fetched")
            .to_string();
        assert!(err.contains("onnx/model.onnx"), "{err}");
        std::fs::write(seed.join("onnx").join("model.onnx"), b"onnx").unwrap();
        assert_eq!(
            super::model_dir_from(None, Some(&root), "bge-small").unwrap(),
            Some(seed)
        );
        // No directory: the caller may fetch. A different model is not this one.
        assert_eq!(
            super::model_dir_from(None, Some(&root), "bge-base").unwrap(),
            None
        );
        assert_eq!(
            super::model_dir_from(None, None, "bge-small").unwrap(),
            None
        );

        let named = root.join("named");
        std::fs::create_dir_all(named.join("onnx")).unwrap();
        let err = super::model_dir_from(Some(&named), Some(&root), "bge-small")
            .expect_err("an explicit path short a file is an error")
            .to_string();
        assert!(err.contains("PACKSET_EMBED_MODEL_PATH"), "{err}");
        assert!(err.contains("onnx/model.onnx"), "{err}");
        for name in super::MODEL_FILES {
            let path = named.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, b"x").unwrap();
        }
        assert_eq!(
            super::model_dir_from(Some(&named), Some(&root), "bge-small").unwrap(),
            Some(named)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn offline_is_a_set_flag_and_the_default_model_seeds_as_bge_small() {
        assert!(super::hub_is_offline(Some("1")));
        assert!(super::hub_is_offline(Some("true")));
        assert!(super::hub_is_offline(Some(" TRUE ")));
        assert!(!super::hub_is_offline(None));
        assert!(!super::hub_is_offline(Some("0")));
        let choice = super::choose("").expect("bge-small");
        assert_eq!(choice.seed, "bge-small");
        assert_eq!(choice.pooling, super::Pooling::Cls);
        let english = super::choose("e5-large-v2").expect("files");
        assert_eq!(english.seed, "e5-large-v2");
        assert!(matches!(english.source, super::Source::Files));
        assert_eq!(english.pooling, super::Pooling::Mean);
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
