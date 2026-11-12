//! Who may run, and who is told to come back.
//!
//! Reads, writes and maintenance wait in separate queues. A free slot goes
//! to a read before a write, and to a write before compaction. Inside a
//! queue the workspaces take turns, and the request that has waited longest
//! in the workspace that is up goes next. When a queue is more than half
//! full the newest request in that workspace goes instead: the one that has
//! been sitting there has usually missed the time its client allowed, and
//! running it would only throw the answer away.
//!
//! The number running at once starts at the ceiling. It shrinks when a
//! backlog is waiting and the requests that just finished took more than
//! twice as long as the ones that ran with nothing waiting, and it grows
//! back one at a time when they do not. A short queue never shrinks it,
//! so a single seat on a quiet machine runs as it did before. Pressure
//! scales that budget again. Reads keep a share until the machine is
//! stalled for everyone.
//!
//! A request names a deadline, as unix milliseconds. When the wait already
//! in its queue would pass that deadline, it is shed at once. It is not
//! left parked until the deadline expires.
//!
//! Linux writes the share of time tasks spent stalled for memory or CPU to
//! `/proc/pressure`. The files are absent off Linux, and then nothing is
//! shed for pressure. The ten-second share is smoothed with an
//! exponentially weighted moving average, and the mode latches with
//! hysteresis, so one sample does not flip it. A short spike also has to
//! show in the sixty-second share before the mode turns on. That is the
//! same shape as CockroachDB's admission control, which sheds background
//! work before foreground writes, and Redis `maxmemory-policy`, which
//! refuses one class of write and keeps another.
//!
//! Under that mode, maintenance (compaction, consolidate, sweep, reindex,
//! embed backfill) is paused, and a bulk write is answered 503. A small
//! append is admitted. It is written to a bounded append log first, so it
//! is durable, and answered 503 with `Retry-After` only when that log is
//! full. Shedding the small write would not free host memory: the stall
//! is coming from other processes.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How many searches, writes and maintenance jobs may run at once, together.
///
/// One worker stays out of that work when the pool has more than one, so
/// health and status still answer during a burst. A single worker cannot
/// split.
#[must_use]
pub fn slots(workers: usize) -> usize {
    match workers {
        0 | 1 => 1,
        n => n - 1,
    }
}

/// Parked bodies, of every class, stay within one request.
pub const PARKED_BODY_CAP: usize = 64 * 1024 * 1024;

/// Whether another body may wait. `queued` jobs of this class already wait,
/// out of `room`, and `parked` is the bytes every class is holding.
#[must_use]
pub fn park_body(parked: usize, body: usize, room: usize, queued: usize) -> bool {
    queued < room && parked.saturating_add(body) <= PARKED_BODY_CAP
}

/// Milliseconds since the unix epoch, for a deadline the client computed
/// the same way.
#[must_use]
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Whether `deadline_ms` is already past `now_ms`. No deadline never is.
#[must_use]
pub fn deadline_passed(deadline_ms: Option<u64>, now_ms: u64) -> bool {
    deadline_ms.is_some_and(|deadline| deadline <= now_ms)
}

/// A class of work. The order of the variants is the order a free slot is
/// offered in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Read,
    Write,
    Maintenance,
}

impl Class {
    fn index(self) -> usize {
        match self {
            Self::Read => 0,
            Self::Write => 1,
            Self::Maintenance => 2,
        }
    }

    /// The order a free slot is offered in.
    #[must_use]
    pub fn priority() -> [Self; 3] {
        [Self::Read, Self::Write, Self::Maintenance]
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Maintenance => "maintenance",
        }
    }
}

/// How many of each class may wait. Reads keep the whole `queue`; writes
/// and maintenance keep a fraction, so a burst of remembers cannot fill
/// the only room a search had.
#[must_use]
pub fn rooms(queue: usize) -> [usize; 3] {
    let queue = queue.max(1);
    [queue, (queue / 4).max(1), (queue / 8).max(1)]
}

/// How many of each class may run, and the total. Reads and writes may both
/// use every slot. A queued read is taken before a queued write, which is
/// what keeps a search from waiting out a line of remembers.
#[must_use]
pub fn ceilings(workers: usize) -> [usize; 4] {
    let total = slots(workers);
    [total, total, 1, total]
}

/// The share of time tasks spent stalled, in percent. The `10` fields are
/// the last ten seconds. The `60` fields are the last minute, and a mode
/// does not turn on from the ten-second share alone.
///
/// `memory_full` is the share in which every task was stalled for memory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Psi {
    pub memory_some: f64,
    pub memory_full: f64,
    pub cpu_some: f64,
    pub memory_some_60: f64,
    pub memory_full_60: f64,
}

/// A write at most this large is one append. Anything larger is bulk and
/// is shed while the host is under pressure.
pub const SMALL_APPEND_MAX: usize = 32 * 1024;

thread_local! {
    /// Set around a handler that was admitted as a small append under
    /// pressure. The service journals that write and skips embed backfill
    /// and reindex. Other threads are unaffected.
    static PRESSURE_APPEND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the write now running was admitted as a small append under
/// pressure.
#[must_use]
pub fn under_pressure() -> bool {
    PRESSURE_APPEND.with(std::cell::Cell::get)
}

/// Mark the current thread as running a journaled append.
pub fn enter_pressure() {
    PRESSURE_APPEND.with(|flag| flag.set(true));
}

/// Clear [`enter_pressure`].
pub fn leave_pressure() {
    PRESSURE_APPEND.with(|flag| flag.set(false));
}

/// Sets the pressure-append flag for this thread and clears it on drop.
pub struct PressureGuard;

impl PressureGuard {
    #[must_use]
    pub fn enter() -> Self {
        enter_pressure();
        Self
    }
}

impl Drop for PressureGuard {
    fn drop(&mut self) {
        leave_pressure();
    }
}

/// How much of a new sample the moving average takes. The rest is the
/// average so far. One sample of a spike moves the average by a fifth.
const EWMA_NEW: f64 = 0.2;

/// Pressure mode turns on at the high thresholds and off at the low ones.
/// The gap is the hysteresis. Maintenance and bulk writes follow this
/// mode. Small appends do not.
const ENTER_SOME: f64 = 25.0;
const EXIT_SOME: f64 = 12.0;
const ENTER_FULL: f64 = 8.0;
const EXIT_FULL: f64 = 3.0;
const ENTER_CPU: f64 = 80.0;
const EXIT_CPU: f64 = 50.0;
/// Reads stop only when every task has been stalled for most of the
/// smoothed interval. A full stall of 15% used to refuse every search.
const ENTER_READ_FULL: f64 = 60.0;
const EXIT_READ_FULL: f64 = 30.0;

impl Psi {
    #[must_use]
    pub fn quiet() -> Self {
        Self {
            memory_some: 0.0,
            memory_full: 0.0,
            cpu_some: 0.0,
            memory_some_60: 0.0,
            memory_full_60: 0.0,
        }
    }

    /// A steady reading: the minute share equals the ten-second share, which
    /// is what a test pins when it wants the mode that reading settles at.
    #[must_use]
    pub fn steady(memory_some: f64, memory_full: f64, cpu_some: f64) -> Self {
        Self {
            memory_some,
            memory_full,
            cpu_some,
            memory_some_60: memory_some,
            memory_full_60: memory_full,
        }
    }

    /// `memory` and `cpu` are the texts of the pressure files. A missing
    /// line is zero, which sheds nothing.
    #[must_use]
    pub fn parse(memory: &str, cpu: &str) -> Self {
        Self {
            memory_some: avg(memory, "some", "avg10"),
            memory_full: avg(memory, "full", "avg10"),
            cpu_some: avg(cpu, "some", "avg10"),
            memory_some_60: avg(memory, "some", "avg60"),
            memory_full_60: avg(memory, "full", "avg60"),
        }
    }
}

fn avg(text: &str, kind: &str, window: &str) -> f64 {
    let prefix = format!("{window}=");
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix(kind) else {
            continue;
        };
        if !rest.starts_with([' ', '\t']) {
            continue;
        }
        for field in rest.split_whitespace() {
            if let Some(value) = field.strip_prefix(&prefix) {
                return value.parse().unwrap_or(0.0);
            }
        }
    }
    0.0
}

fn ewma(prev: f64, sample: f64) -> f64 {
    sample.mul_add(EWMA_NEW, prev * (1.0 - EWMA_NEW))
}

/// The smoothed pressure and the latched modes.
#[derive(Debug, Clone, Copy)]
struct Smooth {
    some: f64,
    full: f64,
    cpu: f64,
    primed: bool,
    /// Bulk writes and maintenance are shed. Small appends are not.
    heavy: bool,
    /// Reads are shed. This latches later than [`Self::heavy`].
    reads: bool,
}

impl Smooth {
    fn quiet() -> Self {
        Self {
            some: 0.0,
            full: 0.0,
            cpu: 0.0,
            primed: false,
            heavy: false,
            reads: false,
        }
    }

    /// Jump to the steady state of `psi`. A pinned test reading does not
    /// wait out the average.
    #[cfg(test)]
    fn snap(&mut self, psi: Psi) {
        self.some = psi.memory_some;
        self.full = psi.memory_full;
        self.cpu = psi.cpu_some;
        self.primed = true;
        self.heavy = psi.memory_some >= ENTER_SOME
            || psi.memory_full >= ENTER_FULL
            || psi.cpu_some >= ENTER_CPU;
        self.reads = psi.memory_full >= ENTER_READ_FULL;
    }

    fn publish(&self, log: &LogUsage) {
        log.set_heavy(self.heavy);
    }

    /// Fold one sample in. The first sample seeds the average. Later
    /// samples move it by [`EWMA_NEW`]. The heavy mode turns on only when
    /// the average is past the high threshold and the minute share agrees,
    /// and it stays on until the average falls through the low threshold.
    fn observe(&mut self, psi: Psi) {
        if !self.primed {
            self.some = psi.memory_some;
            self.full = psi.memory_full;
            self.cpu = psi.cpu_some;
            self.primed = true;
        } else {
            self.some = ewma(self.some, psi.memory_some);
            self.full = ewma(self.full, psi.memory_full);
            self.cpu = ewma(self.cpu, psi.cpu_some);
        }
        let minute_some = psi.memory_some_60 >= ENTER_SOME * 0.5;
        let minute_full = psi.memory_full_60 >= ENTER_FULL * 0.5;
        let want_heavy = (self.some >= ENTER_SOME && minute_some)
            || (self.full >= ENTER_FULL && minute_full)
            || self.cpu >= ENTER_CPU;
        if self.heavy {
            if self.some < EXIT_SOME && self.full < EXIT_FULL && self.cpu < EXIT_CPU {
                self.heavy = false;
            }
        } else if want_heavy {
            self.heavy = true;
        }
        let want_reads = self.full >= ENTER_READ_FULL && psi.memory_full_60 >= EXIT_READ_FULL;
        if self.reads {
            if self.full < EXIT_READ_FULL {
                self.reads = false;
            }
        } else if want_reads {
            self.reads = true;
        }
    }
}

/// Bytes already in the append log, and its cap. The gate and the log
/// share one of these.
#[derive(Debug)]
pub struct LogUsage {
    pub used: std::sync::atomic::AtomicUsize,
    pub cap: std::sync::atomic::AtomicUsize,
    /// The latched pressure mode. The service keeps the append log while
    /// this is set, and folds it when it clears.
    pub heavy: std::sync::atomic::AtomicBool,
}

impl LogUsage {
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            used: std::sync::atomic::AtomicUsize::new(0),
            cap: std::sync::atomic::AtomicUsize::new(cap.max(1)),
            heavy: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[must_use]
    pub fn fits(&self, extra: usize) -> bool {
        self.used
            .load(std::sync::atomic::Ordering::Relaxed)
            .saturating_add(extra)
            <= self.cap.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn note(&self, used: usize) {
        self.used.store(used, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn set_heavy(&self, heavy: bool) {
        self.heavy
            .store(heavy, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Default for LogUsage {
    fn default() -> Self {
        Self::new(crate::journal::DEFAULT_CAP)
    }
}

/// The pressure files, or a quiet reading where they are not there.
#[must_use]
pub fn read_pressure() -> Psi {
    #[cfg(target_os = "linux")]
    {
        let memory = std::fs::read_to_string("/proc/pressure/memory").unwrap_or_default();
        let cpu = std::fs::read_to_string("/proc/pressure/cpu").unwrap_or_default();
        Psi::parse(&memory, &cpu)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Psi::quiet()
    }
}

/// The workspace a request names, or an empty string when it names none.
/// Two requests with the same workspace share a turn; ones that name none
/// share a turn with each other.
#[must_use]
pub fn workspace_of(target: &str, body: &[u8]) -> String {
    if let Some(found) = query_workspace(target) {
        return clip_seat(&found);
    }
    if let Some(found) = json_workspace(body) {
        return clip_seat(&found);
    }
    String::new()
}

fn clip_seat(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect()
}

fn query_workspace(target: &str) -> Option<String> {
    let (_, query) = target.split_once('?')?;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if decode(key) == "workspace" && !value.is_empty() {
            return Some(decode(value));
        }
    }
    None
}

fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `"workspace":"..."` in the first part of a body. A transcript that
/// buries the name past this window shares the unnamed turn.
fn json_workspace(body: &[u8]) -> Option<String> {
    let window = body.len().min(8192);
    let text = String::from_utf8_lossy(&body[..window]);
    let key = "\"workspace\"";
    let at = text.find(key)?;
    let rest = text[at + key.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '"' => return Some(out),
            c => out.push(c),
        }
        if out.len() > 128 {
            return None;
        }
    }
    None
}

/// How many requests of one class may run. `current` starts at `ceiling`
/// and moves only while a queue is waiting.
#[derive(Debug, Clone)]
pub struct Limiter {
    ceiling: usize,
    current: usize,
    long_us: u64,
    short_us: u64,
    samples: u32,
    calm: u32,
}

impl Limiter {
    #[must_use]
    pub fn new(ceiling: usize) -> Self {
        let ceiling = ceiling.max(1);
        Self {
            ceiling,
            current: ceiling,
            long_us: 0,
            short_us: 0,
            samples: 0,
            calm: 0,
        }
    }

    #[must_use]
    pub fn current(&self) -> usize {
        self.current
    }

    #[must_use]
    pub fn ceiling(&self) -> usize {
        self.ceiling
    }

    /// How long a request of this class has been taking, in microseconds.
    /// Zero until one has finished.
    #[must_use]
    pub fn latency_us(&self) -> u64 {
        self.short_us
    }

    /// `sample` is how long one request took. `queued` is whether more of
    /// this class were already waiting. The first samples only set the
    /// baseline.
    pub fn observe(&mut self, sample: Duration, queued: bool) {
        let capped = sample.min(Duration::from_secs(10));
        let sample = u64::try_from(capped.as_micros()).unwrap_or(u64::MAX);
        if self.samples == 0 {
            self.long_us = sample;
            self.short_us = sample;
            self.samples = 1;
            return;
        }
        self.short_us = blend(self.short_us, sample, 1, 5);
        self.long_us = blend(self.long_us, sample, 1, 20);
        self.samples = self.samples.saturating_add(1);
        if self.samples < 8 {
            return;
        }
        let slow = self.short_us > self.long_us.saturating_mul(2)
            && self.short_us > self.long_us.saturating_add(5_000);
        if queued && slow {
            self.current = ((self.current * 3) / 4).max(1);
            self.calm = 0;
        } else if !slow {
            self.calm = self.calm.saturating_add(1);
            if self.calm >= 8 {
                self.current = (self.current + 1).min(self.ceiling);
                self.calm = 0;
            }
        }
    }
}

/// `prev * (den - num) / den + sample * num / den`, without overflowing.
fn blend(prev: u64, sample: u64, num: u64, den: u64) -> u64 {
    let keep = den - num;
    let head = prev / den * keep + sample / den * num;
    let tail = (prev % den).saturating_mul(keep) + (sample % den).saturating_mul(num);
    head + tail / den
}

/// A request the caller wants admitted.
pub struct Arrival<T> {
    pub class: Class,
    pub seat: String,
    pub deadline_ms: Option<u64>,
    pub bytes: usize,
    pub body: T,
}

/// What to do with one arrival.
pub enum Decision<T> {
    /// Run it now. The slot is held until [`Control::finish`].
    /// `pressure_append` is a small write admitted under pressure: the
    /// handler journals it, and does not start a reindex or an embed.
    Run {
        class: Class,
        body: T,
        pressure_append: bool,
    },
    /// It is waiting. A later [`Control::take`] runs it.
    Queued,
    /// Answer 503. Nothing ran.
    Shed {
        body: T,
        retry_after: u64,
        reason: &'static str,
    },
    /// The deadline passed. Answer 408. Nothing ran.
    Expired(T),
}

/// The next queued request, after a slot was given back.
pub enum Next<T> {
    Run {
        class: Class,
        body: T,
        pressure_append: bool,
    },
    Expired(T),
    Idle,
}

struct Waiting<T> {
    deadline_ms: Option<u64>,
    bytes: usize,
    /// This write is a small append admitted under pressure. The handler
    /// journals it before it runs.
    pressure_append: bool,
    body: T,
}

struct Lane<T> {
    room: usize,
    len: usize,
    seats: HashMap<String, VecDeque<Waiting<T>>>,
    order: VecDeque<String>,
}

impl<T> Lane<T> {
    fn new(room: usize) -> Self {
        Self {
            room: room.max(1),
            len: 0,
            seats: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn push(&mut self, seat: String, job: Waiting<T>) {
        let fresh = self.seats.get(&seat).is_none_or(VecDeque::is_empty);
        self.seats.entry(seat.clone()).or_default().push_back(job);
        if fresh {
            self.order.push_back(seat);
        }
        self.len += 1;
    }

    fn pop(&mut self, lifo: bool) -> Option<Waiting<T>> {
        loop {
            let seat = self.order.pop_front()?;
            let Some(queue) = self.seats.get_mut(&seat) else {
                continue;
            };
            let job = if lifo {
                queue.pop_back()
            } else {
                queue.pop_front()
            };
            if queue.is_empty() {
                self.seats.remove(&seat);
            } else {
                self.order.push_back(seat);
            }
            if job.is_some() {
                self.len = self.len.saturating_sub(1);
            }
            if let Some(job) = job {
                return Some(job);
            }
        }
    }
}

/// The queues, the slots, and the counts a `/stats` answer is made of.
pub struct Control<T> {
    lanes: [Lane<T>; 3],
    in_flight: [usize; 3],
    limiters: [Limiter; 3],
    total_in_flight: usize,
    total_limit: usize,
    parked_bytes: usize,
    sheds: [u64; 3],
    pressure_sheds: u64,
    deadline_drops: u64,
    accept_sheds: u64,
    pressure: Psi,
    smooth: Smooth,
    log: std::sync::Arc<LogUsage>,
    pressure_pinned: bool,
    pressure_at: std::time::Instant,
    pressure_seen: bool,
    logged_pressure: bool,
}

impl<T> Control<T> {
    #[must_use]
    pub fn new(workers: usize, queue: usize) -> Self {
        let room = rooms(queue);
        let caps = ceilings(workers);
        Self {
            lanes: [Lane::new(room[0]), Lane::new(room[1]), Lane::new(room[2])],
            in_flight: [0, 0, 0],
            limiters: [
                Limiter::new(caps[0]),
                Limiter::new(caps[1]),
                Limiter::new(caps[2]),
            ],
            total_in_flight: 0,
            total_limit: caps[3],
            parked_bytes: 0,
            sheds: [0, 0, 0],
            pressure_sheds: 0,
            deadline_drops: 0,
            accept_sheds: 0,
            pressure: Psi::quiet(),
            smooth: Smooth::quiet(),
            log: std::sync::Arc::new(LogUsage::default()),
            pressure_pinned: false,
            pressure_at: std::time::Instant::now(),
            pressure_seen: false,
            logged_pressure: false,
        }
    }

    /// Share `log` with the append log, so a full log is visible here.
    pub fn watch_log(&mut self, log: std::sync::Arc<LogUsage>) {
        self.log = log;
    }

    /// The log this gate consults.
    #[must_use]
    pub fn log(&self) -> std::sync::Arc<LogUsage> {
        std::sync::Arc::clone(&self.log)
    }

    /// Use `psi` and do not read the pressure files. Tests pin a quiet
    /// reading so a busy host does not change the result. The mode jumps
    /// to the steady state of `psi`.
    #[cfg(test)]
    pub fn pin_pressure(&mut self, psi: Psi) {
        self.pressure = psi;
        self.smooth.snap(psi);
        self.pressure_pinned = true;
        self.pressure_seen = true;
        self.publish_mode();
    }

    /// Fold one pressure sample, from a quiet start. One spike does not
    /// latch the mode. Tests use this; the server uses [`Self::refresh_pressure`].
    #[cfg(test)]
    pub fn observe_pressure(&mut self, psi: Psi) {
        self.pressure = psi;
        let was = self.smooth.heavy;
        self.smooth.observe(psi);
        self.pressure_pinned = true;
        self.pressure_seen = true;
        self.note_transition(was);
    }

    pub fn note_accept_shed(&mut self) {
        self.accept_sheds = self.accept_sheds.saturating_add(1);
    }

    pub fn arrive(&mut self, arrival: Arrival<T>, now_ms: u64) -> Decision<T> {
        self.refresh_pressure();
        if deadline_passed(arrival.deadline_ms, now_ms) {
            self.deadline_drops = self.deadline_drops.saturating_add(1);
            return Decision::Expired(arrival.body);
        }
        let class = arrival.class;
        let pressure_append = self.pressure_append(class, arrival.bytes);
        if let Some(reason) = self.pressure_refusal(class, arrival.bytes) {
            self.note_shed(class, true);
            return Decision::Shed {
                body: arrival.body,
                retry_after: self.retry_after(class),
                reason,
            };
        }
        if self.can_run(class) {
            self.acquire(class);
            return Decision::Run {
                class,
                body: arrival.body,
                pressure_append,
            };
        }
        if self.misses_deadline(class, arrival.deadline_ms, now_ms) {
            self.note_shed(class, false);
            return Decision::Shed {
                body: arrival.body,
                retry_after: self.retry_after(class),
                reason: "the deadline cannot be met; try again",
            };
        }
        if park_body(
            self.parked_bytes,
            arrival.bytes,
            self.lanes[class.index()].room,
            self.lanes[class.index()].len,
        ) {
            self.parked_bytes = self.parked_bytes.saturating_add(arrival.bytes);
            self.lanes[class.index()].push(
                arrival.seat,
                Waiting {
                    deadline_ms: arrival.deadline_ms,
                    bytes: arrival.bytes,
                    pressure_append,
                    body: arrival.body,
                },
            );
            return Decision::Queued;
        }
        self.note_shed(class, false);
        Decision::Shed {
            body: arrival.body,
            retry_after: self.retry_after(class),
            reason: queue_reason(class),
        }
    }

    /// The slot `class` was holding is free. `sample` is how long the
    /// request took, when it ran.
    pub fn finish(&mut self, class: Class, sample: Option<Duration>) {
        self.release(class);
        if let Some(sample) = sample {
            // One request waiting is a laptop with a few clients, which is
            // where the limit already was. Shrink only for a real backlog.
            let backlog = self.lanes[class.index()].len > self.limiters[class.index()].current();
            self.limiters[class.index()].observe(sample, backlog);
        }
    }

    pub fn take(&mut self, now_ms: u64) -> Next<T> {
        self.refresh_pressure();
        for class in Class::priority() {
            if !self.can_run(class) {
                continue;
            }
            let lifo = self.lifo(class);
            if let Some(job) = self.lanes[class.index()].pop(lifo) {
                self.parked_bytes = self.parked_bytes.saturating_sub(job.bytes);
                if deadline_passed(job.deadline_ms, now_ms) {
                    self.deadline_drops = self.deadline_drops.saturating_add(1);
                    return Next::Expired(job.body);
                }
                self.acquire(class);
                return Next::Run {
                    class,
                    body: job.body,
                    pressure_append: job.pressure_append,
                };
            }
        }
        Next::Idle
    }

    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let paused = self.smooth.heavy;
        let shedding_writes = self.smooth.heavy;
        let busy =
            self.lanes.iter().any(|lane| lane.len > 0) || shedding_writes || self.smooth.reads;
        Snapshot {
            read_depth: self.lanes[0].len,
            read_in_flight: self.in_flight[0],
            read_limit: self.limiters[0].current(),
            read_budget: self.budget(Class::Read),
            read_latency_us: self.limiters[0].latency_us(),
            read_shed: self.sheds[0],
            write_depth: self.lanes[1].len,
            write_in_flight: self.in_flight[1],
            write_limit: self.limiters[1].current(),
            write_budget: self.budget(Class::Write),
            write_latency_us: self.limiters[1].latency_us(),
            write_shed: self.sheds[1],
            maint_depth: self.lanes[2].len,
            maint_in_flight: self.in_flight[2],
            maint_limit: self.limiters[2].current(),
            maint_budget: self.budget(Class::Maintenance),
            maint_latency_us: self.limiters[2].latency_us(),
            maint_shed: self.sheds[2],
            maint_paused: paused,
            total_limit: self.total_limit,
            deadline_drops: self.deadline_drops,
            pressure_sheds: self.pressure_sheds,
            accept_sheds: self.accept_sheds,
            memory_some: self.smooth.some,
            memory_full: self.smooth.full,
            cpu_some: self.smooth.cpu,
            append_log_bytes: self.log.used.load(std::sync::atomic::Ordering::Relaxed),
            append_log_cap: self.log.cap.load(std::sync::atomic::Ordering::Relaxed),
            shedding_writes,
            busy,
        }
    }

    /// Slots `class` may hold right now. Zero when that class is shed.
    /// Writes keep their slots under pressure: a small append still runs,
    /// and a bulk write was already refused.
    fn budget(&self, class: Class) -> usize {
        if class == Class::Maintenance && self.smooth.heavy {
            return 0;
        }
        if class == Class::Read && self.smooth.reads {
            return 0;
        }
        let current = self.limiters[class.index()].current().max(1);
        if class == Class::Read && self.smooth.full > 0.0 {
            let scaled = (current as f64
                * (1.0 - (self.smooth.full / ENTER_READ_FULL).clamp(0.0, 1.0)))
            .round() as usize;
            return scaled.clamp(1, current);
        }
        current
    }

    /// A small write under pressure is journaled and run. Anything else
    /// this mode refuses is named here.
    fn pressure_refusal(&self, class: Class, bytes: usize) -> Option<&'static str> {
        if class == Class::Maintenance && self.smooth.heavy {
            return Some("maintenance is paused; try again");
        }
        if class == Class::Read && self.smooth.reads {
            return Some("memory pressure; try again");
        }
        if class == Class::Write && self.smooth.heavy {
            if bytes > SMALL_APPEND_MAX {
                return Some("memory pressure; a large write is shed");
            }
            if !self.log.fits(bytes) {
                return Some("the append log is full; try again");
            }
        }
        None
    }

    /// Whether `class` is a small append that must be journaled.
    fn pressure_append(&self, class: Class, bytes: usize) -> bool {
        class == Class::Write
            && self.smooth.heavy
            && bytes <= SMALL_APPEND_MAX
            && self.log.fits(bytes)
    }

    /// Whether parking `class` would start it only after `deadline_ms`.
    /// No measurement yet is not a reason to refuse: the wait is unknown.
    fn misses_deadline(&self, class: Class, deadline_ms: Option<u64>, now_ms: u64) -> bool {
        let Some(deadline) = deadline_ms else {
            return false;
        };
        let wait = self.estimate_wait_ms(class);
        wait > deadline.saturating_sub(now_ms)
    }

    /// How long a request that cannot run now would sit, from the recent
    /// service time and the work already ahead of it.
    fn estimate_wait_ms(&self, class: Class) -> u64 {
        let service_us = self.limiters[class.index()].latency_us();
        if service_us == 0 {
            return 0;
        }
        let service_ms = (service_us / 1000).max(1);
        let slots = (self.budget(class).max(1)) as u64;
        let waiting = self.lanes[class.index()].len as u64;
        let occupied = u64::from(
            self.in_flight[class.index()] > 0 || self.total_in_flight >= self.total_limit,
        );
        waiting.saturating_add(occupied).saturating_mul(service_ms) / slots
    }

    fn can_run(&self, class: Class) -> bool {
        let budget = self.budget(class);
        if budget == 0 {
            return false;
        }
        if self.total_in_flight >= self.total_limit {
            return false;
        }
        if self.in_flight[class.index()] >= budget {
            return false;
        }
        if class == Class::Maintenance
            && (self.total_in_flight > 0 || self.lanes[0].len > 0 || self.lanes[1].len > 0)
        {
            return false;
        }
        if class != Class::Read && self.lanes[0].len > 0 && self.total_limit > 1 {
            let free = self.total_limit - self.total_in_flight;
            if free <= 1 {
                return false;
            }
        }
        true
    }

    fn lifo(&self, class: Class) -> bool {
        let lane = &self.lanes[class.index()];
        lane.len * 2 > lane.room
            || self.smooth.heavy
            || self.limiters[class.index()].current() < self.limiters[class.index()].ceiling()
    }

    fn acquire(&mut self, class: Class) {
        self.in_flight[class.index()] += 1;
        self.total_in_flight += 1;
    }

    fn release(&mut self, class: Class) {
        self.in_flight[class.index()] = self.in_flight[class.index()].saturating_sub(1);
        self.total_in_flight = self.total_in_flight.saturating_sub(1);
    }

    fn note_shed(&mut self, class: Class, pressure: bool) {
        self.sheds[class.index()] = self.sheds[class.index()].saturating_add(1);
        if pressure {
            self.pressure_sheds = self.pressure_sheds.saturating_add(1);
        }
    }

    fn retry_after(&self, class: Class) -> u64 {
        let limiter = &self.limiters[class.index()];
        if limiter.samples < 4 || limiter.short_us == 0 {
            return 1;
        }
        let depth = self.lanes[class.index()].len.max(1) as u64;
        let millis = limiter.short_us / 1000;
        let secs = depth.saturating_mul(millis.max(1)) / 1000;
        secs.clamp(1, 5)
    }

    fn refresh_pressure(&mut self) {
        if self.pressure_pinned {
            return;
        }
        if self.pressure_seen && self.pressure_at.elapsed() < Duration::from_millis(200) {
            return;
        }
        let next = read_pressure();
        let was = self.smooth.heavy;
        self.pressure = next;
        self.smooth.observe(next);
        self.pressure_at = std::time::Instant::now();
        self.pressure_seen = true;
        self.note_transition(was);
    }

    fn note_transition(&mut self, was_heavy: bool) {
        if self.smooth.heavy && !was_heavy && !self.logged_pressure {
            eprintln!(
                "packsetd: memory pressure; maintenance and large writes are shed, small appends are journaled"
            );
            self.logged_pressure = true;
        }
        if !self.smooth.heavy {
            self.logged_pressure = false;
        }
        self.publish_mode();
    }

    fn publish_mode(&self) {
        self.smooth.publish(&self.log);
    }
}

fn queue_reason(class: Class) -> &'static str {
    match class {
        Class::Read => "the read queue is full; try again",
        Class::Write => "the write queue is full; try again",
        Class::Maintenance => "the maintenance queue is full; try again",
    }
}

/// The numbers `/stats` reports.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub read_depth: usize,
    pub read_in_flight: usize,
    pub read_limit: usize,
    /// Slots a read may hold after pressure is applied.
    pub read_budget: usize,
    /// Recent read time, in microseconds. Zero until one has finished.
    pub read_latency_us: u64,
    pub read_shed: u64,
    pub write_depth: usize,
    pub write_in_flight: usize,
    pub write_limit: usize,
    pub write_budget: usize,
    pub write_latency_us: u64,
    pub write_shed: u64,
    pub maint_depth: usize,
    pub maint_in_flight: usize,
    pub maint_limit: usize,
    pub maint_budget: usize,
    pub maint_latency_us: u64,
    pub maint_shed: u64,
    pub maint_paused: bool,
    pub total_limit: usize,
    pub deadline_drops: u64,
    pub pressure_sheds: u64,
    pub accept_sheds: u64,
    pub memory_some: f64,
    pub memory_full: f64,
    pub cpu_some: f64,
    /// Bulk writes are shed. Small appends still run.
    pub shedding_writes: bool,
    pub busy: bool,
    pub append_log_bytes: usize,
    pub append_log_cap: usize,
}

impl Snapshot {
    /// `ok` at rest, and `busy, retrying` when work is waiting or writes
    /// are being turned away.
    #[must_use]
    pub fn line(&self) -> &'static str {
        if self.busy {
            "busy, retrying"
        } else {
            "ok"
        }
    }

    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "line": self.line(),
            "busy": self.busy,
            "queues": {
                "read": {
                    "depth": self.read_depth,
                    "in_flight": self.read_in_flight,
                    "limit": self.read_limit,
                    "budget": self.read_budget,
                    "latency_us": self.read_latency_us,
                    "shed": self.read_shed,
                },
                "write": {
                    "depth": self.write_depth,
                    "in_flight": self.write_in_flight,
                    "limit": self.write_limit,
                    "budget": self.write_budget,
                    "latency_us": self.write_latency_us,
                    "shed": self.write_shed,
                },
                "maintenance": {
                    "depth": self.maint_depth,
                    "in_flight": self.maint_in_flight,
                    "limit": self.maint_limit,
                    "budget": self.maint_budget,
                    "latency_us": self.maint_latency_us,
                    "shed": self.maint_shed,
                    "paused": self.maint_paused,
                },
            },
            "limits": {
                "read": self.read_limit,
                "write": self.write_limit,
                "maintenance": self.maint_limit,
                "total": self.total_limit,
            },
            "shed": {
                "read": self.read_shed,
                "write": self.write_shed,
                "maintenance": self.maint_shed,
                "deadline": self.deadline_drops,
                "pressure": self.pressure_sheds,
                "accept": self.accept_sheds,
            },
            "pressure": {
                "memory_some": self.memory_some,
                "memory_full": self.memory_full,
                "cpu_some": self.cpu_some,
                "shedding_writes": self.shedding_writes,
            },
            "append_log": {
                "bytes": self.append_log_bytes,
                "cap": self.append_log_cap,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn quiet(workers: usize, queue: usize) -> Control<u32> {
        let mut control = Control::new(workers, queue);
        control.pin_pressure(Psi::quiet());
        control
    }

    fn job(class: Class, seat: &str, body: u32) -> Arrival<u32> {
        Arrival {
            class,
            seat: seat.to_string(),
            deadline_ms: None,
            bytes: 0,
            body,
        }
    }

    fn run_body(decision: Decision<u32>) -> u32 {
        match decision {
            Decision::Run { body, .. } => body,
            other => panic!("expected the request to run, got {}", label(&other)),
        }
    }

    fn label(decision: &Decision<u32>) -> &'static str {
        match decision {
            Decision::Run { .. } => "run",
            Decision::Queued => "queued",
            Decision::Shed { .. } => "shed",
            Decision::Expired(_) => "expired",
        }
    }

    #[test]
    fn a_quiet_seat_runs_at_once_and_the_limit_stays_up() {
        let mut control = quiet(4, 128);
        let ceiling = control.limiters[0].ceiling();
        assert_eq!(ceiling, 3);
        for i in 0..20 {
            let body = run_body(control.arrive(job(Class::Read, "seat", i), NOW));
            assert_eq!(body, i);
            control.finish(Class::Read, Some(Duration::from_millis(5)));
        }
        let snap = control.snapshot();
        assert!(!snap.busy);
        assert_eq!(snap.line(), "ok");
        assert_eq!(snap.read_shed, 0);
        assert_eq!(snap.read_limit, ceiling);
        assert_eq!(snap.read_depth, 0);
        assert_eq!(snap.read_in_flight, 0);
    }

    #[test]
    fn a_write_is_shed_and_a_queued_read_goes_next() {
        let mut control = quiet(4, 4);
        for i in 1..=3 {
            assert!(
                matches!(
                    control.arrive(job(Class::Write, "a", i), NOW),
                    Decision::Run { .. }
                ),
                "write {i} did not run"
            );
        }
        assert!(matches!(
            control.arrive(job(Class::Write, "a", 4), NOW),
            Decision::Queued
        ));
        match control.arrive(job(Class::Write, "a", 5), NOW) {
            Decision::Shed {
                body,
                retry_after,
                reason,
            } => {
                assert_eq!(body, 5);
                assert_eq!(retry_after, 1);
                assert!(reason.contains("write"));
            }
            other => panic!("{}", label(&other)),
        }
        assert!(matches!(
            control.arrive(job(Class::Read, "b", 6), NOW),
            Decision::Queued
        ));
        control.finish(Class::Write, Some(Duration::from_millis(1)));
        match control.take(NOW) {
            Next::Run { class, body, .. } => {
                assert_eq!(class, Class::Read);
                assert_eq!(body, 6);
            }
            Next::Expired(_) => panic!("the read had no deadline"),
            Next::Idle => panic!("the read was not waiting"),
        }
        let snap = control.snapshot();
        assert!(snap.busy);
        assert_eq!(snap.line(), "busy, retrying");
        assert_eq!(snap.write_shed, 1);
        assert_eq!(snap.read_shed, 0);
        assert!(snap.read_in_flight + snap.write_in_flight <= snap.total_limit);
    }

    #[test]
    fn one_workspace_does_not_take_every_turn() {
        let mut control = quiet(2, 32);
        assert_eq!(
            run_body(control.arrive(job(Class::Read, "noisy", 1), NOW)),
            1
        );
        for i in 2..=5 {
            assert!(matches!(
                control.arrive(job(Class::Read, "noisy", i), NOW),
                Decision::Queued
            ));
        }
        assert!(matches!(
            control.arrive(job(Class::Read, "quiet", 9), NOW),
            Decision::Queued
        ));
        control.finish(Class::Read, Some(Duration::from_millis(1)));
        let Next::Run { body, .. } = control.take(NOW) else {
            panic!("nothing was waiting");
        };
        assert_eq!(body, 2, "the workspace that was already waiting goes first");
        control.finish(Class::Read, Some(Duration::from_millis(1)));
        let Next::Run { body, .. } = control.take(NOW) else {
            panic!("the other workspace was not waiting");
        };
        assert_eq!(body, 9, "the quiet workspace waited behind every noisy one");
    }

    #[test]
    fn a_deep_queue_runs_the_newest_request() {
        let mut control = quiet(2, 4);
        assert_eq!(
            run_body(control.arrive(job(Class::Read, "seat", 1), NOW)),
            1
        );
        for i in 2..=4 {
            assert!(matches!(
                control.arrive(job(Class::Read, "seat", i), NOW),
                Decision::Queued
            ));
        }
        control.finish(Class::Read, Some(Duration::from_millis(1)));
        let Next::Run { body, .. } = control.take(NOW) else {
            panic!("the queue was empty");
        };
        assert_eq!(body, 4);
    }

    #[test]
    fn maintenance_waits_until_the_other_queues_are_empty() {
        let mut control = quiet(4, 8);
        assert!(matches!(
            control.arrive(job(Class::Read, "seat", 1), NOW),
            Decision::Run { .. }
        ));
        assert!(matches!(
            control.arrive(job(Class::Maintenance, "seat", 2), NOW),
            Decision::Queued
        ));
        control.finish(Class::Read, Some(Duration::from_millis(1)));
        let Next::Run { class, body, .. } = control.take(NOW) else {
            panic!("maintenance was dropped");
        };
        assert_eq!(class, Class::Maintenance);
        assert_eq!(body, 2);
    }

    #[test]
    fn a_passed_deadline_is_not_run() {
        let mut control = quiet(2, 8);
        let mut late = job(Class::Write, "seat", 7);
        late.deadline_ms = Some(NOW - 1);
        assert!(matches!(control.arrive(late, NOW), Decision::Expired(7)));
        assert_eq!(control.snapshot().deadline_drops, 1);
        assert_eq!(
            run_body(control.arrive(job(Class::Read, "seat", 1), NOW)),
            1
        );
        let mut queued = job(Class::Read, "seat", 2);
        queued.deadline_ms = Some(NOW + 10);
        assert!(matches!(control.arrive(queued, NOW), Decision::Queued));
        control.finish(Class::Read, None);
        assert!(matches!(control.take(NOW + 11), Next::Expired(2)));
        assert_eq!(control.snapshot().deadline_drops, 2);
        assert!(matches!(control.take(NOW + 11), Next::Idle));
    }

    fn bulk(class: Class, seat: &str, body: u32) -> Arrival<u32> {
        let mut arrival = job(class, seat, body);
        arrival.bytes = SMALL_APPEND_MAX + 1;
        arrival
    }

    #[test]
    fn memory_pressure_sheds_maintenance_and_bulk_writes_and_keeps_small_appends() {
        let mut control = quiet(4, 8);
        // Below the enter thresholds the mode stays off, maintenance included.
        control.pin_pressure(Psi::steady(12.0, 1.0, 0.0));
        assert!(matches!(
            control.arrive(job(Class::Maintenance, "seat", 1), NOW),
            Decision::Run { .. }
        ));
        control.finish(Class::Maintenance, None);
        assert!(matches!(
            control.arrive(job(Class::Write, "seat", 2), NOW),
            Decision::Run {
                pressure_append: false,
                ..
            }
        ));
        control.finish(Class::Write, None);

        // Steady pressure. A lesson still runs and is marked for the log.
        // A bulk body and maintenance are shed. Reads keep a slot.
        control.pin_pressure(Psi::steady(30.0, 10.0, 0.0));
        match control.arrive(job(Class::Write, "seat", 3), NOW) {
            Decision::Run {
                pressure_append: true,
                ..
            } => {}
            other => panic!("{}", label(&other)),
        }
        control.finish(Class::Write, None);
        match control.arrive(bulk(Class::Write, "seat", 4), NOW) {
            Decision::Shed {
                retry_after,
                reason,
                ..
            } => {
                assert_eq!(retry_after, 1);
                assert!(reason.contains("large"), "{reason}");
            }
            other => panic!("{}", label(&other)),
        }
        assert!(matches!(
            control.arrive(job(Class::Maintenance, "seat", 5), NOW),
            Decision::Shed { .. }
        ));
        assert!(matches!(
            control.arrive(job(Class::Read, "seat", 6), NOW),
            Decision::Run { .. }
        ));
        control.finish(Class::Read, None);
        let pressed = control.snapshot();
        assert!(pressed.read_budget >= 1, "reads had no slot");
        assert!(pressed.write_budget >= 1, "small appends had no slot");
        assert_eq!(pressed.maint_budget, 0);
        assert!(pressed.shedding_writes);
        assert!(pressed.maint_paused);

        // The log is full: the next small append is 503, and nothing ran.
        control
            .log
            .note(control.log.cap.load(std::sync::atomic::Ordering::Relaxed));
        let mut small = job(Class::Write, "seat", 7);
        small.bytes = 64;
        match control.arrive(small, NOW) {
            Decision::Shed { reason, .. } => {
                assert!(reason.contains("append log"), "{reason}");
            }
            other => panic!("{}", label(&other)),
        }
        control.log.note(0);

        // Reads stop only when the machine is stalled for everyone.
        control.pin_pressure(Psi::steady(30.0, 80.0, 0.0));
        assert!(matches!(
            control.arrive(job(Class::Read, "seat", 9), NOW),
            Decision::Shed { .. }
        ));
        let snap = control.snapshot();
        assert!(snap.pressure_sheds >= 3);
        assert!(snap.busy);
        assert!(snap.shedding_writes);
        assert_eq!(snap.read_budget, 0);
    }

    #[test]
    fn one_pressure_spike_does_not_flip_the_mode_and_a_sustained_reading_does() {
        let mut control = quiet(2, 4);
        // A single ten-second spike with a quiet minute stays off.
        control.observe_pressure(Psi {
            memory_some: 80.0,
            memory_full: 40.0,
            cpu_some: 0.0,
            memory_some_60: 1.0,
            memory_full_60: 0.5,
        });
        assert!(!control.snapshot().shedding_writes);
        assert!(matches!(
            control.arrive(bulk(Class::Write, "seat", 1), NOW),
            Decision::Run { .. }
        ));
        control.finish(Class::Write, None);

        // The same stall held for the minute latches, and stays latched
        // while the average is still above the exit threshold.
        for _ in 0..6 {
            control.observe_pressure(Psi::steady(40.0, 20.0, 0.0));
        }
        assert!(control.snapshot().shedding_writes);
        assert!(matches!(
            control.arrive(bulk(Class::Write, "seat", 2), NOW),
            Decision::Shed { .. }
        ));
        // One quiet sample does not leave the mode.
        control.observe_pressure(Psi::steady(0.0, 0.0, 0.0));
        assert!(
            control.snapshot().shedding_writes,
            "one quiet sample cleared pressure"
        );
        for _ in 0..30 {
            control.observe_pressure(Psi::steady(0.0, 0.0, 0.0));
        }
        assert!(!control.snapshot().shedding_writes);
    }

    #[test]
    fn reads_are_taken_before_writes_under_contention() {
        let mut control = quiet(2, 32);
        assert!(matches!(
            control.arrive(job(Class::Write, "seat", 1), NOW),
            Decision::Run { .. }
        ));
        for body in [2, 3, 4] {
            assert!(
                matches!(
                    control.arrive(job(Class::Write, "seat", body), NOW),
                    Decision::Queued
                ),
                "write {body} was not queued"
            );
        }
        for body in [10, 11] {
            assert!(
                matches!(
                    control.arrive(job(Class::Read, "seat", body), NOW),
                    Decision::Queued
                ),
                "read {body} was not queued"
            );
        }
        control.finish(Class::Write, Some(Duration::from_millis(1)));
        for expected in [10, 11] {
            match control.take(NOW) {
                Next::Run { class, body, .. } => {
                    assert_eq!(class, Class::Read);
                    assert_eq!(body, expected);
                    control.finish(Class::Read, Some(Duration::from_millis(1)));
                }
                Next::Expired(_) => panic!("the read had no deadline"),
                Next::Idle => panic!("a read was still waiting"),
            }
        }
        match control.take(NOW) {
            Next::Run { class, body, .. } => {
                assert_eq!(class, Class::Write);
                assert_eq!(body, 2);
            }
            Next::Expired(_) => panic!("the write had no deadline"),
            Next::Idle => panic!("the write was still waiting"),
        }
    }

    #[test]
    fn a_deadline_the_queue_cannot_meet_is_shed_at_once() {
        let mut control = quiet(2, 32);
        assert!(matches!(
            control.arrive(job(Class::Read, "seat", 1), NOW),
            Decision::Run { .. }
        ));
        control.finish(Class::Read, Some(Duration::from_millis(200)));
        assert!(matches!(
            control.arrive(job(Class::Read, "seat", 2), NOW),
            Decision::Run { .. }
        ));
        let mut late = job(Class::Read, "seat", 3);
        late.deadline_ms = Some(NOW + 50);
        match control.arrive(late, NOW) {
            Decision::Shed {
                body,
                retry_after,
                reason,
            } => {
                assert_eq!(body, 3);
                assert!(retry_after >= 1);
                assert!(reason.contains("deadline"), "{reason}");
            }
            other => panic!("{}", label(&other)),
        }
        assert_eq!(control.snapshot().read_depth, 0, "the late read was queued");
        assert_eq!(control.snapshot().read_shed, 1);
        assert!(control.snapshot().read_latency_us >= 200_000);
    }

    #[test]
    fn a_slow_queue_lowers_the_limit_and_an_empty_one_does_not() {
        let mut limiter = Limiter::new(4);
        for _ in 0..30 {
            limiter.observe(Duration::from_millis(10), false);
        }
        assert_eq!(limiter.current(), 4);
        for _ in 0..12 {
            limiter.observe(Duration::from_millis(100), true);
        }
        assert!(limiter.current() < 4, "limit stayed {}", limiter.current());
        assert!(limiter.current() >= 1);

        let mut calm = Limiter::new(4);
        for _ in 0..8 {
            calm.observe(Duration::from_millis(10), false);
        }
        for _ in 0..30 {
            calm.observe(Duration::from_millis(500), false);
        }
        assert_eq!(calm.current(), 4, "an empty queue shrank the limit");
    }

    #[test]
    fn pressure_text_is_the_ten_second_share() {
        let memory = "some avg10=12.50 avg60=1.00 avg300=0.10 total=12345\n\
                      full avg10=2.00 avg60=0.00 avg300=0.00 total=99\n";
        let cpu = "some avg10=3.25 avg60=0.00 avg300=0.00 total=4\n\
                   full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        let psi = Psi::parse(memory, cpu);
        assert!((psi.memory_some - 12.5).abs() < 0.001);
        assert!((psi.memory_full - 2.0).abs() < 0.001);
        assert!((psi.cpu_some - 3.25).abs() < 0.001);
        assert!((psi.memory_some_60 - 1.0).abs() < 0.001);
        assert!(psi.memory_full_60.abs() < 0.001);
        let quiet = Psi::parse("", "");
        assert_eq!(quiet.memory_some, 0.0);
        assert_eq!(quiet.memory_full_60, 0.0);
    }

    #[test]
    fn the_workspace_comes_from_the_query_or_the_body() {
        assert_eq!(
            workspace_of("/v1/search?q=a&workspace=git%3Aseat", b""),
            "git:seat"
        );
        assert_eq!(
            workspace_of("/v1/atoms", br#"{"text":"one","workspace":"other seat"}"#),
            "other seat"
        );
        assert_eq!(workspace_of("/health", b""), "");
        assert_eq!(workspace_of("/v1/search?workspace=a&workspace=b", b""), "a");
    }

    #[test]
    fn stats_names_the_three_queues() {
        let control = quiet(4, 128);
        let value = control.snapshot().json();
        assert_eq!(value["line"], "ok");
        assert_eq!(value["queues"]["read"]["limit"], 3);
        assert_eq!(value["queues"]["read"]["budget"], 3);
        assert_eq!(value["queues"]["read"]["depth"], 0);
        assert_eq!(value["queues"]["read"]["latency_us"], 0);
        assert_eq!(value["queues"]["read"]["shed"], 0);
        assert_eq!(value["queues"]["write"]["limit"], 3);
        assert_eq!(value["queues"]["write"]["budget"], 3);
        assert_eq!(value["queues"]["maintenance"]["limit"], 1);
        assert_eq!(value["queues"]["maintenance"]["budget"], 1);
        assert_eq!(value["limits"]["total"], 3);
        assert_eq!(value["shed"]["write"], 0);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn pressure_stays_quiet_without_proc() {
        let psi = read_pressure();
        assert_eq!(psi, Psi::quiet());
    }
}
