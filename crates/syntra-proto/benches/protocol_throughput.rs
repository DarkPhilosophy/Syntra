//! Deterministic throughput and latency benchmark of the peer protocol codec.
//!
//! Every input event a user produces crosses this codec twice: encoded on the
//! capturing machine and decoded on the receiving one, so its cost is the
//! floor under input-processing cost. The benchmark drives the real
//! [`ProtoEvent::encode`] and [`ProtoEvent::decode`] over fixed workloads.
//!
//! # What this does and does not measure
//!
//! It measures *codec CPU cost per event* on the machine it runs on: a
//! deterministic software workload with no display, input device, network or
//! privilege. It makes **no claim about end-to-end input latency**, which also
//! includes capture, the network, emulation and the compositor, none of which
//! run here.
//!
//! # Determinism and correctness
//!
//! Workloads come from a fixed-seed integer generator, so two runs process
//! byte-identical events in the same order. Before anything is timed, every
//! scenario is verified *outside the timed section*: each frame must decode
//! and re-encode to the identical bytes. The printed `digest` is an FNV-1a
//! over all encoded bytes. If the digest differs between two builds, the wire
//! bytes changed and the timings must not be compared.
//!
//! # Method
//!
//! One untimed warm-up pass, then `SAMPLES` timed samples of the whole
//! workload. The reported `ns/event` is the **median** sample divided by the
//! event count, and `iqr` is the inter-quartile range of the samples as a share
//! of that median, so a noisy machine is visible rather than hidden. It
//! includes the per-batch timer reads. `p99` is the 99th percentile of
//! per-batch *means* (batches of [`LATENCY_BATCH`] events) in the median
//! sample: it is not the latency of any single event.
//!
//! `encode` consumes its event, so `encode+clone` includes one `clone` per
//! iteration. The `clone` row measures that alone; subtract it to read the
//! codec by itself. `decode` borrows its frame and has no such overhead.
//!
//! # Running
//!
//! `cargo bench -p syntra-proto --bench protocol_throughput`
//!
//! Options after `--`: `--quick` shortens the run, `--json` prints one JSON
//! object per row.

use std::hint::black_box;
use std::time::Instant;

use syntra_input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use syntra_proto::{
    ClipboardEntryKind, ClipboardManifestEntry, MAX_CLIPBOARD_CHUNK_SIZE, Position, ProtoEvent,
};

/// Events timed together for one latency reading.
const LATENCY_BATCH: usize = 64;
/// Fixed seed: the workload never changes between runs.
const SEED: u64 = 0x5eed_c0de_0000_0011;

/// A small deterministic generator (SplitMix64). Chosen so the benchmark has
/// no dependency and no platform variation; it is not for cryptography.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// A pointer delta between -64.0 and 64.0 in 1/16 steps: the range and
    /// granularity relative motion has.
    fn delta(&mut self) -> f64 {
        (self.below(2049) as f64 - 1024.0) / 16.0
    }
}

/// One named, fixed workload.
struct Scenario {
    name: &'static str,
    events: Vec<ProtoEvent>,
}

/// Mostly pointer motion with the keys, buttons and scrolls that interleave
/// with it in real use: the dominant traffic while a pointer is on a peer.
fn input_mix(count: usize) -> Scenario {
    let mut rng = Rng(SEED);
    let mut time = 0u32;
    let events = (0..count)
        .map(|_| {
            time = time.wrapping_add(1 + rng.below(8) as u32);
            ProtoEvent::Input(match rng.below(100) {
                0..=84 => InputEvent::Pointer(PointerEvent::Motion {
                    time,
                    dx: rng.delta(),
                    dy: rng.delta(),
                }),
                85..=89 => InputEvent::Pointer(PointerEvent::Button {
                    time,
                    button: 0x110 + rng.below(3) as u32,
                    state: rng.below(2) as u32,
                }),
                90..=93 => InputEvent::Pointer(PointerEvent::Axis {
                    time,
                    axis: rng.below(2) as u8,
                    value: rng.delta(),
                }),
                94..=95 => InputEvent::Pointer(PointerEvent::AxisDiscrete120 {
                    axis: rng.below(2) as u8,
                    value: (rng.below(5) as i32 - 2) * 120,
                }),
                96..=98 => InputEvent::Keyboard(KeyboardEvent::Key {
                    time,
                    key: 1 + rng.below(120) as u32,
                    state: rng.below(2) as u8,
                }),
                _ => InputEvent::Keyboard(KeyboardEvent::Modifiers {
                    depressed: rng.below(256) as u32,
                    latched: 0,
                    locked: rng.below(4) as u32,
                    group: 0,
                }),
            })
        })
        .collect();
    Scenario {
        name: "input-mix",
        events,
    }
}

/// Pointer motion alone: the worst case for event rate, such as a
/// high-polling-rate mouse moving continuously.
fn motion_only(count: usize) -> Scenario {
    let mut rng = Rng(SEED ^ 1);
    let mut time = 0u32;
    let events = (0..count)
        .map(|_| {
            time = time.wrapping_add(1);
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Motion {
                time,
                dx: rng.delta(),
                dy: rng.delta(),
            }))
        })
        .collect();
    Scenario {
        name: "motion-only",
        events,
    }
}

/// Edge crossings, acknowledgements and keepalive: rare, but they must stay
/// cheap.
fn control_mix(count: usize) -> Scenario {
    let mut rng = Rng(SEED ^ 2);
    let sides = [
        Position::Left,
        Position::Right,
        Position::Top,
        Position::Bottom,
    ];
    let events = (0..count)
        .map(|_| match rng.below(4) {
            0 => ProtoEvent::EnterAt {
                side: sides[rng.below(4) as usize],
                along: rng.below(1000) as f32 / 1000.0,
            },
            1 => ProtoEvent::LeaveAt {
                along: rng.below(1000) as f32 / 1000.0,
            },
            2 => ProtoEvent::Ack(rng.below(1 << 20) as u32),
            _ => ProtoEvent::Ping,
        })
        .collect();
    Scenario {
        name: "control-mix",
        events,
    }
}

/// Manifests of 64 entries: the control traffic that opens a file transfer,
/// dominated by path strings.
///
/// The builder keeps to the encoder's own invariants rather than relying on
/// the codec to accept anything: file ids start at 1 and strictly increase,
/// directories have size 0, paths are relative with no `:`, `\` or empty,
/// `.` or `..` component, and the sizes sum to at most 64 * 512 KiB = 32 MiB,
/// under the 64 MiB clipboard limit. `prepare` still verifies every event.
fn manifests(count: usize) -> Scenario {
    const ENTRIES: u64 = 64;
    const MAX_FILE_SIZE: u64 = 512 * 1024;
    let mut rng = Rng(SEED ^ 3);
    let events = (0..count)
        .map(|round| ProtoEvent::ClipboardManifest {
            transfer_id: round as u64 + 1,
            entries: (1..=ENTRIES)
                .map(|file_id| {
                    let directory = file_id % 8 == 1;
                    ClipboardManifestEntry {
                        file_id,
                        path: format!("folder/{}/file-{file_id}.dat", rng.below(1000)),
                        kind: if directory {
                            ClipboardEntryKind::Directory
                        } else {
                            ClipboardEntryKind::File
                        },
                        size: if directory {
                            0
                        } else {
                            rng.below(MAX_FILE_SIZE + 1)
                        },
                    }
                })
                .collect(),
        })
        .collect();
    Scenario {
        name: "manifest-64-entries",
        events,
    }
}

/// Maximum-size clipboard chunks: the bulk path of a large paste.
fn clipboard_chunks(count: usize) -> Scenario {
    let mut rng = Rng(SEED ^ 4);
    let payload: Vec<u8> = (0..MAX_CLIPBOARD_CHUNK_SIZE)
        .map(|_| rng.below(256) as u8)
        .collect();
    let events = (0..count)
        .map(|index| ProtoEvent::ClipboardChunk {
            transfer_id: 7,
            index: index as u32,
            data: payload.clone(),
        })
        .collect();
    Scenario {
        name: "clipboard-chunk-max",
        events,
    }
}

/// FNV-1a over encoded bytes, so a run can prove it did the same work.
fn fingerprint(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// The encoded workload and what was checked about it.
struct Prepared {
    frames: Vec<Vec<u8>>,
    wire_bytes: usize,
    digest: u64,
}

/// Encodes the workload once and verifies it, *outside any timed section*:
/// every frame must decode, and the decoded event must re-encode to the
/// identical bytes. A codec that is fast because it is wrong fails here.
fn prepare(scenario: &Scenario) -> Prepared {
    let mut frames = Vec::with_capacity(scenario.events.len());
    let mut digest = 0xcbf2_9ce4_8422_2325u64;
    let mut wire_bytes = 0usize;
    for event in &scenario.events {
        let frame = event
            .clone()
            .encode()
            .unwrap_or_else(|error| panic!("{}: benchmark event invalid: {error}", scenario.name));
        let decoded = ProtoEvent::decode(&frame)
            .unwrap_or_else(|error| panic!("{}: frame does not decode: {error}", scenario.name));
        let again = decoded
            .encode()
            .unwrap_or_else(|error| panic!("{}: decoded event invalid: {error}", scenario.name));
        assert_eq!(
            frame, again,
            "{}: decode then encode changed the bytes",
            scenario.name
        );
        wire_bytes += frame.len();
        fingerprint(&mut digest, &frame);
        frames.push(frame);
    }
    Prepared {
        frames,
        wire_bytes,
        digest,
    }
}

/// Everything reported for one scenario and one operation.
struct Row {
    scenario: &'static str,
    operation: &'static str,
    events: usize,
    wire_bytes: usize,
    digest: u64,
    median_ns_per_event: f64,
    iqr_percent: f64,
    p99_batch_ns_per_event: f64,
    megabytes_per_second: f64,
}

/// Times `work` over the half-open range of events, in batches.
fn sample<F: FnMut(usize, usize)>(events: usize, work: &mut F) -> (f64, Vec<f64>) {
    let mut batches = Vec::with_capacity(events / LATENCY_BATCH + 1);
    let started = Instant::now();
    let mut at = 0;
    while at < events {
        let end = (at + LATENCY_BATCH).min(events);
        let batch_started = Instant::now();
        work(at, end);
        batches.push(batch_started.elapsed().as_nanos() as f64 / (end - at) as f64);
        at = end;
    }
    (started.elapsed().as_nanos() as f64, batches)
}

fn quantile(sorted: &[f64], fraction: f64) -> f64 {
    let index = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// One warm-up pass, then `samples` timed passes; reports the median.
fn measure<F: FnMut(usize, usize)>(
    scenario: &'static str,
    operation: &'static str,
    prepared: &Prepared,
    samples: usize,
    mut work: F,
) -> Row {
    let events = prepared.frames.len();
    work(0, events);

    let mut runs: Vec<(f64, Vec<f64>)> = (0..samples).map(|_| sample(events, &mut work)).collect();
    runs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let totals: Vec<f64> = runs.iter().map(|run| run.0).collect();
    let median_total = quantile(&totals, 0.5);
    let spread = quantile(&totals, 0.75) - quantile(&totals, 0.25);

    let mut batches = runs[runs.len() / 2].1.clone();
    batches.sort_by(|a, b| a.total_cmp(b));

    Row {
        scenario,
        operation,
        events,
        wire_bytes: prepared.wire_bytes,
        digest: prepared.digest,
        median_ns_per_event: median_total / events as f64,
        iqr_percent: spread / median_total * 100.0,
        p99_batch_ns_per_event: quantile(&batches, 0.99),
        megabytes_per_second: prepared.wire_bytes as f64 / (median_total / 1e9) / 1_000_000.0,
    }
}

fn run_scenario(scenario: &Scenario, samples: usize) -> Vec<Row> {
    let prepared = prepare(scenario);
    let name = scenario.name;
    let events = &scenario.events;
    let frames = &prepared.frames;

    // The control: what cloning the input costs, so it can be subtracted.
    let clone = measure(name, "clone", &prepared, samples, |from, to| {
        for event in &events[from..to] {
            black_box(black_box(event).clone());
        }
    });
    let encode = measure(name, "encode+clone", &prepared, samples, |from, to| {
        for event in &events[from..to] {
            black_box(black_box(event.clone()).encode().expect("verified"));
        }
    });
    let decode = measure(name, "decode", &prepared, samples, |from, to| {
        for frame in &frames[from..to] {
            black_box(ProtoEvent::decode(black_box(frame)).expect("verified"));
        }
    });
    let round_trip = measure(name, "round-trip", &prepared, samples, |from, to| {
        for event in &events[from..to] {
            let frame = black_box(event.clone()).encode().expect("verified");
            black_box(ProtoEvent::decode(black_box(&frame)).expect("verified"));
        }
    });
    vec![clone, encode, decode, round_trip]
}

fn print_table(rows: &[Row]) {
    println!(
        "{:<20} {:<12} {:>8} {:>11} {:>7} {:>11} {:>9}  digest",
        "scenario", "operation", "events", "ns/event", "iqr%", "p99 ns/ev", "MB/s"
    );
    for row in rows {
        println!(
            "{:<20} {:<12} {:>8} {:>11.1} {:>7.1} {:>11.1} {:>9.1}  {:016x}",
            row.scenario,
            row.operation,
            row.events,
            row.median_ns_per_event,
            row.iqr_percent,
            row.p99_batch_ns_per_event,
            row.megabytes_per_second,
            row.digest
        );
    }
}

fn print_json(rows: &[Row]) {
    for row in rows {
        println!(
            "{{\"scenario\":\"{}\",\"operation\":\"{}\",\"events\":{},\"wire_bytes\":{},\
             \"median_ns_per_event\":{:.2},\"iqr_percent\":{:.2},\
             \"p99_batch_ns_per_event\":{:.2},\"megabytes_per_second\":{:.2},\
             \"digest\":\"{:016x}\"}}",
            row.scenario,
            row.operation,
            row.events,
            row.wire_bytes,
            row.median_ns_per_event,
            row.iqr_percent,
            row.p99_batch_ns_per_event,
            row.megabytes_per_second,
            row.digest
        );
    }
}

/// Written to stderr so the evidence is self-describing even when stdout is
/// machine-readable JSON.
const METHOD: &str = "\
syntra-proto codec benchmark (software only: no input device, display or network)
  operations:
    clone         ProtoEvent::clone alone. A CONTROL, not codec work.
    encode+clone  clone + ProtoEvent::encode. encode takes `self`, so each
                  iteration must clone; subtract `clone` for the codec alone.
    decode        ProtoEvent::decode over a borrowed frame (no clone).
    round-trip    clone + encode + decode.
  ns/event is the MEDIAN sample of the whole workload divided by event count; it
  includes the per-batch timer reads and bookkeeping (batches of 64 events).
  p99 is the 99th percentile of BATCH MEANS in the median sample, NOT the
  latency of any single event. iqr% is the inter-quartile range of the samples
  as a share of the median.
  digest is FNV-1a over all encoded bytes: if it differs between two builds the
  wire bytes changed and timings must not be compared.
  This is codec CPU cost only; it says nothing about end-to-end input latency.";

fn main() {
    const FLAGS: [&str; 3] = ["--quick", "--json", "--bench"];
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| arguments.iter().any(|argument| argument == name);
    let quick = flag("--quick");
    // `cargo test --benches` runs this binary without `--bench`; keep that a
    // fast, still-verified smoke run instead of a full measurement.
    let smoke = !flag("--bench") && !flag("--json");

    let unknown: Vec<&String> = arguments
        .iter()
        .filter(|argument| argument.starts_with("--") && !FLAGS.contains(&argument.as_str()))
        .collect();
    if !unknown.is_empty() {
        if smoke {
            // `cargo test -- --nocapture` and similar reach every test target,
            // including this one; they are not ours to reject.
            eprintln!("ignoring options meant for the test harness: {unknown:?}");
        } else {
            // A measurement must never silently run something other than what
            // was asked for, such as after a typo.
            eprintln!("unknown option(s) {unknown:?}; expected one of {FLAGS:?}");
            std::process::exit(2);
        }
    }
    let (scale, samples) = if quick || smoke { (1, 3) } else { (20, 15) };

    eprintln!("{METHOD}");
    eprintln!(
        "  run: scale={scale} samples={samples} batch={LATENCY_BATCH} seed={SEED:#018x}{}",
        if smoke {
            " (smoke run: not a measurement)"
        } else {
            ""
        }
    );

    let scenarios = [
        input_mix(10_000 * scale),
        motion_only(10_000 * scale),
        control_mix(10_000 * scale),
        manifests(100 * scale),
        clipboard_chunks(50 * scale),
    ];
    let rows: Vec<Row> = scenarios
        .iter()
        .flat_map(|scenario| run_scenario(scenario, samples))
        .collect();
    if flag("--json") {
        print_json(&rows);
    } else {
        print_table(&rows);
    }
}
