//! `ts` is when a claim was first written; `updated` is its last write.
//!
//! A real `packsetd` on a free loopback port over a temporary home. A
//! grade, a fire, a new text, a repeat and a replacement each rewrite the
//! record. Read back, `ts` has not moved, and a recency-ranked search
//! still puts the claim written first below the one written after it.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use packset_core::clock;
use serde_json::{json, Value};

const DAY_S: i64 = 86_400;
/// Two first writes eight days apart, both inside the fourteen-day recency
/// half-life, so the recency term alone tells them apart.
const OLDER_DAYS: i64 = 10;
const NEWER_DAYS: i64 = 2;
/// Stamps are to the millisecond. A pause past one keeps two writes'
/// `updated` apart, so a rewrite that did not stamp it is seen.
const STAMP_GAP: Duration = Duration::from_millis(2);

struct Writer {
    child: Child,
    url: String,
    _home: tempfile::TempDir,
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn writer() -> Writer {
    let home = tempfile::tempdir().unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = Command::new(env!("CARGO_BIN_EXE_packsetd"))
        .args(["--port", &port.to_string(), "--home"])
        .arg(home.path().join("pack"))
        .env("HOME", home.path())
        .env("PACKSET_AUTH", "off")
        .env_remove("PACKSET_EMBED")
        .env_remove("PACKSET_MILLI")
        .env_remove("PACKSET_HOME")
        .env_remove("PACKSET_URL")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ureq::get(&format!("{url}/health")).call().is_ok() {
            return Writer {
                child,
                url,
                _home: home,
            };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("packsetd did not answer on {url}");
}

impl Writer {
    fn post(&self, path: &str, body: Value) -> Value {
        std::thread::sleep(STAMP_GAP);
        ureq::post(&format!("{}{path}", self.url))
            .send_json(body)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
            .into_json()
            .unwrap()
    }

    fn note(&self, issue: &str, text: &str, ts: Option<&str>) -> Value {
        let mut body = json!({
            "workspace": "w", "kind": "note", "issue": issue,
            "origin": "user-declared", "level": "explicit", "text": text,
        });
        if let Some(ts) = ts {
            body["ts"] = json!(ts);
        }
        self.post("/v1/atoms", body)
    }

    fn read(&self, id: &str, as_of: Option<&str>) -> Value {
        let mut get = ureq::get(&format!("{}/v1/atoms/{id}", self.url)).query("workspace", "w");
        if let Some(at) = as_of {
            get = get.query("as_of", at);
        }
        get.call()
            .unwrap_or_else(|e| panic!("read {id}: {e}"))
            .into_json()
            .unwrap()
    }
}

fn days_ago(days: i64) -> String {
    clock::shift(&clock::utcnow(), -days * DAY_S).unwrap()
}

fn id_of(atom: &Value) -> String {
    atom["id"].as_str().unwrap().to_string()
}

fn millis(atom: &Value, key: &str) -> i64 {
    clock::parse_millis(
        atom[key]
            .as_str()
            .unwrap_or_else(|| panic!("no {key}: {atom}")),
    )
    .unwrap()
}

#[test]
fn no_rewrite_moves_ts_and_each_moves_updated() {
    let w = writer();
    let written = days_ago(OLDER_DAYS);
    let id = id_of(&w.note(
        "ljos-a",
        "The deploy gate runs the staging suite on Friday.",
        Some(&written),
    ));
    let other = id_of(&w.note("ljos-c", "Fuse weights live in the panel file.", None));
    let first = w.read(&id, None);
    assert_eq!(first["ts"], json!(written));
    let mut last = millis(&first, "updated");
    let mut step = |name: &str, atom: &Value| {
        assert_eq!(atom["ts"], json!(written), "{name} moved ts: {atom}");
        let now = millis(atom, "updated");
        assert!(now > last, "{name} did not move updated: {atom}");
        last = now;
    };

    w.post("/v1/grade", json!({"workspace": "w", "id": id}));
    let graded = w.read(&id, None);
    step("a grade", &graded);
    assert!(
        millis(&graded["review"], "last") > millis(&first["review"], "last"),
        "the review clock counts from the grade: {graded}"
    );

    let fired = w.post("/v1/fire", json!({"workspace": "w", "ids": [id, other]}));
    assert_eq!(fired["held"], json!(false), "{fired}");
    let after_fire = w.read(&id, None);
    step("a fire", &after_fire);
    assert_eq!(
        after_fire["review"]["last"], graded["review"]["last"],
        "a fire is not a review: {after_fire}"
    );

    let text = "The deploy gate runs the staging suite on Monday.";
    w.post(
        "/v1/atoms/update",
        json!({"workspace": "w", "id": id, "fields": {"text": text}}),
    );
    let updated = w.read(&id, None);
    assert_eq!(updated["text"], json!(text));
    step("a text update", &updated);

    let repeat = w.note("ljos-a", text, None);
    assert_eq!(repeat["id"], json!(id), "a repeat is the stored claim");
    assert_eq!(repeat["ts"], json!(written), "a repeat moved ts: {repeat}");

    let live_at = updated["updated"].as_str().unwrap().to_string();
    let replacement = w.post(
        "/v1/atoms",
        json!({
            "workspace": "w", "kind": "note", "issue": "ljos-a",
            "origin": "user-declared", "level": "explicit", "supersedes": [id],
            "text": "The deploy gate runs the staging suite every weekday.",
        }),
    );
    assert_eq!(replacement["supersedes"], json!([id]), "{replacement}");
    let replaced = w.read(&id, Some(&live_at));
    assert!(replaced["valid_to"].is_string(), "{replaced}");
    step("a replacement", &replaced);
}

#[test]
fn a_recency_ranked_search_orders_rewritten_claims_by_first_write() {
    let w = writer();
    let text = "The deploy gate runs the staging suite on Monday.";
    let older = id_of(&w.note(
        "ljos-a",
        "The deploy gate runs the staging suite on Friday.",
        Some(&days_ago(OLDER_DAYS)),
    ));
    let newer = id_of(&w.note("ljos-b", text, Some(&days_ago(NEWER_DAYS))));
    let other = id_of(&w.note("ljos-c", "Fuse weights live in the panel file.", None));

    // The older claim is rewritten last, and ends with the newer one's
    // words, so only the time each was first written can order them.
    w.post("/v1/grade", json!({"workspace": "w", "id": older}));
    w.post("/v1/fire", json!({"workspace": "w", "ids": [older, other]}));
    w.post(
        "/v1/atoms/update",
        json!({"workspace": "w", "id": older, "fields": {"text": text}}),
    );

    let found: Value = ureq::get(&format!("{}/v1/search", w.url))
        .query("workspace", "w")
        .query("q", "deploy gate staging suite")
        .query("rank", "fused")
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    let order: Vec<&str> = found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|hit| hit["id"].as_str())
        .filter(|id| *id == older || *id == newer)
        .collect();
    assert_eq!(order, [newer.as_str(), older.as_str()], "{found}");
}
