//! `packset`: start, stop and inspect the pack writer.
//!
//! The daemon owns the store and every client speaks HTTP to it, so a seat
//! needs one command that answers "is it up, and at what URL". Everything here
//! is either that question or a thin read of `/v1`.
//!
//! ```console
//! packset ensure [--owned] | start [--owned] | stop | status | port | url | which
//! packset remember [--workspace WS] TEXT...
//! packset prefer [--workspace WS] TEXT...
//! packset search [--workspace WS] [--as-of TS] [--rerank] [--rank MODE] QUERY...
//! packset due [WORKSPACE]
//! packset islands [WORKSPACE]
//! packset island [--workspace WS] [--fire] CUE
//! packset fire [--workspace WS] ID ID...
//! packset grade ID [--lapsed] [WORKSPACE]
//! packset forget WORKSPACE
//! packset retract ID [--workspace WS] [--why ACCESSION]
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
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
    load_seat_env();
    let args: Vec<String> = env::args().skip(1).collect();
    let (verb, rest) = args
        .split_first()
        .map_or(("ensure", &[][..]), |(head, tail)| (head.as_str(), tail));
    let port = port();

    match verb {
        "-h" | "--help" | "help" => {
            match rest.first().map(String::as_str) {
                Some(named) if spec(named).is_some() => println!("{}", verb_usage(named)),
                _ => println!("{}", usage()),
            }
            return Ok(());
        }
        "-V" | "--version" => {
            println!(
                "packset {} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("PACKSET_COMMIT")
            );
            return Ok(());
        }
        _ => {}
    }
    let Some(takes) = spec(verb) else {
        anyhow::bail!("unknown command: {verb}\n\n{}", usage());
    };
    // Every verb's arguments go through one parser before anything runs, so
    // `packset forget --help` prints help rather than forgetting a workspace
    // called `--help`, and a flag a verb does not know is refused.
    let args = match parse(verb, &takes, rest)? {
        Parsed::Help => {
            println!("{}", verb_usage(verb));
            return Ok(());
        }
        Parsed::Run(args) => args,
    };

    match verb {
        "ensure" => {
            let owned = args.switch("--owned");
            let already = procfs::listening(port);
            if already && !is_ours(port) {
                anyhow::bail!("{}", held_by(port));
            }
            let held = if already {
                if owned {
                    eprintln!(
                        "packset: a writer is already listening on 127.0.0.1:{port}; --owned leaves it running"
                    );
                }
                None
            } else {
                start(port, owned)?
            };
            wait_healthy(port)?;
            // A writer this call just started is the binary and the home
            // this process passed it. Asking it for status would scan the
            // pack, and an upgrade open has to answer inside the startup
            // budget. A writer that was already up can be another home or
            // another build, and that question is one status.
            if already {
                if let Some(line) = writer_problem(port) {
                    anyhow::bail!("{line}");
                }
            }
            print_url(port);
            if let Some(held) = held {
                wait_for_owned(held)?;
            }
            Ok(())
        }
        "start" => {
            if let Some(held) = start(port, args.switch("--owned"))? {
                wait_for_owned(held)?;
            }
            Ok(())
        }
        "stop" => stop(port),
        "status" => status(port, args.switch("--all"), args.first()),
        "port" => {
            println!("{port}");
            Ok(())
        }
        "url" => {
            print_url(port);
            Ok(())
        }
        "migrate-home" => migrate_home(port),
        "which" => {
            let daemon = resolve_daemon().ok_or_else(missing_daemon)?;
            println!("{}", daemon.display());
            Ok(())
        }
        "remember" => write("lesson", &args),
        "prefer" => write("preference", &args),
        "search" => search(&args),
        "due" => due(args.first()),
        "sweep" => sweep(args.first()),
        "forget" => forget(args.first()),
        "retract" => retract(&args),
        "islands" => islands(args.first()),
        "hubs" => hubs(args.first()),
        "island" => island(&args),
        "fire" => fire(&args),
        "grade" => grade(&args),
        "pin" => pin(args.first()),
        "accessions" => accessions(args.first()),
        "atoms" => atoms(&args),
        "export" => export(&args),
        "citers" => citers(args.first(), args.second()),
        other => unreachable!("{other} has a spec and no arm"),
    }
}

/// What one verb takes besides its name.
#[derive(Debug, Clone, Copy)]
struct Spec {
    /// Flags that take the next argument as their value.
    values: &'static [&'static str],
    /// Flags that stand alone.
    switches: &'static [&'static str],
    /// The most positionals the verb reads; `None` when they are free text
    /// or a list.
    most: Option<usize>,
    /// The positionals are the words of a text, so once a plain word has
    /// been read, a later word that starts with a dash is a word too.
    text: bool,
}

const NOTHING: Spec = Spec {
    values: &[],
    switches: &[],
    most: Some(0),
    text: false,
};

const ONE_WORKSPACE: Spec = Spec {
    values: &[],
    switches: &[],
    most: Some(1),
    text: false,
};

/// The arguments each verb takes, or `None` for a verb packset does not have.
fn spec(verb: &str) -> Option<Spec> {
    Some(match verb {
        "ensure" | "start" => Spec {
            switches: &["--owned"],
            ..NOTHING
        },
        "stop" | "port" | "url" | "which" | "migrate-home" => NOTHING,
        "status" => Spec {
            switches: &["--all"],
            ..ONE_WORKSPACE
        },
        "due" | "sweep" | "forget" | "islands" | "hubs" | "pin" | "accessions" => ONE_WORKSPACE,
        "retract" => Spec {
            values: &["--workspace", "--why"],
            switches: &[],
            most: Some(1),
            text: false,
        },
        "remember" | "prefer" => Spec {
            values: &["--workspace"],
            switches: &[],
            most: None,
            text: true,
        },
        "search" => Spec {
            values: &["--workspace", "--as-of", "--rank"],
            switches: &["--rerank"],
            most: None,
            text: true,
        },
        "island" => Spec {
            values: &["--workspace"],
            switches: &["--fire"],
            most: None,
            text: true,
        },
        "fire" => Spec {
            values: &["--workspace"],
            switches: &[],
            most: None,
            text: false,
        },
        "grade" => Spec {
            values: &[],
            switches: &["--lapsed"],
            most: Some(2),
            text: false,
        },
        "atoms" => Spec {
            values: &["--as-of"],
            ..ONE_WORKSPACE
        },
        "export" => Spec {
            values: &["--into"],
            ..ONE_WORKSPACE
        },
        "citers" => Spec {
            most: Some(2),
            ..ONE_WORKSPACE
        },
        _ => return None,
    })
}

/// A verb's arguments, sorted into flags and positionals.
#[derive(Debug, Default)]
struct Args {
    values: Vec<(&'static str, String)>,
    switches: Vec<&'static str>,
    positional: Vec<String>,
}

impl Args {
    /// The value a flag was given; the last one when it was given twice.
    fn value(&self, flag: &str) -> Option<&str> {
        self.values
            .iter()
            .rev()
            .find(|(name, _)| *name == flag)
            .map(|(_, value)| value.as_str())
    }

    fn switch(&self, flag: &str) -> bool {
        self.switches.contains(&flag)
    }

    fn first(&self) -> Option<&str> {
        self.positional.first().map(String::as_str)
    }

    fn second(&self) -> Option<&str> {
        self.positional.get(1).map(String::as_str)
    }
}

#[derive(Debug)]
enum Parsed {
    /// `-h` or `--help` came before any `--`: print the verb's usage and do
    /// nothing else.
    Help,
    Run(Args),
}

/// Whether an argument looks like a flag. A lone `-` is a positional.
fn dashed(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-')
}

/// Sort `args` into what `takes` says the verb takes.
///
/// `-h` or `--help` anywhere before `--` asks for help, even in a text, so
/// help never runs the verb. A flag the verb does not take is refused rather
/// than read as a workspace, an id or the first word of a text. `--` ends the
/// flags: everything after it is a positional, dash or not.
fn parse(verb: &str, takes: &Spec, args: &[String]) -> anyhow::Result<Parsed> {
    let mut out = Args::default();
    let mut at = 0;
    let mut ended = false;
    while at < args.len() {
        let arg = args[at].as_str();
        at += 1;
        if ended || !dashed(arg) {
            out.positional.push(arg.to_string());
            continue;
        }
        if arg == "--" {
            ended = true;
            continue;
        }
        if arg == "-h" || arg == "--help" {
            return Ok(Parsed::Help);
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value)),
            _ => (arg, None),
        };
        if let Some(flag) = takes.values.iter().copied().find(|f| *f == name) {
            let value = match inline {
                Some(value) => value.to_string(),
                None => {
                    let next = args.get(at).ok_or_else(|| {
                        anyhow::anyhow!("{verb}: {flag} needs a value\n\n{}", verb_usage(verb))
                    })?;
                    if dashed(next) {
                        anyhow::bail!(
                            "{verb}: {flag} needs a value, not {next}\n\n{}",
                            verb_usage(verb)
                        );
                    }
                    at += 1;
                    next.clone()
                }
            };
            if value.trim().is_empty() {
                anyhow::bail!("{verb}: {flag} needs a value\n\n{}", verb_usage(verb));
            }
            out.values.push((flag, value));
            continue;
        }
        if inline.is_none() {
            if let Some(flag) = takes.switches.iter().copied().find(|f| *f == name) {
                out.switches.push(flag);
                continue;
            }
        }
        if takes.text && !out.positional.is_empty() {
            out.positional.push(arg.to_string());
            continue;
        }
        anyhow::bail!(
            "{verb}: unknown flag {arg}; put -- before an argument that starts with a dash\n\n{}",
            verb_usage(verb)
        );
    }
    if let Some(most) = takes.most {
        if out.positional.len() > most {
            let extra = out.positional[most..].join(" ");
            anyhow::bail!(
                "{verb}: takes at most {most} argument{}, and {extra} is one too many\n\n{}",
                if most == 1 { "" } else { "s" },
                verb_usage(verb)
            );
        }
    }
    Ok(Parsed::Run(out))
}

/// The verbs one usage line names: `forget WS ...` names `forget`, and
/// `start | stop` names both.
fn line_names(line: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut words = line.split_whitespace();
    while let Some(word) = words.next() {
        names.push(word);
        if words.next() != Some("|") {
            break;
        }
    }
    names
}

/// One verb's lines of [`usage`], or the whole of it for a verb it does not
/// name.
fn verb_usage(verb: &str) -> String {
    let all = usage();
    let lines: Vec<&str> = all
        .lines()
        .skip(1)
        .filter(|line| line_names(line).contains(&verb))
        .map(str::trim)
        .collect();
    if lines.is_empty() {
        return all;
    }
    let mut out = String::from("usage: packset ");
    out.push_str(&lines.join("\n       packset "));
    out.push_str("\n\n-h, --help prints this and runs nothing; -- ends the flags");
    out
}

fn usage() -> String {
    "packset: start, stop and inspect the pack writer\n\
     \n\
         ensure [--owned]       start if down, then print the URL. With --owned this process stays, and the writer exits when it does\n\
         sweep [WS]             lapse the reviews left due past twice their interval; the third miss forgets a never-recalled lesson\n\
         forget WS              drop a scratch workspace's live atoms, keep its tombstones; smoke and herd runs call this on the way out\n\
         retract ID [--workspace WS] [--why ACCESSION]  withdraw one claim; the deed that showed it wrong, or none\n\
         start [--owned]        the writer alone. With --owned this process stays, and the writer exits when it does\n\
         stop                   stop the writer\n\
         status [--all | WORKSPACE]  counts by kind, pin, index; --all over every workspace\n\
         port | url | which\n\
         migrate-home           move ~/.ljos/memory to $XDG_DATA_HOME/packset; the writer must be stopped\n\
         remember [--workspace WS] TEXT   one lesson\n\
         prefer [--workspace WS] TEXT     one standing preference\n\
         search [--workspace WS] [--as-of TS] [--rerank] [--rank MODE] QUERY  ranked claims live now, or at TS\n\
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

fn client() -> anyhow::Result<PacksetClient> {
    // A named PACKSET_URL (INSIDE_MEMORY_URL is its alias) wins; the
    // loopback port is only the fallback when no URL is set at all.
    // `off` is the one way to have no pack, and the seat honors it as
    // "no pack on purpose" -- so data commands refuse rather than
    // writing to a loopback writer the seat turned off.
    match packset_client::PacksetClient::from_env() {
        Ok(named) => Ok(named),
        Err(packset_client::Error::NoUrl) => {
            anyhow::bail!("PACKSET_URL=off: no pack on purpose")
        }
        Err(other) => Err(other.into()),
    }
}

/// Whether the writer on this port is one of ours. Port-pinned on
/// purpose: writer management answers about the port even when a named
/// PACKSET_URL points the data commands elsewhere.
fn is_ours(port: u16) -> bool {
    // One probe. The retrying health call waits out the client budget on a
    // refused connection and starts a writer when PACKSET_HOME is set.
    PacksetClient::new(format!("http://127.0.0.1:{port}"))
        .health_once()
        .is_ok_and(|body| OURS.iter().any(|name| body.trim_start().starts_with(name)))
}

/// KEY=VALUE pairs from a seat env file. Comments and blank lines stay out.
fn env_pairs(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        pairs.push((k.to_string(), v.trim().to_string()));
    }
    pairs
}

/// Load `~/.config/ljos/env` when the process has not set those keys, so
/// bare `packset search` speaks the same workspace `ljos doctor` prints.
fn load_seat_env() {
    let Some(home) = env::var_os("HOME") else {
        return;
    };
    let path = PathBuf::from(home).join(".config/ljos/env");
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    for (k, v) in env_pairs(&text) {
        if env::var_os(&k).is_none() {
            env::set_var(k, v);
        }
    }
}

/// The workspace a command was given, or the one the environment names.
/// The workspace a verb acts on: the argument, else `PACKSET_WORKSPACE`, else
/// what the client derives from the working directory's git remote, else
/// `default`. The same answer every other client gives.
fn workspace(given: Option<&str>) -> anyhow::Result<String> {
    if let Some(w) = given
        .map(str::to_string)
        .or_else(|| env::var("PACKSET_WORKSPACE").ok())
        .filter(|w| !w.is_empty())
    {
        return Ok(w);
    }
    Ok(client()?.workspace())
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

/// The write end of an owned writer's pipe, held for this process's life.
///
/// The writer has the read end as its stdin. This end is close-on-exec, so
/// the writer does not inherit it. When this process dies, including by a
/// signal it cannot catch, the kernel closes the end and the writer sees
/// EOF.
struct OwnedHold {
    child: Child,
    pipe: std::io::PipeWriter,
}

/// The build that produces `packset` and `packsetd` together.
const BUILD_HINT: &str = "cargo build --release -p packset";

/// `packsetd` is not beside this binary and not on `PATH`.
fn missing_daemon() -> anyhow::Error {
    anyhow::anyhow!("no packsetd binary beside packset; {BUILD_HINT}")
}

/// Who holds `port`, and what to run instead.
fn held_by(port: u16) -> String {
    match procfs::pid_on_port(port) {
        Some(pid) => {
            let name = procfs::process_name(pid)
                .map(|name| format!(" ({name})"))
                .unwrap_or_default();
            format!(
                "port {port} is held by pid {pid}{name}; stop that process, or set PACKSET_PORT to a free port and run packset ensure"
            )
        }
        None => format!(
            "port {port} is held by another process; stop that process, or set PACKSET_PORT to a free port and run packset ensure"
        ),
    }
}

/// Whether `a` and `b` are the same directory, following links when both exist.
fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// The running writer's build against this binary. `None` when they match,
/// or when the writer did not name a build.
fn build_mismatch(
    ours_version: &str,
    ours_commit: &str,
    status: &serde_json::Value,
) -> Option<String> {
    let theirs_v = status
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let theirs_c = status
        .get("commit")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if theirs_v.is_empty() && theirs_c.is_empty() {
        return None;
    }
    if theirs_v == ours_version && (theirs_c.is_empty() || theirs_c == ours_commit) {
        return None;
    }
    let shown = if theirs_c.is_empty() {
        "no commit"
    } else {
        theirs_c
    };
    Some(format!(
        "writer is packsetd {theirs_v} ({shown}); this client is {ours_version} ({ours_commit}); packset stop && packset ensure replaces the writer"
    ))
}

/// Who is listening on `port`, for an error a person can act on.
fn holder_on(port: u16) -> String {
    match procfs::pid_on_port(port) {
        Some(pid) => {
            let name = procfs::process_name(pid)
                .map(|name| format!(" ({name})"))
                .unwrap_or_default();
            format!("pid {pid}{name}")
        }
        None => "another process".to_string(),
    }
}

/// The command that starts this pack on a port `avoid` is not using.
fn second_port_command(avoid: u16) -> String {
    format!(
        "PACKSET_PORT={} packset ensure",
        procfs::suggest_port(avoid)
    )
}

/// The running writer's home against the one this client would open.
fn home_mismatch_line(port: u16, theirs: &str, ours: &Path) -> Option<String> {
    if theirs.is_empty() || same_path(Path::new(theirs), ours) {
        return None;
    }
    Some(format!(
        "writer on 127.0.0.1:{port} is {} for home {theirs}; this client uses {}. A second pack needs its own port: {}",
        holder_on(port),
        ours.display(),
        second_port_command(port)
    ))
}

/// The token on this home was refused by the writer already on `port`.
///
/// `packset stop` is not the next command. It stops whatever writer is on
/// `port`, which is the other pack.
fn refused_token(port: u16) -> String {
    let home = packset_core::home::resolve()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "the default pack home".to_string());
    format!(
        "port {port} is held by {} for another pack; this client uses {home}, and the writer refused its token (401). A second pack needs its own port: {}",
        holder_on(port),
        second_port_command(port)
    )
}

/// Why this client should not keep the writer already on `port`.
///
/// A transport error is not one of those reasons: `/health` already
/// answered, and a status that failed for another cause leaves ensure
/// to print the URL.
fn writer_problem(port: u16) -> Option<String> {
    let client = PacksetClient::new(format!("http://127.0.0.1:{port}"));
    match client.status(None) {
        Ok(status) => {
            let mut lines = Vec::new();
            if let Some(theirs) = status.get("home").and_then(serde_json::Value::as_str) {
                if let Some(ours) = packset_core::home::resolve() {
                    if let Some(line) = home_mismatch_line(port, theirs, &ours) {
                        lines.push(line);
                    }
                }
            }
            if let Some(line) =
                build_mismatch(env!("CARGO_PKG_VERSION"), env!("PACKSET_COMMIT"), &status)
            {
                lines.push(line);
            }
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        }
        Err(err) if err.to_string().contains("401") => Some(refused_token(port)),
        Err(_) => None,
    }
}

/// Start the daemon and wait for it to bind.
///
/// `owned` keeps the returned pipe open in this process and passes the read
/// end as the writer's stdin. The caller waits on the child, so the pipe
/// stays open until this process dies. A shared start returns `None` and
/// detaches, as before.
fn start(port: u16, owned: bool) -> anyhow::Result<Option<OwnedHold>> {
    if procfs::listening(port) {
        if is_ours(port) {
            eprintln!("packset: already listening on 127.0.0.1:{port}");
            return Ok(None);
        }
        anyhow::bail!("{}", held_by(port));
    }
    let daemon = resolve_daemon().ok_or_else(missing_daemon)?;
    let log = log_path();
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).map_err(|err| {
            anyhow::anyhow!(
                "cannot write the log directory {} ({err}); set PACKSET_LOG or XDG_STATE_HOME to a directory this user can write",
                dir.display()
            )
        })?;
    }
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|err| {
            anyhow::anyhow!(
                "cannot write the log {} ({err}); set PACKSET_LOG to a file this user can write",
                log.display()
            )
        })?;
    let errs = out.try_clone()?;

    let mut command = Command::new(&daemon);
    command
        .arg("--port")
        .arg(port.to_string())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(errs));
    // The read end becomes the writer's stdin. The write end stays here.
    // `std::io::pipe` marks both ends close-on-exec, and only the read end
    // is handed to the child, so the writer's own copy of the write end
    // cannot keep the pipe open after we die.
    let pipe = if owned {
        let (reader, writer) = std::io::pipe()?;
        command.arg("--owned").stdin(Stdio::from(reader));
        Some(writer)
    } else {
        command.stdin(Stdio::null());
        None
    };
    // A new session, so a hangup reaches this process and not the writer.
    // A shared writer then stays up. An owned writer still exits: this
    // process holds its pipe, the hangup or any other signal kills us, the
    // kernel closes the pipe, and the writer flushes.
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
            return match pipe {
                Some(pipe) => Ok(Some(OwnedHold { child, pipe })),
                None => Ok(None),
            };
        }
        if let Ok(Some(code)) = child.try_wait() {
            // The home is already open by a writer on some other port.
            // That writer will not bind this one, so the startup wait
            // would only hold the error back.
            if packset_core::home::resolve()
                .is_some_and(|home| home_open_on_another_port(&home, port, &log))
            {
                anyhow::bail!(
                    "packsetd exited {code}; see {}{}",
                    log.display(),
                    tail(&log)
                );
            }
            // Many seats run `ensure` at once when a herd starts; the losers
            // exit on the port or the store lock while the winner is coming
            // up. A writer listening on our port is the outcome asked for,
            // whichever process it is. It is not owned by us.
            let sibling = Instant::now() + STARTUP;
            while Instant::now() < sibling {
                if procfs::listening(port) {
                    eprintln!("packset: another seat's writer came up on 127.0.0.1:{port}");
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
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

/// Block until the owned writer exits. The pipe in `held` stays open until
/// then, which is what keeps the writer alive.
fn wait_for_owned(mut held: OwnedHold) -> anyhow::Result<()> {
    // The URL was just printed. On a pipe, stdout is block-buffered, and
    // the next line waits until this process dies, so the seat would never
    // see the URL it is waiting on.
    let _ = std::io::stdout().flush();
    let status = held.child.wait()?;
    drop(held.pipe);
    if status.success() {
        return Ok(());
    }
    let log = log_path();
    anyhow::bail!(
        "packsetd exited {status}; see {}{}",
        log.display(),
        tail(&log)
    )
}

/// A bound port is not yet an answering writer: the store opens after the
/// listener, so a seat that asks the moment `ensure` returns can still be
/// refused. Wait for `/health` to say it is ours, within the startup budget.
fn wait_healthy(port: u16) -> anyhow::Result<()> {
    let deadline = Instant::now() + STARTUP;
    while Instant::now() < deadline {
        if is_ours(port) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("127.0.0.1:{port} is bound but /health does not answer as packsetd")
}

/// The child lost the home lock, and the writer that holds the home is
/// listening on a port other than `port`.
fn home_open_on_another_port(home: &Path, port: u16, log: &Path) -> bool {
    let Ok(text) = fs::read_to_string(log) else {
        return false;
    };
    if !text.contains("already open") {
        return false;
    }
    let Ok(raw) = fs::read_to_string(home.join("port")) else {
        return false;
    };
    let Some(other) = raw.trim().parse::<u16>().ok() else {
        return false;
    };
    other != port && procfs::listening(other)
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
    // Nothing is listening. Ask nothing: a health probe would retry the
    // refusal for the whole budget, and with PACKSET_HOME set it would
    // start a writer and then this command would kill it.
    if !procfs::listening(port) {
        eprintln!("packset: nothing of ours to stop on {port}");
        return Ok(());
    }
    if !is_ours(port) {
        anyhow::bail!("{}", held_by(port));
    }
    let Some(pid) = procfs::pid_on_port(port) else {
        anyhow::bail!(
            "listening on {port} but the holder is not visible; the port stays taken until that process exits"
        );
    };
    procfs::terminate(pid)?;
    // A writer flushes on the signal before it lets the port go; `stop`
    // returns once the port is free, so an `ensure` after it starts one.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while procfs::listening(port) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if procfs::listening(port) {
        anyhow::bail!("packset: signalled {pid}, and {port} is still held after 10s");
    }
    eprintln!("packset: stopped {pid}");
    Ok(())
}

/// Move the old pack home to the XDG one. The writer holds the database
/// open, so this refuses while one of ours listens.
fn migrate_home(port: u16) -> anyhow::Result<()> {
    if let Some(named) = packset_core::home::named_home() {
        anyhow::bail!(
            "PACKSET_HOME (or an older name) is set to {}; that home stays where it is",
            named.display()
        );
    }
    let old =
        packset_core::home::legacy_home().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let new =
        packset_core::home::current_home().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    if is_ours(port) {
        anyhow::bail!("a writer is listening on {port}; `packset stop` first");
    }
    // A writer on another port holds the same database open. Its lock says
    // so whatever port it took.
    if home_is_held(&old) {
        anyhow::bail!(
            "a writer has {} open; stop it first",
            old.join("packsetd.lock").display()
        );
    }
    packset_core::home::migrate(&old, &new).map_err(|e| anyhow::anyhow!(e))?;
    println!(
        "packset: moved {} to {}; the old name links to it",
        old.display(),
        new.display()
    );
    Ok(())
}

/// Whether a running packsetd holds the lock in `home`. A home with no lock
/// file has never had a writer, so it is free.
fn home_is_held(home: &std::path::Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = fs::OpenOptions::new()
        .append(true)
        .open(home.join("packsetd.lock"))
    else {
        return false;
    };
    // SAFETY: flock on a descriptor this function owns; closing it on return
    // drops the lock again.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0 }
}

/// What `status` and `search` say when the local writer is not listening.
fn writer_down(port: u16) -> String {
    format!("writer is not running on 127.0.0.1:{port}; packset ensure starts it")
}

fn print_url(port: u16) {
    println!("PACKSET_URL=http://127.0.0.1:{port}");
    println!("INSIDE_MEMORY_URL=http://127.0.0.1:{port}");
}

fn status(port: u16, all: bool, given: Option<&str>) -> anyhow::Result<()> {
    let client = client()?;
    // A named PACKSET_URL points the read at that writer; the loopback
    // port gates only when the URL did not name one.
    if client.base() == format!("http://127.0.0.1:{port}") {
        if !procfs::listening(port) {
            anyhow::bail!("{}", writer_down(port));
        }
        if !is_ours(port) {
            anyhow::bail!("{}", held_by(port));
        }
    }
    let health = match client.health() {
        Ok(health) if !health.trim().is_empty() => health,
        Ok(_) => anyhow::bail!(
            "writer at {} did not answer /health; check PACKSET_URL, or packset ensure when it is the local writer",
            client.base()
        ),
        Err(err) => anyhow::bail!(
            "writer at {} did not answer ({err}); check PACKSET_URL, or packset ensure when it is the local writer",
            client.base()
        ),
    };
    if all && given.is_some() {
        anyhow::bail!("status: --all counts every workspace; drop the workspace or the flag");
    }
    let scope = if all { None } else { Some(workspace(given)?) };
    // Print the up line only once this client can read the pack. A writer
    // for another home answers /health and then refuses the token; saying
    // up before that refusal looks like the pack opened.
    let detail = client.status(scope.as_deref())?;
    println!("packset: up on {} ({})", client.base(), health.trim());
    println!("{}", serde_json::to_string_pretty(&detail)?);
    Ok(())
}

fn pin(name: Option<&str>) -> anyhow::Result<()> {
    let client = client()?;
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
fn citers(accession: Option<&str>, given: Option<&str>) -> anyhow::Result<()> {
    let accession =
        accession.ok_or_else(|| anyhow::anyhow!("name an accession: packset citers ACCESSION"))?;
    let workspace = workspace(given)?;
    for atom in client()?.citers(&workspace, accession)? {
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
fn export(args: &Args) -> anyhow::Result<()> {
    let into = args
        .value("--into")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("export needs --into DIR"))?;
    let workspace = workspace(args.first())?;
    let held = client()?;
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

/// The writer's warning lines, `warning` then `prose.warning`.
fn warning_lines(stored: &serde_json::Value) -> Vec<&str> {
    [stored.get("warning"), stored.pointer("/prose/warning")]
        .into_iter()
        .filter_map(|line| line.and_then(serde_json::Value::as_str))
        .filter(|line| !line.is_empty())
        .collect()
}

/// POST one explicit claim of `kind`; the text is stored as given.
/// A warning on the answer goes to stderr. Stdout stays the one id line.
fn write(kind: &str, args: &Args) -> anyhow::Result<()> {
    let text = args.positional.join(" ").trim().to_string();
    if text.is_empty() {
        anyhow::bail!("{kind}: the text is the claim; pass it");
    }
    let workspace = workspace(args.value("--workspace"))?;
    let atom = serde_json::json!({
        "schema": "inside.atom/v1",
        "kind": kind,
        "level": "explicit",
        "text": text,
        "workspace": workspace,
    });
    let stored = client()?.post_atom(&atom)?;
    println!(
        "{}\t{}\tdue {}",
        stored["id"].as_str().unwrap_or("-"),
        kind,
        stored["due_at"].as_str().unwrap_or("-")
    );
    for line in warning_lines(&stored) {
        eprintln!("packset: warning: {line}");
    }
    Ok(())
}

/// Ranked claims for a question: score, kind, id, text.
/// `--as-of TS` ranks the claims whose window contained TS.
/// `--rerank` runs the measured cross-encoder second stage.
/// `--rank relevance` chooses the top k by lexical, BM25 and cosine,
/// with no half-life and no retrievability. The default is unchanged.
fn search(args: &Args) -> anyhow::Result<()> {
    let query = args.positional.join(" ").trim().to_string();
    if query.is_empty() {
        anyhow::bail!("search: pass a question");
    }
    let held = client()?;
    // Before the workspace lookup. That lookup asks the writer, and a
    // refused connection there would wait out the budget.
    if held.base() == format!("http://127.0.0.1:{}", port()) && !procfs::listening(port()) {
        anyhow::bail!("{}", writer_down(port()));
    }
    let workspace = workspace(args.value("--workspace"))?;
    eprintln!("packset: workspace {workspace}");
    let hits = held.search_ranked(
        &workspace,
        &query,
        10,
        args.value("--as-of"),
        args.switch("--rerank"),
        args.value("--rank"),
    )?;
    if hits.is_empty() {
        eprintln!("packset: no claims matched {query:?} in {workspace}");
    }
    for hit in hits {
        let id = hit.id.as_deref().unwrap_or("-");
        // The fifth column is the cross-encoder score, and only when the
        // stage scored this hit. A search without it keeps the four columns.
        // Labels, when the hit has any, are one more column after that, so
        // an unlabelled line keeps the columns it had. A comma joins the
        // names: they are tokens, and the text stays the fourth field.
        let line = match hit.rerank_score {
            Some(rerank) => format!(
                "{:.4}\t{}\t{}\t{}\t{:.4}",
                hit.score, hit.kind, id, hit.text, rerank
            ),
            None => format!("{:.4}\t{}\t{}\t{}", hit.score, hit.kind, id, hit.text),
        };
        if hit.labels.is_empty() {
            println!("{line}");
        } else {
            println!("{line}\t{}", hit.labels.join(","));
        }
    }
    Ok(())
}

/// Live claims whose `due_at` has passed, soonest first: due, id, text.
/// Lapse what was left due past twice its interval; the third miss forgets.
fn sweep(given: Option<&str>) -> anyhow::Result<()> {
    let workspace = workspace(given)?;
    let report = client()?.sweep(&workspace)?;
    println!(
        "{} lapsed by neglect, {} forgotten",
        report["lapsed"].as_u64().unwrap_or(0),
        report["forgotten"].as_u64().unwrap_or(0)
    );
    for id in report["forgotten_ids"].as_array().into_iter().flatten() {
        println!("forgotten\t{}", id.as_str().unwrap_or("-"));
    }
    Ok(())
}

/// Drop a scratch workspace's live atoms, no deed. Its tombstones stay, so a
/// deleted id is not posted again; a live atom's id is free afterwards. This
/// is not retraction -- a retraction names the deed that withdrew the claim.
/// A per-run scratch workspace calls this on its way out, so the long-lived
/// writer does not keep every smoke and herd run's atoms.
fn forget(given: Option<&str>) -> anyhow::Result<()> {
    let workspace = given
        .map(str::to_string)
        .filter(|w| !w.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("forget needs a workspace: packset forget WORKSPACE"))?;
    let report = client()?.forget_workspace(&workspace)?;
    println!(
        "{} forgotten from {}",
        report["forgotten"].as_u64().unwrap_or(0),
        report["workspace"].as_str().unwrap_or(&workspace)
    );
    Ok(())
}

/// Withdraw one claim. `--why` is the deed accession that showed it wrong;
/// the daemon refuses one that is not an accession. The line is the id,
/// `retracted`, and that deed, or `-` when the seat named none.
///
/// This is not [`forget`]. Forget drops a scratch workspace and names no
/// deed. A retraction leaves the record, so search from now does not list
/// the claim and a dated read from before the retraction still does.
fn retract(args: &Args) -> anyhow::Result<()> {
    let id = args
        .first()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("retract needs an atom id"))?;
    let workspace = workspace(args.value("--workspace"))?;
    let why = args.value("--why").map(str::trim).filter(|w| !w.is_empty());
    client()?.delete_atom(&workspace, id, why)?;
    println!("{id}\tretracted\t{}", why.unwrap_or("-"));
    Ok(())
}

fn due(given: Option<&str>) -> anyhow::Result<()> {
    let workspace = workspace(given)?;
    let now = packset_core::clock::utcnow();
    let held = client()?.atoms_as_of(&workspace, None)?;
    let mut atoms: Vec<serde_json::Value> = held
        .iter()
        .filter(|a| {
            a["due_at"]
                .as_str()
                .is_some_and(|d| !d.is_empty() && d <= now.as_str())
        })
        .cloned()
        .collect();
    if atoms.is_empty() {
        let next = held
            .iter()
            .filter_map(|atom| atom["due_at"].as_str())
            .filter(|due| !due.is_empty())
            .min();
        match next {
            Some(next) => {
                eprintln!("packset: nothing is due in {workspace}; the next review is {next}")
            }
            None => eprintln!("packset: nothing is due in {workspace}"),
        }
        return Ok(());
    }
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
fn hubs(given: Option<&str>) -> anyhow::Result<()> {
    let workspace = match given {
        Some(w) => w.to_string(),
        None => client()?.workspace(),
    };
    let body = client()?.hubs(&workspace, 10)?;
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

fn islands(given: Option<&str>) -> anyhow::Result<()> {
    let workspace = workspace(given)?;
    let body = client()?.islands(&workspace)?;
    for island in body["islands"].as_array().into_iter().flatten() {
        let first = island["atoms"][0]["text"].as_str().unwrap_or("");
        println!("{}\t{}", island["size"], first);
    }
    Ok(())
}

/// Two or more claims fired together.
fn fire(args: &Args) -> anyhow::Result<()> {
    let ids = &args.positional;
    if ids.len() < 2 {
        anyhow::bail!("fire: pass two or more claim ids that fired together");
    }
    let workspace = workspace(args.value("--workspace"))?;
    let body = client()?.fire(&workspace, ids)?;
    println!("{} fired, {} changed", body["fired"], body["changed"]);
    Ok(())
}

/// The memories a cue activates: activation, seed mark, id, text.
///
/// An empty island is one line on stderr: nothing to show, and why.
/// Stdout stays the table, so a miss does not look like a row.
fn island(args: &Args) -> anyhow::Result<()> {
    let firing = args.switch("--fire");
    let cue = args.positional.join(" ").trim().to_string();
    if cue.is_empty() {
        anyhow::bail!("island: pass the cue, the task or question at hand");
    }
    let workspace = workspace(args.value("--workspace"))?;
    let body = client()?.activate(&workspace, &cue, 24, firing)?;
    let rows = body["island"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if rows.is_empty() {
        eprintln!("{}", island_empty_line(&workspace, &cue, &body));
        return Ok(());
    }
    for atom in rows {
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

/// One line when a cue activates nothing. The clause after the semicolon
/// is why: no claim matched, the seeds were weak, or activation did not spread.
fn island_empty_line(workspace: &str, cue: &str, body: &serde_json::Value) -> String {
    let seeds = body
        .get("seeds")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let why = if seeds == 0 {
        format!("no claim matched {cue:?}")
    } else if body
        .get("weak")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        "no seed both scorers agreed on".to_string()
    } else {
        "activation did not spread".to_string()
    };
    format!("packset: nothing to show in {workspace}; {why}")
}

/// Grade one review: recalled unless `--lapsed`.
fn grade(args: &Args) -> anyhow::Result<()> {
    let lapsed = args.switch("--lapsed");
    let id = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("grade needs an atom id"))?;
    let workspace = workspace(args.second())?;
    let graded = client()?.grade(&workspace, id, !lapsed)?;
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
fn atoms(args: &Args) -> anyhow::Result<()> {
    let workspace = workspace(args.first())?;
    eprintln!("packset: workspace {workspace}");
    for atom in client()?.atoms_as_of(&workspace, args.value("--as-of"))? {
        println!("{}", serde_json::to_string(&atom)?);
    }
    Ok(())
}

fn accessions(given: Option<&str>) -> anyhow::Result<()> {
    let workspace = workspace(given)?;
    for accession in client()?.accessions(&workspace)? {
        println!("{accession}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse, spec, verb_usage, Args, Parsed};

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    /// The verb's arguments as it would run them; a panic on help or a refusal.
    fn run_args(verb: &str, args: &[&str]) -> Args {
        match parse(verb, &spec(verb).unwrap(), &strings(args)).unwrap() {
            Parsed::Run(args) => args,
            Parsed::Help => panic!("{verb} {args:?} asked for help"),
        }
    }

    fn refused(verb: &str, args: &[&str]) -> String {
        parse(verb, &spec(verb).unwrap(), &strings(args))
            .map(|parsed| panic!("{verb} {args:?} ran: {parsed:?}"))
            .unwrap_err()
            .to_string()
    }

    /// Every verb `run` dispatches.
    const VERBS: &[&str] = &[
        "ensure",
        "start",
        "stop",
        "status",
        "port",
        "url",
        "which",
        "remember",
        "prefer",
        "search",
        "due",
        "sweep",
        "forget",
        "retract",
        "islands",
        "hubs",
        "island",
        "fire",
        "grade",
        "pin",
        "accessions",
        "atoms",
        "export",
        "citers",
    ];

    #[test]
    fn forget_help_is_help_not_a_workspace() {
        for help in ["--help", "-h"] {
            assert!(matches!(
                parse("forget", &spec("forget").unwrap(), &strings(&[help])).unwrap(),
                Parsed::Help
            ));
        }
        assert!(verb_usage("forget").contains("packset forget WS"));
    }

    #[test]
    fn help_runs_no_verb_wherever_it_comes_before_the_end_of_flags() {
        for verb in VERBS {
            let takes = spec(verb).unwrap();
            for args in [
                &["--help"][..],
                &["-h"],
                &["seat", "--help"],
                &["--help", "seat"],
            ] {
                assert!(
                    matches!(parse(verb, &takes, &strings(args)), Ok(Parsed::Help)),
                    "{verb} {args:?} did not ask for help"
                );
            }
        }
    }

    #[test]
    fn every_verb_has_its_own_usage_line() {
        for verb in VERBS {
            let usage = verb_usage(verb);
            assert!(
                usage.starts_with("usage: packset "),
                "{verb} has no usage line: {usage}"
            );
        }
    }

    #[test]
    fn an_unknown_flag_is_refused_not_read_as_a_workspace() {
        for verb in [
            "forget",
            "retract",
            "sweep",
            "pin",
            "status",
            "due",
            "islands",
            "hubs",
            "accessions",
            "atoms",
            "citers",
            "grade",
            "fire",
            "stop",
            "start",
            "ensure",
        ] {
            let err = refused(verb, &["--hepl"]);
            assert!(err.contains("unknown flag --hepl"), "{verb}: {err}");
        }
        // A text verb refuses a flag before its first word, too.
        for verb in ["remember", "prefer", "search", "island"] {
            let err = refused(verb, &["--hepl", "the", "claim"]);
            assert!(err.contains("unknown flag --hepl"), "{verb}: {err}");
        }
    }

    #[test]
    fn a_dash_workspace_needs_the_end_of_flags() {
        let args = run_args("forget", &["--", "--odd"]);
        assert_eq!(args.first(), Some("--odd"));
        let args = run_args("forget", &["scratch-1"]);
        assert_eq!(args.first(), Some("scratch-1"));
        // After `--`, even help is a positional.
        let args = run_args("forget", &["--", "--help"]);
        assert_eq!(args.first(), Some("--help"));
        // A lone dash is a positional, not a flag.
        let args = run_args("forget", &["-"]);
        assert_eq!(args.first(), Some("-"));
    }

    #[test]
    fn extra_positionals_are_refused() {
        let err = refused("forget", &["one", "two"]);
        assert!(err.contains("at most 1 argument"), "{err}");
        let err = refused("stop", &["now"]);
        assert!(err.contains("at most 0 arguments"), "{err}");
        let err = refused("citers", &["acc", "seat", "more"]);
        assert!(err.contains("at most 2 arguments"), "{err}");
    }

    #[test]
    fn the_workspace_flag_leaves_the_text() {
        let args = run_args("remember", &["--workspace", "seat", "the", "claim"]);
        assert_eq!(args.value("--workspace"), Some("seat"));
        assert_eq!(args.positional, ["the", "claim"]);
        let args = run_args("remember", &["only"]);
        assert!(args.value("--workspace").is_none());
        assert_eq!(args.positional, ["only"]);
        let args = run_args("remember", &["--workspace=seat", "claim"]);
        assert_eq!(args.value("--workspace"), Some("seat"));
    }

    #[test]
    fn a_dash_word_inside_a_text_is_a_word() {
        let args = run_args("remember", &["pass", "-x", "for", "a", "trace"]);
        assert_eq!(args.positional, ["pass", "-x", "for", "a", "trace"]);
        let args = run_args("remember", &["--", "--force", "is", "never", "safe"]);
        assert_eq!(args.positional, ["--force", "is", "never", "safe"]);
    }

    #[test]
    fn a_value_flag_does_not_take_a_flag_as_its_value() {
        let err = refused("remember", &["--workspace", "--help"]);
        assert!(
            err.contains("--workspace needs a value, not --help"),
            "{err}"
        );
        let err = refused("remember", &["--workspace"]);
        assert!(err.contains("--workspace needs a value"), "{err}");
        let err = refused("export", &["--into="]);
        assert!(err.contains("--into needs a value"), "{err}");
    }

    #[test]
    fn search_as_of_is_not_the_query() {
        let args = run_args("search", &["--as-of", "2024-06-01T00:00:00Z", "Borda"]);
        assert_eq!(args.value("--as-of"), Some("2024-06-01T00:00:00Z"));
        assert_eq!(args.positional, ["Borda"]);
        assert!(args.value("--workspace").is_none());
        let args = run_args(
            "search",
            &[
                "--workspace",
                "acme-cli",
                "--as-of",
                "2024-06-01T00:00:00Z",
                "Borda",
            ],
        );
        assert_eq!(args.value("--workspace"), Some("acme-cli"));
        assert_eq!(args.value("--as-of"), Some("2024-06-01T00:00:00Z"));
        assert_eq!(args.positional, ["Borda"]);
        assert!(!args.switch("--rerank"));
    }

    #[test]
    fn search_rank_is_not_the_query() {
        let args = run_args("search", &["--rank", "relevance", "fusion"]);
        assert_eq!(args.value("--rank"), Some("relevance"));
        assert_eq!(args.positional, ["fusion"]);
        assert!(!args.switch("--rerank"));
    }

    #[test]
    fn search_rerank_is_not_the_query() {
        let args = run_args("search", &["--rerank", "fusion"]);
        assert!(args.switch("--rerank"));
        assert_eq!(args.positional, ["fusion"]);
        assert!(args.value("--as-of").is_none());
        // A known flag after the words is still the flag.
        let args = run_args("island", &["the", "cue", "--fire"]);
        assert!(args.switch("--fire"));
        assert_eq!(args.positional, ["the", "cue"]);
    }

    #[test]
    fn search_as_of_needs_a_timestamp() {
        let err = refused("search", &["--as-of"]);
        assert!(err.contains("--as-of needs a value"), "{err}");
    }

    #[test]
    fn retract_keeps_the_id_and_the_deed_apart() {
        let args = run_args("retract", &["abc", "--why", "deed-test-x"]);
        assert_eq!(args.first(), Some("abc"));
        assert_eq!(args.value("--why"), Some("deed-test-x"));
        assert!(args.value("--workspace").is_none());
        let args = run_args(
            "retract",
            &["--workspace", "seat", "--why", "deed-test-x", "abc"],
        );
        assert_eq!(args.value("--workspace"), Some("seat"));
        assert_eq!(args.value("--why"), Some("deed-test-x"));
        assert_eq!(args.first(), Some("abc"));
        let args = run_args("retract", &["abc"]);
        assert!(args.value("--why").is_none());
        assert_eq!(args.first(), Some("abc"));
        let err = refused("retract", &["abc", "extra"]);
        assert!(err.contains("at most 1 argument"), "{err}");
        assert!(verb_usage("retract").contains("packset retract ID"));
    }

    #[test]
    fn grade_and_export_and_atoms_keep_their_shapes() {
        let args = run_args("grade", &["abc", "--lapsed", "seat"]);
        assert!(args.switch("--lapsed"));
        assert_eq!(args.first(), Some("abc"));
        assert_eq!(args.second(), Some("seat"));
        let args = run_args("export", &["--into", "bag/data/atoms", "git:x/y"]);
        assert_eq!(args.value("--into"), Some("bag/data/atoms"));
        assert_eq!(args.first(), Some("git:x/y"));
        let args = run_args("atoms", &["--as-of", "2024-06-01T00:00:00Z", "seat"]);
        assert_eq!(args.value("--as-of"), Some("2024-06-01T00:00:00Z"));
        assert_eq!(args.first(), Some("seat"));
    }

    #[test]
    fn env_text_skips_comments_and_does_not_invent_keys() {
        let pairs = super::env_pairs(
            "# Shared by the seat\nPACKSET_WORKSPACE=git:example.com/seat/notes\n\nPACKSET_URL=http://127.0.0.1:8761\n=novalue\n",
        );
        assert_eq!(
            pairs,
            vec![
                (
                    "PACKSET_WORKSPACE".into(),
                    "git:example.com/seat/notes".into()
                ),
                ("PACKSET_URL".into(), "http://127.0.0.1:8761".into()),
            ]
        );
    }

    #[test]
    fn status_all_is_the_global_count() {
        let args = run_args("status", &["--all"]);
        assert!(args.switch("--all"));
        assert!(args.first().is_none());
        let args = run_args("status", &["seat"]);
        assert!(!args.switch("--all"));
        assert_eq!(args.first(), Some("seat"));
    }

    #[test]
    fn a_workspace_name_is_one_file_name() {
        assert_eq!(super::export_file_name("seat"), "seat.jsonl");
        assert_eq!(
            super::export_file_name("git:github.com/leidarljos/ljos"),
            "git_github.com_leidarljos_ljos.jsonl"
        );
    }

    #[test]
    fn off_is_no_pack_not_the_loopback_writer() {
        // The seat honors PACKSET_URL=off as "no pack on purpose"; a data
        // command that fell back to loopback would write to a writer the
        // seat turned off. No other test here touches the environment, so
        // this set-and-restore races nothing.
        let url = std::env::var_os("PACKSET_URL");
        let alias = std::env::var_os("INSIDE_MEMORY_URL");
        std::env::set_var("PACKSET_URL", "off");
        std::env::remove_var("INSIDE_MEMORY_URL");
        let err = super::client().unwrap_err();
        assert!(err.to_string().contains("no pack on purpose"), "{err}");
        match url {
            Some(v) => std::env::set_var("PACKSET_URL", v),
            None => std::env::remove_var("PACKSET_URL"),
        }
        match alias {
            Some(v) => std::env::set_var("INSIDE_MEMORY_URL", v),
            None => std::env::remove_var("INSIDE_MEMORY_URL"),
        }
    }

    #[test]
    fn a_different_build_names_the_two_versions_and_the_next_command() {
        let status = serde_json::json!({"version": "0.9.21", "commit": "deadbeef"});
        let line = super::build_mismatch("0.17.10", "84a2b0f0", &status).unwrap();
        assert!(line.contains("0.9.21"), "{line}");
        assert!(line.contains("deadbeef"), "{line}");
        assert!(line.contains("0.17.10"), "{line}");
        assert!(line.contains("84a2b0f0"), "{line}");
        assert!(line.contains("packset stop && packset ensure"), "{line}");
        assert!(super::build_mismatch(
            "0.17.10",
            "84a2b0f0",
            &serde_json::json!({"version": "0.17.10", "commit": "84a2b0f0"})
        )
        .is_none());
    }

    #[test]
    fn an_open_home_on_another_port_is_reported_without_the_sibling_wait() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let log = home.join("log");
        std::fs::write(&log, "the home is already open by pid 4\n").unwrap();
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let other = listener.local_addr().unwrap().port();
        std::fs::write(home.join("port"), format!("{other}\n")).unwrap();
        let asked = if other == 1 { 2 } else { other - 1 };
        assert!(super::home_open_on_another_port(home, asked, &log));
        assert!(!super::home_open_on_another_port(home, other, &log));
        std::fs::write(&log, "cannot bind\n").unwrap();
        assert!(!super::home_open_on_another_port(home, asked, &log));
        drop(listener);
        std::fs::write(&log, "already open\n").unwrap();
        assert!(!super::home_open_on_another_port(home, asked, &log));
    }

    #[test]
    fn a_different_home_names_both_paths_and_the_next_command() {
        let line = super::home_mismatch_line(
            8761,
            "/tmp/other/pack",
            std::path::Path::new("/tmp/this/pack"),
        )
        .unwrap();
        assert!(line.contains("/tmp/other/pack"), "{line}");
        assert!(line.contains("/tmp/this/pack"), "{line}");
        assert!(line.contains("PACKSET_PORT="), "{line}");
        assert!(line.contains("packset ensure"), "{line}");
        assert!(
            !line.contains("packset stop"),
            "stopping would take the other pack down: {line}"
        );
        assert!(super::home_mismatch_line(
            8761,
            "/tmp/this/pack",
            std::path::Path::new("/tmp/this/pack")
        )
        .is_none());
    }

    #[test]
    fn an_empty_island_is_one_line_that_says_why() {
        let missed = super::island_empty_line(
            "demo",
            "fusing two ballots",
            &serde_json::json!({"seeds": 0, "weak": true, "island": []}),
        );
        assert_eq!(
            missed,
            "packset: nothing to show in demo; no claim matched \"fusing two ballots\""
        );
        assert_eq!(missed.lines().count(), 1, "{missed}");
        let weak = super::island_empty_line(
            "demo",
            "ballots",
            &serde_json::json!({"seeds": 1, "weak": true, "island": []}),
        );
        assert_eq!(
            weak,
            "packset: nothing to show in demo; no seed both scorers agreed on"
        );
        let quiet = super::island_empty_line(
            "demo",
            "ballots",
            &serde_json::json!({"seeds": 2, "weak": false, "island": []}),
        );
        assert_eq!(
            quiet,
            "packset: nothing to show in demo; activation did not spread"
        );
    }

    #[test]
    fn a_missing_daemon_names_the_packset_package() {
        let err = super::missing_daemon().to_string();
        assert!(err.contains("cargo build --release -p packset"), "{err}");
        assert!(!err.contains("packset-daemon"), "{err}");
        assert!(!err.contains("packset-cli"), "{err}");
    }
}
