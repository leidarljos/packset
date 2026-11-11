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
//! so a single seat on a quiet machine runs as it did before.
//!
//! Linux writes the share of time tasks spent stalled for memory or CPU to
//! `/proc/pressure`. Maintenance stops first, then writes; reads stop only
//! when every task is stalled for memory. The files are absent off Linux,
//! and then nothing is shed for pressure.

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

/// The share of time tasks spent stalled, in percent, over the last ten
/// seconds. `memory_full` is the share in which every task was stalled for
/// memory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Psi {
    pub memory_some: f64,
    pub memory_full: f64,
    pub cpu_some: f64,
}

/// Maintenance pauses once memory or CPU stall is visible. Writes stop at
/// a higher memory stall, and reads only when the machine is stuck.
const MAINT_MEMORY_SOME: f64 = 10.0;
const MAINT_MEMORY_FULL: f64 = 1.0;
const MAINT_CPU_SOME: f64 = 70.0;
const WRITE_MEMORY_SOME: f64 = 25.0;
const WRITE_MEMORY_FULL: f64 = 2.0;
const READ_MEMORY_FULL: f64 = 10.0;

impl Psi {
    #[must_use]
    pub fn quiet() -> Self {
        Self {
            memory_some: 0.0,
            memory_full: 0.0,
            cpu_some: 0.0,
        }
    }

    /// `memory` and `cpu` are the texts of the pressure files. A missing
    /// line is zero, which sheds nothing.
    #[must_use]
    pub fn parse(memory: &str, cpu: &str) -> Self {
        Self {
            memory_some: avg10(memory, "some"),
            memory_full: avg10(memory, "full"),
            cpu_some: avg10(cpu, "some"),
        }
    }

    #[must_use]
    pub fn pause_maintenance(self) -> bool {
        self.memory_some >= MAINT_MEMORY_SOME
            || self.memory_full >= MAINT_MEMORY_FULL
            || self.cpu_some >= MAINT_CPU_SOME
    }

    #[must_use]
    pub fn shed_writes(self) -> bool {
        self.memory_some >= WRITE_MEMORY_SOME || self.memory_full >= WRITE_MEMORY_FULL
    }

    #[must_use]
    pub fn shed_reads(self) -> bool {
        self.memory_full >= READ_MEMORY_FULL
    }

    fn sheds(self, class: Class) -> bool {
        match class {
            Class::Read => self.shed_reads(),
            Class::Write => self.shed_writes(),
            Class::Maintenance => self.pause_maintenance(),
        }
    }
}

fn avg10(text: &str, kind: &str) -> f64 {
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix(kind) else {
            continue;
        };
        if !rest.starts_with([' ', '\t']) {
            continue;
        }
        for field in rest.split_whitespace() {
            if let Some(value) = field.strip_prefix("avg10=") {
                return value.parse().unwrap_or(0.0);
            }
        }
    }
    0.0
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
    Run { class: Class, body: T },
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
    Run { class: Class, body: T },
    Expired(T),
    Idle,
}

struct Waiting<T> {
    deadline_ms: Option<u64>,
    bytes: usize,
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
            pressure_pinned: false,
            pressure_at: std::time::Instant::now(),
            pressure_seen: false,
            logged_pressure: false,
        }
    }

    /// Use `psi` and do not read the pressure files. Tests pin a quiet
    /// reading so a busy host does not change the result.
    #[cfg(test)]
    pub fn pin_pressure(&mut self, psi: Psi) {
        self.pressure = psi;
        self.pressure_pinned = true;
        self.pressure_seen = true;
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
        if self.pressure.sheds(class) {
            self.note_shed(class, true);
            return Decision::Shed {
                body: arrival.body,
                retry_after: self.retry_after(class),
                reason: pressure_reason(class),
            };
        }
        if self.can_run(class) {
            self.acquire(class);
            return Decision::Run {
                class,
                body: arrival.body,
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
                };
            }
        }
        Next::Idle
    }

    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let paused = self.pressure.pause_maintenance();
        let shedding_writes = self.pressure.shed_writes();
        let busy = self.lanes.iter().any(|lane| lane.len > 0)
            || shedding_writes
            || self.pressure.shed_reads();
        Snapshot {
            read_depth: self.lanes[0].len,
            read_in_flight: self.in_flight[0],
            read_limit: self.limiters[0].current(),
            read_shed: self.sheds[0],
            write_depth: self.lanes[1].len,
            write_in_flight: self.in_flight[1],
            write_limit: self.limiters[1].current(),
            write_shed: self.sheds[1],
            maint_depth: self.lanes[2].len,
            maint_in_flight: self.in_flight[2],
            maint_limit: self.limiters[2].current(),
            maint_shed: self.sheds[2],
            maint_paused: paused,
            total_limit: self.total_limit,
            deadline_drops: self.deadline_drops,
            pressure_sheds: self.pressure_sheds,
            accept_sheds: self.accept_sheds,
            memory_some: self.pressure.memory_some,
            memory_full: self.pressure.memory_full,
            cpu_some: self.pressure.cpu_some,
            shedding_writes,
            busy,
        }
    }

    fn can_run(&self, class: Class) -> bool {
        if self.pressure.sheds(class) {
            return false;
        }
        if self.total_in_flight >= self.total_limit {
            return false;
        }
        if self.in_flight[class.index()] >= self.limiters[class.index()].current() {
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
            || self.pressure.shed_writes()
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
        let became = next.shed_writes() && !self.pressure.shed_writes();
        self.pressure = next;
        self.pressure_at = std::time::Instant::now();
        self.pressure_seen = true;
        if became && !self.logged_pressure {
            eprintln!("packsetd: memory pressure; writes are shed and maintenance is paused");
            self.logged_pressure = true;
        }
        if !next.shed_writes() && !next.pause_maintenance() {
            self.logged_pressure = false;
        }
    }
}

fn pressure_reason(class: Class) -> &'static str {
    match class {
        Class::Maintenance => "maintenance is paused; try again",
        Class::Write | Class::Read => "memory pressure; try again",
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
    pub read_shed: u64,
    pub write_depth: usize,
    pub write_in_flight: usize,
    pub write_limit: usize,
    pub write_shed: u64,
    pub maint_depth: usize,
    pub maint_in_flight: usize,
    pub maint_limit: usize,
    pub maint_shed: u64,
    pub maint_paused: bool,
    pub total_limit: usize,
    pub deadline_drops: u64,
    pub pressure_sheds: u64,
    pub accept_sheds: u64,
    pub memory_some: f64,
    pub memory_full: f64,
    pub cpu_some: f64,
    pub shedding_writes: bool,
    pub busy: bool,
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
                    "shed": self.read_shed,
                },
                "write": {
                    "depth": self.write_depth,
                    "in_flight": self.write_in_flight,
                    "limit": self.write_limit,
                    "shed": self.write_shed,
                },
                "maintenance": {
                    "depth": self.maint_depth,
                    "in_flight": self.maint_in_flight,
                    "limit": self.maint_limit,
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
            Next::Run { class, body } => {
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
        let Next::Run { class, body } = control.take(NOW) else {
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

    #[test]
    fn memory_pressure_sheds_maintenance_then_writes_then_reads() {
        let mut control = quiet(4, 8);
        control.pin_pressure(Psi {
            memory_some: 12.0,
            memory_full: 0.0,
            cpu_some: 0.0,
        });
        assert!(matches!(
            control.arrive(job(Class::Maintenance, "seat", 1), NOW),
            Decision::Shed { .. }
        ));
        assert!(matches!(
            control.arrive(job(Class::Write, "seat", 2), NOW),
            Decision::Run { .. }
        ));
        control.finish(Class::Write, None);

        control.pin_pressure(Psi {
            memory_some: 30.0,
            memory_full: 0.0,
            cpu_some: 0.0,
        });
        match control.arrive(job(Class::Write, "seat", 3), NOW) {
            Decision::Shed {
                retry_after,
                reason,
                ..
            } => {
                assert_eq!(retry_after, 1);
                assert!(reason.contains("memory"));
            }
            other => panic!("{}", label(&other)),
        }
        assert!(matches!(
            control.arrive(job(Class::Read, "seat", 4), NOW),
            Decision::Run { .. }
        ));
        control.finish(Class::Read, None);

        control.pin_pressure(Psi {
            memory_some: 30.0,
            memory_full: 12.0,
            cpu_some: 0.0,
        });
        assert!(matches!(
            control.arrive(job(Class::Read, "seat", 5), NOW),
            Decision::Shed { .. }
        ));
        let snap = control.snapshot();
        assert!(snap.pressure_sheds >= 3);
        assert!(snap.busy);
        assert!(snap.shedding_writes);
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
        assert!(psi.shed_writes());
        assert!(psi.pause_maintenance());
        assert!(!psi.shed_reads());
        let quiet = Psi::parse("", "");
        assert!(!quiet.shed_writes());
        assert!(!quiet.pause_maintenance());
        assert!(!quiet.shed_reads());
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
        assert_eq!(value["queues"]["write"]["limit"], 3);
        assert_eq!(value["queues"]["maintenance"]["limit"], 1);
        assert_eq!(value["limits"]["total"], 3);
        assert_eq!(value["shed"]["write"], 0);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn pressure_stays_quiet_without_proc() {
        let psi = read_pressure();
        assert!(!psi.shed_writes());
        assert!(!psi.pause_maintenance());
        assert!(!psi.shed_reads());
    }
}
