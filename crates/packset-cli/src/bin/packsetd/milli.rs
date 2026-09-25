//! The search projection, driven as a separate process.
//!
//! The index is a projection and never the store: everything in it is
//! derivable from the atoms, so a corrupt or missing index costs ranking
//! quality and nothing else. Every failure here falls back to the linear
//! scorer rather than answering with a partial index, because a wrong answer
//! that looks complete is worse than a slower one that is.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use packset_core::record;
use serde_json::{json, Value};

use crate::store::Record;

/// Environment variables naming the search binary.
pub const BIN_VARS: &[&str] = &["PACKSET_MILLI", "INSIDE_MILLI", "GROK_INSIDE_MILLI"];

/// Sets already backfilled into an index by this process.
///
/// The backfill exists for a projection written before atoms carried a `set`,
/// and one pass over the set fixes that for good: every write since keeps the
/// field current. Doing it per query instead re-uploads the whole set on every
/// scoped search, which is the difference between a search and an indexing
/// job.
fn backfilled() -> &'static std::sync::Mutex<std::collections::HashSet<(PathBuf, String)>> {
    static SEEN: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<(PathBuf, String)>>,
    > = std::sync::OnceLock::new();
    SEEN.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Forget what was backfilled into `dir`, because the index is being rebuilt.
fn forget_backfill(dir: &Path) {
    if let Ok(mut seen) = backfilled().lock() {
        seen.retain(|(indexed, _)| indexed != dir);
    }
}

/// The search binary, if this seat has one.
///
/// Absent is the normal case, not an error: the linear scorer answers the same
/// question without it.
#[must_use]
pub fn binary() -> Option<PathBuf> {
    for var in BIN_VARS {
        if let Some(raw) = std::env::var_os(var) {
            let path = PathBuf::from(raw);
            if is_executable(&path) {
                return Some(path);
            }
        }
    }
    // The same three places the writer being replaced looks, relative to the
    // tree root. The workspace target directory is deliberately not among
    // them: neither writer searches it, and a seat that builds there names the
    // binary with PACKSET_MILLI.
    let here = std::env::current_exe().ok()?;
    let root = here.parent()?.parent()?.parent()?;
    for candidate in [
        root.join("bin/packset-milli"),
        root.join("crates/packset-milli/target/release/packset-milli"),
        root.join("bin/inside-milli"),
    ] {
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    for name in ["packset-milli", "inside-milli"] {
        if let Some(found) = which(name) {
            return Some(found);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    true
}

fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// Run the binary, or nothing when it is absent or unhappy.
fn run(argv: &[String], stdin: Option<&str>) -> Option<Value> {
    let binary = binary()?;
    let mut child = Command::new(binary)
        .args(argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    if let Some(text) = stdin {
        child.stdin.take()?.write_all(text.as_bytes()).ok()?;
    } else {
        drop(child.stdin.take());
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let value: Value = serde_json::from_str(raw.trim()).ok()?;
    value.is_object().then_some(value)
}

/// The primary key for one document.
///
/// The index accepts `[A-Za-z0-9_-]` only, and a workspace name carries
/// neither, so the workspace card's key is a digest of the name rather than
/// the name.
#[must_use]
pub fn document_id(field: &str, workspace: &str, atom_id: &str) -> String {
    match field {
        "user" => "user".into(),
        "memory" => format!("memory_{}", short_digest(workspace)),
        _ => atom_id.to_string(),
    }
}

/// The first sixteen hex characters of the SHA-256 of `text`.
fn short_digest(text: &str) -> String {
    let digest = sha256(text.as_bytes());
    digest
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// SHA-256, so the workspace card keeps the key the other writer gave it.
fn sha256(message: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let mut data = message.to_vec();
    let bits = (message.len() as u64) * 8;
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bits.to_be_bytes());

    let (blocks, _) = data.as_chunks::<64>();
    for block in blocks {
        let mut w = [0u32; 64];
        let (words, _) = block.as_chunks::<4>();
        for (i, chunk) in words.iter().enumerate() {
            w[i] = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// The document for one atom.
#[must_use]
pub fn atom_document(atom: &Record) -> Value {
    let entities: Vec<String> = atom
        .get("entities")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let id = atom.get("id").and_then(Value::as_str).unwrap_or("");
    json!({
        "id": document_id("atom", "", id),
        "field": "atom",
        "kind": atom.get("kind").and_then(Value::as_str).unwrap_or("atom"),
        "text": atom.get("text").and_then(Value::as_str).unwrap_or(""),
        "entities": entities.join(" "),
        "workspace": atom.get("workspace").and_then(Value::as_str).unwrap_or(""),
        "set": atom.get("set").and_then(Value::as_str).unwrap_or(""),
        "trust": match atom.get("trust") {
            None | Some(Value::Null) => 1.0,
            Some(other) => other.as_f64().unwrap_or(1.0),
        },
    })
}

/// Every document a workspace projects: the two cards and its live atoms.
#[must_use]
pub fn pack_documents(workspace: &str, user: &str, memory: &str, atoms: &[Record]) -> Vec<Value> {
    let mut docs = Vec::new();
    if !user.is_empty() {
        docs.push(json!({
            "id": document_id("user", workspace, ""),
            "field": "user", "kind": "user", "text": user, "entities": "",
            "workspace": workspace, "trust": 1.5,
        }));
    }
    if !memory.is_empty() {
        docs.push(json!({
            "id": document_id("memory", workspace, ""),
            "field": "memory", "kind": "memory", "text": memory, "entities": "",
            "workspace": workspace, "trust": 1.25,
        }));
    }
    let now = packset_core::clock::utcnow();
    for atom in atoms {
        if record::is_live(atom, &now) || record::is_due(atom, &now) {
            docs.push(atom_document(atom));
        }
    }
    docs
}

fn jsonl(docs: &[Value]) -> String {
    let mut out = String::new();
    for doc in docs {
        if let Ok(line) = serde_json::to_string(doc) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// Add or replace documents. An empty batch is a success.
#[must_use]
pub fn upsert(docs: &[Value], index_dir: &Path) -> bool {
    if docs.is_empty() {
        return true;
    }
    let argv = vec![
        "index".into(),
        "--index".into(),
        index_dir.display().to_string(),
    ];
    run(&argv, Some(&jsonl(docs))).is_some()
}

/// Drop documents by id.
#[must_use]
pub fn delete(ids: &[String], index_dir: &Path) -> bool {
    if ids.is_empty() {
        return true;
    }
    let argv = vec![
        "delete".into(),
        "--index".into(),
        index_dir.display().to_string(),
    ];
    let payload = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
    run(&argv, Some(&payload)).is_some()
}

/// The pack a projection is built from or searched against.
///
/// The four travel together everywhere, and separating them is how a set's
/// cards end up written as a workspace's.
#[derive(Debug, Clone, Copy)]
pub struct Corpus<'a> {
    /// The workspace being projected.
    pub workspace: &'a str,
    /// The seat card, or a set's stand-in for it.
    pub user: &'a str,
    /// The workspace card, or a set's stand-in for it.
    pub memory: &'a str,
    /// The live atoms.
    pub atoms: &'a [Record],
}

/// Rebuild the projection from a whole workspace.
///
/// Never called with a set's cards: they would become the workspace's prose
/// documents and every unscoped search would then answer with them.
#[must_use]
pub fn replace(corpus: Corpus<'_>, dir: &Path) -> bool {
    forget_backfill(dir);
    let docs = pack_documents(corpus.workspace, corpus.user, corpus.memory, corpus.atoms);
    let argv = vec![
        "index".into(),
        "--index".into(),
        dir.display().to_string(),
        "--replace".into(),
    ];
    run(&argv, Some(&jsonl(&docs))).is_some()
}

/// Upsert the live atoms only, leaving the prose documents alone.
#[must_use]
pub fn reindex_atoms(atoms: &[Record], dir: &Path) -> bool {
    let now = packset_core::clock::utcnow();
    let docs: Vec<Value> = atoms
        .iter()
        .filter(|a| {
            a.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| !id.is_empty())
                && (record::is_live(a, &now) || record::is_due(a, &now))
        })
        .map(atom_document)
        .collect();
    upsert(&docs, dir)
}

/// Whether the projection exists on disk.
#[must_use]
pub fn index_ready(dir: &Path) -> bool {
    dir.join("data.mdb").exists()
}

/// Keep the atom hits the pack still says are live and in scope.
///
/// The pack decides membership, not the index: a stale projection that
/// predates the `set` field still scopes correctly after this, and a hit for
/// an atom that has since been tombstoned never reaches a reader. Prose hits
/// from the index are dropped because prose always comes from the pack.
#[must_use]
pub fn filter_atom_hits(hits: &[Value], live: &[Record], set: Option<&str>) -> Vec<Value> {
    let by_id: BTreeMap<&str, &Record> = live
        .iter()
        .filter_map(|a| a.get("id").and_then(Value::as_str).map(|id| (id, a)))
        .collect();
    hits.iter()
        .filter(|hit| !matches!(hit["field"].as_str(), Some("user" | "memory")))
        .filter_map(|hit| {
            let id = hit["id"].as_str()?;
            let atom = by_id.get(id)?;
            if let Some(name) = set {
                if atom.get("set").and_then(Value::as_str) != Some(name) {
                    return None;
                }
            }
            // The index stores what it scores; the stamp, the kind and the
            // review date come from the pack's own record.
            let mut hit = hit.clone();
            for key in ["ts", "kind", "due_at"] {
                if hit.get(key).is_none_or(Value::is_null) {
                    if let Some(value) = atom.get(key) {
                        hit[key] = value.clone();
                    }
                }
            }
            Some(hit)
        })
        .collect()
}

/// What the index needs before this query can be trusted.
fn ensure_atoms(corpus: Corpus<'_>, dir: &Path, set: Option<&str>) -> bool {
    let atoms = corpus.atoms;
    if !index_ready(dir) {
        // A set-scoped call must never full-replace: its cards are not the
        // workspace's, and writing them as such poisons every other search.
        if set.is_some() {
            return reindex_atoms(atoms, dir);
        }
        return replace(corpus, dir);
    }
    match set {
        // Backfill the named set's atoms once, so `--set` sees the field even
        // on a projection written before it existed. Once is enough: every
        // write since keeps the field current, and repeating it per query
        // turns a scoped search into an indexing job.
        Some(name) => {
            let key = (dir.to_path_buf(), name.to_string());
            if backfilled().lock().is_ok_and(|seen| seen.contains(&key)) {
                return true;
            }
            let now = packset_core::clock::utcnow();
            let docs: Vec<Value> = atoms
                .iter()
                .filter(|a| {
                    a.get("set").and_then(Value::as_str) == Some(name)
                        && (record::is_live(a, &now) || record::is_due(a, &now))
                })
                .map(atom_document)
                .collect();
            let done = upsert(&docs, dir);
            if done {
                if let Ok(mut seen) = backfilled().lock() {
                    seen.insert(key);
                }
            }
            done
        }
        None => true,
    }
}

/// One search against the projection, or nothing when it cannot answer.
#[must_use]
pub fn search(
    corpus: Corpus<'_>,
    query: &str,
    limit: usize,
    dir: &Path,
    set: Option<&str>,
) -> Option<Vec<Value>> {
    let (workspace, atoms) = (corpus.workspace, corpus.atoms);
    binary()?;
    if !ensure_atoms(corpus, dir, set) {
        return None;
    }
    let once = |q: &str| -> Option<Vec<Value>> {
        let mut argv = vec![
            "search".into(),
            "--index".into(),
            dir.display().to_string(),
            "--q".into(),
            q.to_string(),
            "--limit".into(),
            limit.to_string(),
        ];
        if !workspace.is_empty() {
            argv.push("--workspace".into());
            argv.push(workspace.to_string());
        }
        if let Some(name) = set {
            argv.push("--set".into());
            argv.push(name.to_string());
        }
        let payload = run(&argv, None)?;
        let raw = payload.get("hits")?.as_array()?;
        let hits: Vec<Value> = raw.iter().filter(|h| h.is_object()).cloned().collect();
        Some(filter_atom_hits(&hits, atoms, set))
    };

    let mut atom_hits = once(query)?;
    if atom_hits.is_empty() && !query.trim().is_empty() {
        // Nothing found may mean the projection is behind rather than that the
        // pack has nothing. Reindex the atoms once and ask again; a failed
        // reindex means the index is not authoritative, and the caller falls
        // back to the linear scorer rather than reporting a prose-only miss.
        if !reindex_atoms(atoms, dir) {
            return None;
        }
        atom_hits = once(query)?;
    }
    Some(atom_hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(value: Value) -> Record {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn an_index_hit_carries_the_records_stamp() {
        let live = vec![record(json!({
            "id": "a1", "kind": "lesson", "text": "one",
            "ts": "2026-09-01T00:00:00.000Z", "due_at": "2026-09-20T00:00:00.000Z"
        }))];
        let hits = vec![
            json!({"field": "atom", "id": "a1", "text": "one", "score": 1.0}),
            json!({"field": "atom", "id": "gone", "text": "two", "score": 0.5}),
            json!({"field": "user", "id": null, "text": "card", "score": 0.4}),
        ];
        let kept = filter_atom_hits(&hits, &live, None);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0]["ts"], json!("2026-09-01T00:00:00.000Z"));
        assert_eq!(kept[0]["kind"], json!("lesson"));
        assert_eq!(kept[0]["due_at"], json!("2026-09-20T00:00:00.000Z"));
        assert_eq!(kept[0]["score"], json!(1.0));
    }

    #[test]
    fn the_digest_is_the_one_the_other_writer_computes() {
        // hashlib.sha256(b"").hexdigest()[:16]
        assert_eq!(short_digest(""), "e3b0c44298fc1c14");
        // hashlib.sha256(b"abc").hexdigest()[:16]
        assert_eq!(short_digest("abc"), "ba7816bf8f01cfea");
    }

    #[test]
    fn sha256_matches_the_known_vectors() {
        let empty = sha256(b"");
        assert_eq!(empty[0], 0xe3);
        assert_eq!(empty[31], 0x55);
        let abc = sha256(b"abc");
        assert_eq!(abc[0], 0xba);
        assert_eq!(abc[31], 0xad);
        // A message spanning two blocks exercises the padding.
        let long = sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(long[0], 0x24);
        assert_eq!(long[31], 0xc1);
        assert_eq!(
            short_digest("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8"
        );
    }

    #[test]
    fn a_workspace_card_keys_on_a_digest_not_a_name() {
        // The index accepts [A-Za-z0-9_-] and a workspace name carries neither
        // a colon nor a slash safely.
        let key = document_id("memory", "git:github.com/HaoZeke/vissue", "");
        assert!(key.starts_with("memory_"), "{key}");
        assert!(
            key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "{key}"
        );
        assert_eq!(document_id("user", "anything", ""), "user");
        assert_eq!(document_id("atom", "w", "abc123"), "abc123");
    }

    #[test]
    fn an_atom_document_flattens_the_entities() {
        let doc = atom_document(&record(json!({
            "id": "a", "kind": "voice", "text": "x",
            "entities": ["one", "two"], "workspace": "w", "trust": 2.0
        })));
        assert_eq!(doc["entities"], json!("one two"));
        assert_eq!(doc["trust"], json!(2.0));
        assert_eq!(doc["set"], json!(""), "absent is empty, not missing");
    }

    #[test]
    fn a_pack_projects_its_cards_only_when_they_say_something() {
        let docs = pack_documents("w", "", "", &[]);
        assert!(docs.is_empty(), "{docs:?}");
        let docs = pack_documents("w", "seat card", "workspace card", &[]);
        assert_eq!(docs.len(), 2);
        assert!(docs[0]["trust"].as_f64().unwrap() > docs[1]["trust"].as_f64().unwrap());
    }

    #[test]
    fn an_expired_atom_is_not_projected() {
        let atoms = vec![
            record(json!({"id": "live", "text": "a", "workspace": "w"})),
            record(json!({
                "id": "gone", "text": "b", "workspace": "w",
                "valid_to": "2000-01-01T00:00:00.000Z"
            })),
        ];
        let docs = pack_documents("w", "", "", &atoms);
        let ids: Vec<&str> = docs.iter().filter_map(|d| d["id"].as_str()).collect();
        assert_eq!(ids, vec!["live"], "{docs:?}");
    }

    #[test]
    fn the_pack_decides_membership_and_not_the_index() {
        // A stale projection may return an atom the pack has since dropped, or
        // one written before the set field existed.
        let live = vec![record(json!({"id": "kept", "text": "a", "set": "review"}))];
        let hits = vec![
            json!({"field": "atom", "id": "kept", "text": "a"}),
            json!({"field": "atom", "id": "vanished", "text": "b"}),
            json!({"field": "user", "id": "user", "text": "prose"}),
        ];
        let filtered = filter_atom_hits(&hits, &live, Some("review"));
        let ids: Vec<&str> = filtered.iter().filter_map(|h| h["id"].as_str()).collect();
        assert_eq!(ids, vec!["kept"], "{filtered:?}");

        // And out of scope means out, whatever the index said.
        assert!(filter_atom_hits(&hits, &live, Some("other")).is_empty());
    }

    #[test]
    fn prose_from_the_index_is_always_dropped() {
        let live = vec![record(json!({"id": "a", "text": "x"}))];
        let hits = vec![
            json!({"field": "user", "id": "user", "text": "p"}),
            json!({"field": "memory", "id": "memory_x", "text": "q"}),
        ];
        assert!(
            filter_atom_hits(&hits, &live, None).is_empty(),
            "prose comes from the pack, so the index copy can be stale"
        );
    }

    #[test]
    fn an_empty_batch_is_a_success_without_running_anything() {
        let dir = tempfile::tempdir().unwrap();
        assert!(upsert(&[], dir.path()));
        assert!(delete(&[], dir.path()));
    }

    #[test]
    fn a_missing_index_directory_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!index_ready(&dir.path().join("nothing")));
        std::fs::write(dir.path().join("data.mdb"), b"x").unwrap();
        assert!(index_ready(dir.path()));
    }
}
