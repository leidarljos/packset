//! `packset`: start, stop and inspect the pack writer.
//!
//! The daemon owns the store and every client speaks HTTP to it, so a seat
//! needs one command that answers "is it up, and at what URL". Everything here
//! is either that question or a thin read of `/v1`.
//!
//! ```console
//! packset ensure | start | stop | status | port | url | which
//! packset remember [--workspace WS] TEXT...
//! packset prefer [--workspace WS] TEXT...
//! packset search [--workspace WS] QUERY...
//! packset due [WORKSPACE]
//! packset islands [WORKSPACE]
//! packset island [--workspace WS] [--fire] CUE
//! packset fire [--workspace WS] ID ID...
//! packset grade ID [--lapsed] [WORKSPACE]
//! packset pin [NAME]
//! packset accessions [WORKSPACE]
//! packset atoms [--as-of TS] [WORKSPACE]
//! packset citers ACCESSION [WORKSPACE]
//! ```
//!
//! Clients export `PACKSET_URL`; `INSIDE_MEMORY_URL` is an alias.

mod procfs;

use std::env;
use std::fs::{self, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use packset_client::PacksetClient;

/// How long `start` waits for the daemon to bind.
const STARTUP: Duration = Duration::from_secs(5);

/// What `/health` says when the answer came from our own writer.
const OURS: &[&str] = &["packsetd", "inside-memd"];

fn main() -> std::process::ExitCode {
    // A closed pipe ends the run quietly, so `packset status | head` is not a panic.
    // SAFETY: resetting a signal disposition before any thread is spawned.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("packset: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let (verb, rest) = args
        .split_first()
        .map_or(("ensure", &[][..]), |(head, tail)| (head.as_str(), tail));
    let port = port();

    match verb {
        "ensure" => {
            if procfs::listening(port) && !is_ours(port) {
                anyhow::bail!("port {port} is held by something else; set PACKSET_PORT");
            }
            if !procfs::listening(port) {
                start(port)?;
            }
            print_url(port);
            Ok(())
        }
        "start" => start(port),
        "stop" => stop(port),
        "status" => status(port, rest.first().map(String::as_str)),
        "port" => {
            println!("{port}");
            Ok(())
        }
        "url" => {
            print_url(port);
            Ok(())
        }
        "which" => {
            let daemon = resolve_daemon()
                .ok_or_else(|| anyhow::anyhow!("no packsetd binary; cargo build --release"))?;
            println!("{}", daemon.display());
            Ok(())
        }
        "remember" => write(port, "lesson", rest),
        "prefer" => write(port, "preference", rest),
        "search" => search(port, rest),
        "due" => due(port, rest.first().map(String::as_str)),
        "islands" => islands(port, rest.first().map(String::as_str)),
        "hubs" => hubs(port, rest.first().map(String::as_str)),
        "island" => island(port, rest),
        "fire" => fire(port, rest),
        "grade" => grade(port, rest),
        "pin" => pin(port, rest.first().map(String::as_str)),
        "accessions" => accessions(port, rest.first().map(String::as_str)),
        "atoms" => atoms(port, rest),
        "export" => export(port, rest),
        "citers" => citers(
            port,
            rest.first().map(String::as_str),
            rest.get(1).map(String::as_str),
        ),
        "-h" | "--help" | "help" => {
            println!("{}", usage());
            Ok(())
        }
        "-V" | "--version" => {
            println!(
                "packset {} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("PACKSET_COMMIT")
            );
            Ok(())
        }
        other => anyhow::bail!("unknown command: {other}\n\n{}", usage()),
    }
}

fn usage() -> String {
    "packset: start, stop and inspect the pack writer\n\
     \n\
         ensure                 start if down, then print the URL\n\
         start | stop\n\
         status [WORKSPACE]     counts by kind, pin, index\n\
         port | url | which\n\
         remember [--workspace WS] TEXT   one lesson, two sentences at most\n\
         prefer [--workspace WS] TEXT     one standing preference\n\
         search [--workspace WS] QUERY    ranked claims, score kind id text\n\
         due [WORKSPACE]        claims whose review clock has run out\n\
         islands [WORKSPACE]    the link graph's clusters, largest first\n\
         hubs [WORKSPACE]       the claims the link graph turns on, highest first\n\
         island [--workspace WS] [--fire] CUE   the memories a cue activates\n\
         fire [--workspace WS] ID ID...   these claims fired together; their links gain weight\n\
         grade ID [--lapsed] [WS]  mark a review recalled, or lapsed\n\
         pin [NAME]             read, or set, the pinned set\n\
         accessions [WORKSPACE] deed accessions live atoms cite\n\
         atoms [--as-of TS] [WS] live-now atoms, or those live at TS\n\
         citers ACCESSION [WS]  the live atoms citing one accession\n\
         export --into DIR [WS] atoms to a satchel; cited accessions to stdout"
        .to_string()
}

/// The port this seat uses.
fn port() -> u16 {
    packset_client::default_port()
}

/// Where the daemon's own output goes.
fn log_path() -> PathBuf {
    if let Some(named) = env::var_os("PACKSET_LOG") {
        return PathBuf::from(named);
    }
    let state = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))
        .unwrap_or_else(|| PathBuf::from("."));
    // One log a port, so two writers on one host do not interleave and
    // a failure's tail is the failing writer's.
    state.join(format!("packsetd-{}.log", port()))
}

fn client(port: u16) -> PacksetClient {
    PacksetClient::new(format!("http://127.0.0.1:{port}"))
}

/// Whether the writer on this port is one of ours.
fn is_ours(port: u16) -> bool {
    client(port)
        .health()
        .is_ok_and(|body| OURS.iter().any(|name| body.trim_start().starts_with(name)))
}

/// The workspace a command was given, or the one the environment names.
/// The workspace a verb acts on: the argument, else `PACKSET_WORKSPACE`, else
/// what the client derives from the working directory's git remote, else
/// `default`. The same answer every other client gives.
fn workspace(given: Option<&str>) -> anyhow::Result<String> {
    Ok(given
        .map(str::to_string)
        .or_else(|| env::var("PACKSET_WORKSPACE").ok())
        .filter(|w| !w.is_empty())
        .unwrap_or_else(|| client(port()).workspace()))
}

/// The daemon this seat would run.
///
/// Next to this binary first, because `cargo build` puts the pair in one
/// directory and a checkout should not consult `PATH` to find its own build.
fn resolve_daemon() -> Option<PathBuf> {
    if let Some(named) = env::var_os("PACKSET_BIN") {
        let path = PathBuf::from(named);
        if procfs::executable(&path) {
            return Some(path);
        }
    }
    if let Some(beside) = env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("packsetd")))
        .filter(|path| procfs::executable(path))
    {
        return Some(beside);
    }
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join("packsetd"))
        .find(|path| procfs::executable(path))
}

/// Start the daemon and wait for it to bind.
fn start(port: u16) -> anyhow::Result<()> {
    if procfs::listening(port) {
        if is_ours(port) {
            eprintln!("packset: already listening on 127.0.0.1:{port}");
            return Ok(());
        }
        anyhow::bail!("port {port} is held by something else; set PACKSET_PORT");
    }
    let daemon = resolve_daemon().ok_or_else(|| {
        anyhow::anyhow!("no packsetd binary; cargo build --release -p packset-daemon")
    })?;
    let log = log_path();
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir)?;
    }
    let out = OpenOptions::new().create(true).append(true).open(&log)?;
    let errs = out.try_clone()?;

    let mut command = Command::new(&daemon);
    command
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(errs));
    // A new session, so closing the terminal that ran `packset ensure` does not
    // take the writer with it.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;

    let deadline = Instant::now() + STARTUP;
    while Instant::now() < deadline {
        if procfs::listening(port) {
            eprintln!("packset: listening on 127.0.0.1:{port}");
            return Ok(());
        }
        if let Ok(Some(code)) = child.try_wait() {
            anyhow::bail!(
                "packsetd exited {code}; see {}{}",
                log.display(),
                tail(&log)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("did not come up; see {}{}", log.display(), tail(&log))
}

/// The last few log lines, for an error that would otherwise say only a path.
fn tail(log: &Path) -> String {
    let Ok(text) = fs::read_to_string(log) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().rev().take(3).collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n");
    for line in lines.into_iter().rev() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn stop(port: u16) -> anyhow::Result<()> {
    if !is_ours(port) {
        eprintln!("packset: nothing of ours to stop on {port}");
        return Ok(());
    }
    let Some(pid) = procfs::pid_on_port(port) else {
        eprintln!("packset: listening on {port} but the holder is not visible");
        return Ok(());
    };
    procfs::terminate(pid)?;
    eprintln!("packset: stopped {pid}");
    Ok(())
}

fn print_url(port: u16) {
    println!("PACKSET_URL=http://127.0.0.1:{port}");
    println!("INSIDE_MEMORY_URL=http://127.0.0.1:{port}");
}

fn status(port: u16, given: Option<&str>) -> anyhow::Result<()> {
    if !procfs::listening(port) {
        anyhow::bail!("down");
    }
    if !is_ours(port) {
        anyhow::bail!("port {port} is held by another process");
    }
    let client = client(port);
    let health = client.health().unwrap_or_default();
    println!("packset: up on 127.0.0.1:{port} ({})", health.trim());
    let scope = given
        .map(str::to_string)
        .or_else(|| env::var("PACKSET_WORKSPACE").ok())
        .filter(|w| !w.is_empty());
    let detail = client.status(scope.as_deref())?;
    println!("{}", serde_json::to_string_pretty(&detail)?);
    Ok(())
}

fn pin(port: u16, name: Option<&str>) -> anyhow::Result<()> {
    let client = client(port);
    let workspace = workspace(None)?;
    let answer = match name {
        Some(name) => client.set_pin(&workspace, name)?,
        None => client.pin(&workspace)?,
    };
    println!("{}", serde_json::to_string(&answer)?);
    Ok(())
}

/// Every deed accession cited by a live atom, one per line.
///
/// The line-per-accession shape is the point: `deedar evidence -` and `deedar
/// current -` read a list on stdin, so a pack answers the staleness question
/// the same way a tracker does.
/// The live atoms citing one accession, one per line: id, then the claim.
///
/// The other direction of `accessions`, and the pack's half of the backwards
/// walk. A tracker answers which issues cite a product; this answers which
/// remembered claims do.
fn citers(port: u16, accession: Option<&str>, given: Option<&str>) -> anyhow::Result<()> {
    let accession =
        accession.ok_or_else(|| anyhow::anyhow!("name an accession: packset citers ACCESSION"))?;
    let workspace = workspace(given)?;
    for atom in client(port).citers(&workspace, accession)? {
        let id = atom
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let text = atom
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        println!("{id}\t{text}");
    }
    Ok(())
}

/// Write a workspace's live atoms into a satchel, and name what they cite.
///
/// What a seat learned is the third thing a handover carries, beside the work
/// and what the work produced. The atoms go in as one JSON object a line,
/// which is what every other export in this stack is, and the accessions they
/// cite go to stdout so a deed store can be handed them on a pipe:
///
///   packset export --into bag/data/atoms | deedar export --into bag/data/deeds -
///
/// Two streams because they have two destinations. Writing the accessions into
/// the satchel would make this the thing that decides what a satchel needs,
/// and that is the tracker's call: the pack only knows what its own atoms
/// mention.
fn export(port: u16, args: &[String]) -> anyhow::Result<()> {
    let mut into: Option<std::path::PathBuf> = None;
    let mut given: Option<String> = None;
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "--into" => {
                at += 1;
                into =
                    Some(std::path::PathBuf::from(args.get(at).ok_or_else(|| {
                        anyhow::anyhow!("--into needs a directory")
                    })?));
            }
            other => given = Some(other.to_string()),
        }
        at += 1;
    }
    let into = into.ok_or_else(|| anyhow::anyhow!("export needs --into DIR"))?;
    let workspace = workspace(given.as_deref())?;
    let held = client(port);
    let atoms = held.atoms(&workspace)?;
    // The accessions come from the endpoint that already answers this, rather
    // than from a copy of the rule for what an accession looks like. Two
    // places deciding that is two places to change it.
    let cited = held.accessions(&workspace)?;

    std::fs::create_dir_all(&into)?;
    let mut lines = String::new();
    for atom in &atoms {
        lines.push_str(&serde_json::to_string(atom)?);
        lines.push('\n');
    }
    let path = into.join(export_file_name(&workspace));
    std::fs::write(&path, lines)?;
    eprintln!("{} atoms to {}", atoms.len(), path.display());
    for accession in cited {
        println!("{accession}");
    }
    Ok(())
}

/// `--workspace WS` pulled out of an argument list; the rest is the text.
fn split_workspace(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut workspace = None;
    let mut rest = Vec::new();
    let mut at = 0;
    while at < args.len() {
        if args[at] == "--workspace" {
            workspace = args.get(at + 1).cloned();
            at += 2;
            continue;
        }
        rest.push(args[at].clone());
        at += 1;
    }
    (workspace, rest)
}

/// POST one explicit claim of `kind`; the text is stored as given.
fn write(port: u16, kind: &str, args: &[String]) -> anyhow::Result<()> {
    let (given, words) = split_workspace(args);
    let text = words.join(" ").trim().to_string();
    if text.is_empty() {
        anyhow::bail!("{kind}: the text is the claim; pass it");
    }
    let workspace = workspace(given.as_deref())?;
    let atom = serde_json::json!({
        "schema": "inside.atom/v1",
        "kind": kind,
        "level": "explicit",
        "text": text,
        "workspace": workspace,
    });
    let stored = client(port).post_atom(&atom)?;
    println!(
        "{}\t{}\tdue {}",
        stored["id"].as_str().unwrap_or("-"),
        kind,
        stored["due_at"].as_str().unwrap_or("-")
    );
    Ok(())
}

/// Ranked claims for a question: score, kind, id, text.
fn search(port: u16, args: &[String]) -> anyhow::Result<()> {
    let (given, words) = split_workspace(args);
    let query = words.join(" ").trim().to_string();
    if query.is_empty() {
        anyhow::bail!("search: pass a question");
    }
    let workspace = workspace(given.as_deref())?;
    for hit in client(port).search(&workspace, &query, 10)? {
        println!(
            "{:.4}\t{}\t{}\t{}",
            hit.score,
            hit.kind,
            hit.id.as_deref().unwrap_or("-"),
            hit.text
        );
    }
    Ok(())
}

/// Live claims whose `due_at` has passed, soonest first: due, id, text.
fn due(port: u16, given: Option<&str>) -> anyhow::Result<()> {
    let workspace = workspace(given)?;
    let now = packset_core::clock::utcnow();
    let mut atoms: Vec<serde_json::Value> = client(port)
        .atoms_as_of(&workspace, None)?
        .into_iter()
        .filter(|a| {
            a["due_at"]
                .as_str()
                .is_some_and(|d| !d.is_empty() && d <= now.as_str())
        })
        .collect();
    atoms.sort_by(|a, b| a["due_at"].as_str().cmp(&b["due_at"].as_str()));
    for atom in atoms {
        println!(
            "{}\t{}\t{}",
            atom["due_at"].as_str().unwrap_or(""),
            atom["id"].as_str().unwrap_or("-"),
            atom["text"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

/// One line per island: size, then the first claim in it.
/// One line per hub: score, links, id, text.
fn hubs(port: u16, given: Option<&str>) -> anyhow::Result<()> {
    let workspace = given.map_or_else(|| client(port).workspace(), str::to_string);
    let body = client(port).hubs(&workspace, 10)?;
    for hub in body["hubs"].as_array().into_iter().flatten() {
        println!(
            "{:.4}\t{}\t{}\t{}",
            hub["score"].as_f64().unwrap_or(0.0),
            hub["links"].as_u64().unwrap_or(0),
            hub["id"].as_str().unwrap_or("-"),
            hub["text"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

fn islands(port: u16, given: Option<&str>) -> anyhow::Result<()> {
    let workspace = workspace(given)?;
    let body = client(port).islands(&workspace)?;
    for island in body["islands"].as_array().into_iter().flatten() {
        let first = island["atoms"][0]["text"].as_str().unwrap_or("");
        println!("{}\t{}", island["size"], first);
    }
    Ok(())
}

/// Two or more claims fired together.
fn fire(port: u16, args: &[String]) -> anyhow::Result<()> {
    let (given, ids) = split_workspace(args);
    if ids.len() < 2 {
        anyhow::bail!("fire: pass two or more claim ids that fired together");
    }
    let workspace = workspace(given.as_deref())?;
    let body = client(port).fire(&workspace, &ids)?;
    println!("{} fired, {} changed", body["fired"], body["changed"]);
    Ok(())
}

/// The memories a cue activates: activation, seed mark, id, text.
fn island(port: u16, args: &[String]) -> anyhow::Result<()> {
    let (given, words) = split_workspace(args);
    let firing = words.iter().any(|w| w == "--fire");
    let cue = words
        .iter()
        .filter(|w| *w != "--fire")
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    if cue.is_empty() {
        anyhow::bail!("island: pass the cue, the task or question at hand");
    }
    let workspace = workspace(given.as_deref())?;
    let body = client(port).activate(&workspace, &cue, 24, firing)?;
    for atom in body["island"].as_array().into_iter().flatten() {
        println!(
            "{:.3}\t{}\t{}\t{}",
            atom["activation"].as_f64().unwrap_or(0.0),
            if atom["seed"].as_bool().unwrap_or(false) {
                "seed"
            } else {
                "    "
            },
            atom["id"].as_str().unwrap_or("-"),
            atom["text"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

/// Grade one review: recalled unless `--lapsed`.
fn grade(port: u16, args: &[String]) -> anyhow::Result<()> {
    let lapsed = args.iter().any(|a| a == "--lapsed");
    let mut rest: Vec<&String> = args.iter().filter(|a| *a != "--lapsed").collect();
    let id = rest
        .first()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("grade needs an atom id"))?;
    rest.remove(0);
    let workspace = workspace(rest.first().map(|s| s.as_str()))?;
    let graded = client(port).grade(&workspace, &id, !lapsed)?;
    println!("{}", graded["due_at"].as_str().unwrap_or("graded"));
    Ok(())
}

/// `<workspace>.jsonl` with the path separators a git-remote workspace name
/// carries folded to `_`.
fn export_file_name(workspace: &str) -> String {
    let flat: String = workspace
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '_'
            } else {
                c
            }
        })
        .collect();
    format!("{flat}.jsonl")
}

/// Live-now atoms, or those live at `--as-of`, one JSON object a line.
fn atoms(port: u16, args: &[String]) -> anyhow::Result<()> {
    let mut as_of: Option<String> = None;
    let mut given: Option<String> = None;
    let mut at = 0;
    while at < args.len() {
        match args[at].as_str() {
            "--as-of" => {
                at += 1;
                as_of = Some(
                    args.get(at)
                        .ok_or_else(|| anyhow::anyhow!("--as-of needs a timestamp"))?
                        .to_string(),
                );
            }
            other => given = Some(other.to_string()),
        }
        at += 1;
    }
    let workspace = workspace(given.as_deref())?;
    for atom in client(port).atoms_as_of(&workspace, as_of.as_deref())? {
        println!("{}", serde_json::to_string(&atom)?);
    }
    Ok(())
}

fn accessions(port: u16, given: Option<&str>) -> anyhow::Result<()> {
    let workspace = workspace(given)?;
    for accession in client(port).accessions(&workspace)? {
        println!("{accession}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_workspace_flag_leaves_the_text() {
        let args: Vec<String> = ["--workspace", "seat", "the", "claim"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (ws, rest) = super::split_workspace(&args);
        assert_eq!(ws.as_deref(), Some("seat"));
        assert_eq!(rest, ["the", "claim"]);
        let (ws, rest) = super::split_workspace(&["only".to_string()]);
        assert!(ws.is_none());
        assert_eq!(rest, ["only"]);
    }

    #[test]
    fn a_workspace_name_is_one_file_name() {
        assert_eq!(super::export_file_name("seat"), "seat.jsonl");
        assert_eq!(
            super::export_file_name("git:github.com/leidarljos/ljos"),
            "git_github.com_leidarljos_ljos.jsonl"
        );
    }
}
