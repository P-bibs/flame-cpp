#![allow(unused)]

//! Here's an example of how to use some of FLAMEs APIs:
//!
//! ```
//! extern crate flame;
//!
//! use std::fs::File;
//!
//! pub fn main() {
//!     // Manual `start` and `end`
//!     flame::start("read file");
//!     let x = read_a_file();
//!     flame::end("read file");
//!
//!     // Time the execution of a closure.  (the result of the closure is returned)
//!     let y = flame::span_of("database query", || query_database());
//!
//!     // Time the execution of a block by creating a guard.
//!     let z = {
//!         let _guard = flame::start_guard("cpu-heavy calculation");
//!         cpu_heavy_operations_1();
//!         // Notes can be used to annotate a particular instant in time.
//!         flame::note("something interesting happened", None);
//!         cpu_heavy_operations_2()
//!     };
//!
//!     // Dump the report to disk
//!     flame::dump_html(&mut File::create("flame-graph.html").unwrap()).unwrap();
//!
//!     // Or read and process the data yourself!
//!     let spans = flame::spans();
//!
//!     println!("{} {} {}", x, y, z);
//! }
//!
//! # fn read_a_file() -> bool { true }
//! # fn query_database() -> bool { true }
//! # fn cpu_heavy_operations_1() {}
//! # fn cpu_heavy_operations_2() -> bool { true }
//! ```


#[macro_use]
extern crate lazy_static;
extern crate thread_id;

#[cfg(feature = "json")]
#[macro_use]
extern crate serde_derive;
#[cfg(feature = "json")]
extern crate serde;
#[cfg(feature = "json")]
extern crate serde_json;

mod html;

use std::cell::{RefCell, Cell};
use std::iter::Peekable;
use std::borrow::Cow;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::io::{Write, Error as IoError};

pub type StrCow = Cow<'static, str>;

lazy_static!(static ref ALL_THREADS: Mutex<Vec<(usize, Option<String>, PrivateFrame)>> = Mutex::new(Vec::new()););
thread_local!(static LIBRARY: RefCell<Library> = RefCell::new(Library::new()));

#[derive(Debug)]
struct Library {
    name: Option<String>,
    current: PrivateFrame,
    epoch: Instant,
}

#[derive(Debug)]
struct PrivateFrame {
    next_id: u32,
    all: Vec<Event>,
    id_stack: Vec<u32>,
}

#[derive(Debug)]
struct Event {
    id: u32,
    parent: Option<u32>,
    name: StrCow,
    collapse: bool,
    start_ns: u64,
    end_ns: Option<u64>,
    delta: Option<u64>,
    notes: Vec<Note>,
}

/// A named timespan.
///
/// The span is the most important feature of Flame.  It denotes
/// a chunk of time that is important to you.
///
/// The Span records
/// * Start and stop time
/// * A list of children (also called sub-spans)
/// * A list of notes
#[derive(Debug, Clone)]
#[cfg_attr(feature = "json", derive(Serialize))]
pub struct Span {
    /// The name of the span
    pub name: StrCow,
    /// The timestamp of the start of the span
    pub start_ns: u64,
    /// The timestamp of the end of the span
    pub end_ns: u64,
    /// The time that ellapsed between start_ns and end_ns
    pub delta: u64,
    /// How deep this span is in the tree
    pub depth: u16,
    /// A list of spans that occurred inside this one
    pub children: Vec<Span>,
    /// A list of notes that occurred inside this span
    pub notes: Vec<Note>,
    #[cfg_attr(feature = "json", serde(skip_serializing))]
    collapsable: bool,
    #[cfg_attr(feature = "json", serde(skip_serializing))]
    _priv: (),
}

/// A note for use in debugging.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "json", derive(Serialize))]
pub struct Note {
    /// A short name describing what happened at some instant in time
    pub name: StrCow,
    /// A longer description
    pub description: Option<StrCow>,
    /// The time that the note was added
    pub instant: u64,
    #[cfg_attr(feature = "json", serde(skip_serializing))]
    _priv: (),
}

/// A collection of events that happened on a single thread.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "json", derive(Serialize))]
pub struct Thread {
    pub id: usize,
    pub name: Option<String>,
    pub spans: Vec<Span>,
    #[cfg_attr(feature = "json", serde(skip_serializing))]
    _priv: (),
}

#[must_use = "The guard is immediately dropped after instantiation. This is probably not
what you want! Consider using a `let` binding to increase its lifetime."]
pub struct SpanGuard {
    name: Option<StrCow>,
    collapse: bool,
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        if ::std::thread::panicking() { return; }
        let name = self.name.take().unwrap();
        end_impl(name, self.collapse);
    }
}

impl SpanGuard {
    pub fn end(self) { }
    pub fn end_collapse(mut self) {
        self.collapse = true;
    }
}

fn ns_since_epoch(epoch: Instant) -> u64 {
    let elapsed = epoch.elapsed();
    elapsed.as_secs() * 1000_000_000 + u64::from(elapsed.subsec_nanos())
}

fn convert_events_to_span<'a, I>(events: I) -> Vec<Span>
where I: Iterator<Item = &'a Event> {
    let mut iterator = events.peekable();
    let mut v = vec![];
    while let Some(event) = iterator.next() {
        if let Some(span) = event_to_span(event, &mut iterator, 0) {
            v.push(span);
        }
    }
    v
}

fn event_to_span<'a, I: Iterator<Item = &'a Event>>(event: &Event, events: &mut Peekable<I>, depth: u16) -> Option<Span> {
    if event.end_ns.is_some() && event.delta.is_some() {
        let mut span = Span {
            name: event.name.clone(),
            start_ns: event.start_ns,
            end_ns: event.end_ns.unwrap(),
            delta: event.delta.unwrap(),
            depth,
            children: vec![],
            notes: event.notes.clone(),
            collapsable: event.collapse,
            _priv: ()
        };

        loop {
            {
                match events.peek() {
                    Some(next) if next.parent != Some(event.id) => break,
                    None => break,
                    _ => {}
                }
            }

            let next = events.next().unwrap();
            let child = event_to_span(next, events, depth + 1);
            if let Some(child) = child {
                // Try to collapse with the previous span
                if !span.children.is_empty() && child.collapsable && child.children.is_empty() {
                    let last = span.children.last_mut().unwrap();
                    if last.name == child.name && last.depth == child.depth {
                        last.end_ns = child.end_ns;
                        last.delta += child.delta;
                        continue;
                    }
                }

                // Otherwise, it's a new node
                span.children.push(child);
            }
        }
        Some(span)
    } else {
        None
    }
}

impl Span {
    #[cfg(feature = "json")]
    pub fn into_json(&self) -> String {
        ::serde_json::to_string_pretty(self).unwrap()
    }
}

impl Thread {
    #[cfg(feature = "json")]
    pub fn into_json(&self) -> String {
        ::serde_json::to_string_pretty(self).unwrap()
    }

    #[cfg(feature = "json")]
    pub fn into_json_list(threads: &[Thread]) -> String {
        ::serde_json::to_string_pretty(threads).unwrap()
    }
}

impl Library {
    fn new() -> Library {
        Library {
            name: ::std::thread::current().name().map(Into::into),
            current: PrivateFrame {
                all: vec![],
                id_stack: vec![],
                next_id: 0,
            },
            epoch: Instant::now(),
        }
    }
}

fn commit_impl(library: &mut Library) {
    use std::thread;
    use std::sync::MutexGuard;
    use std::mem;
    
    let mut frame = PrivateFrame {
        all: vec![],
        id_stack: vec![],
        next_id: 0,
    };

    mem::swap(&mut frame, &mut library.current);
    if frame.all.is_empty() {
        return;
    }

    if let Ok(mut handle) = ALL_THREADS.lock() {
        let thread_name = library.name.clone();
        let thread_id = ::thread_id::get();
        handle.push((thread_id, thread_name, frame))
    }
}

pub fn commit_thread() {
    LIBRARY.with(|library| commit_impl(&mut *library.borrow_mut()));
}

impl Drop for Library {
    fn drop(&mut self) {
        if ::std::thread::panicking() { return; }
        commit_impl(self);
    }
}

/// Starts a `Span` and also returns a `SpanGuard`.
///
/// When the `SpanGuard` is dropped (or `.end()` is called on it),
/// the span will automatically be ended.
pub fn start_guard<S: Into<StrCow>>(name: S) -> SpanGuard {
    let name = name.into();
    start(name.clone());
    SpanGuard { name: Some(name), collapse: false }
}

/// Starts and ends a `Span` that lasts for the duration of the
/// function `f`.
pub fn span_of<S, F, R>(name: S, f: F) -> R where
S: Into<StrCow>,
F: FnOnce() -> R
{
    let name = name.into();
    start(name.clone());
    let r = f();
    end(name);
    r
}

/// Starts a new Span
pub fn start<S: Into<StrCow>>(name: S) {
    LIBRARY.with(|library| {
        let mut library = library.borrow_mut();
        let epoch = library.epoch;

        let collector = &mut library.current;
        let id = collector.next_id;
        collector.next_id += 1;

        let parent = collector.id_stack.last().cloned();

        let this = Event {
            id,
            parent,
            name: name.into(),
            collapse: false,
            start_ns: ns_since_epoch(epoch),
            end_ns: None,
            delta: None,
            notes: vec![]
        };

        collector.all.push(this);
        collector.id_stack.push(id);
    });
}

fn end_impl<S: Into<StrCow>>(name: S, collapse: bool) -> u64 {
    use std::thread;

    let name = name.into();
    let delta = LIBRARY.with(|library| {
        let mut library = library.borrow_mut();
        let epoch = library.epoch;
        let collector = &mut library.current;

        let current_id = match collector.id_stack.pop() {
            Some(id) => id,
            None if thread::panicking() => 0,
            None => panic!("flame::end({:?}) called without a currently running span!", &name)
        };

        let event = &mut collector.all[current_id as usize];

        if event.name != name {
            panic!("flame::end({}) attempted to end {}", &name, event.name);
        }

        let timestamp = ns_since_epoch(epoch);
        event.end_ns = Some(timestamp);
        event.collapse = collapse;
        event.delta = Some(timestamp - event.start_ns);
        event.delta
    });

    match delta {
        Some(d) => d,
        None => 0, // panicking
    }
}

/// Ends the current Span and returns the number
/// of nanoseconds that passed.
pub fn end<S: Into<StrCow>>(name: S) -> u64 {
    end_impl(name, false)
}

/// Ends the current Span and returns a given result.
///
/// This is mainly useful for code generation / plugins where
/// wrapping all returned expressions is easier than creating
/// a temporary variable to hold the result.
pub fn end_with<S: Into<StrCow>, R>(name: S, result: R) -> R {
    end_impl(name, false);
    result
}

/// Ends the current Span and returns the number of
/// nanoseconds that passed.
///
/// If this span is a leaf node, and the previous span
/// has the same name and depth, then collapse this
/// span into the previous one.  The end_ns field will
/// be updated to the end time of *this* span, and the
/// delta field will be the sum of the deltas from this
/// and the previous span.
///
/// This means that it is possible for end_ns - start_n
/// to not be equal to delta.
pub fn end_collapse<S: Into<StrCow>>(name: S) -> u64 {
    end_impl(name, true)
}

/// Records a note on the current Span.
pub fn note<S: Into<StrCow>>(name: S, description: Option<S>) {
    let name = name.into();
    let description = description.map(Into::into);

    LIBRARY.with(|library| {
        let mut library = library.borrow_mut();
        let epoch = library.epoch;

        let collector = &mut library.current;

        let current_id = match collector.id_stack.last() {
            Some(id) => *id,
            None => panic!("flame::note({}, {:?}) called without a currently running span!",
                           &name, &description)
        };

        let event = &mut collector.all[current_id as usize];
        event.notes.push(Note {
            name,
            description,
            instant: ns_since_epoch(epoch),
            _priv: ()
        });
    });
}

/// Clears all of the recorded info that Flame has
/// tracked.
pub fn clear() {
    if ::std::thread::panicking() { return; }
    LIBRARY.with(|library| {
        let mut library = library.borrow_mut();
        library.current = PrivateFrame {
            all: vec![],
            id_stack: vec![],
            next_id: 0,
        };
        library.epoch = Instant::now();
    });

    let mut handle = ALL_THREADS.lock().unwrap();
    handle.clear();
}

/// Returns a list of spans from the current thread
pub fn spans() -> Vec<Span> {
    if ::std::thread::panicking() { return vec![]; }
    LIBRARY.with(|library| {
        let library = library.borrow();
        let cur = &library.current;
        convert_events_to_span(cur.all.iter())
    })
}

pub fn threads() -> Vec<Thread> {
    if ::std::thread::panicking() { return vec![]; }

    let my_thread_name = ::std::thread::current().name().map(Into::into);
    let my_thread_id = ::thread_id::get();

    let mut out = vec![ Thread {
        id: my_thread_id,
        name: my_thread_name,
        spans: spans(),
        _priv: (),
    }];

    if let Ok(mut handle) = ALL_THREADS.lock() {
        for &(id, ref name, ref frm) in &*handle {
            out.push(Thread {
                id,
                name: name.clone(),
                spans: convert_events_to_span(frm.all.iter()),
                _priv: (),
            });
        }
    }

    out
}

/// Prints all of the frames to stdout.
pub fn debug() {
    if ::std::thread::panicking() { return; }
    LIBRARY.with(|library| {
        println!("{:?}", library);
    });
}

pub fn dump_text_to_writer<W: Write>(mut out: W) -> Result<(), IoError>  {
    fn print_span<W: Write>(span: &Span, out: &mut W) -> Result<f32, IoError> {
        let mut buf = String::new();
        for _ in 0 .. span.depth {
            buf.push_str("  ");
        }
        buf.push_str("| ");
        let ms = span.delta as f32 / 1000000.0;
        buf.push_str(&format!("{}: {}ms", span.name, ms));
        writeln!(out, "{}", buf)?;
        let mut missing = ms;
        for child in &span.children {
            missing -= print_span(child, out)?;
        }

        if !span.children.is_empty() {
            let mut buf = String::new();
            for _ in 0 ..= span.depth {
                buf.push_str("  ");
            }
            buf.push_str("+ ");
            buf.push_str(&format!("{}ms", missing));
            writeln!(out, "{}", buf)?;
        }

        Ok(ms)
    }

    for thread in threads() {
        writeln!(out, "THREAD: {}", thread.id)?;
        for span in thread.spans {
            print_span(&span, &mut out)?;
        }
        writeln!(out)?;
    }
    Ok(())
}

pub fn dump_stdout() {
    let stdout = ::std::io::stdout();
    let stdout = stdout.lock();
    dump_text_to_writer(stdout);
}

#[cfg(feature="json")]
pub fn dump_json<W: std::io::Write>(out: &mut W) -> std::io::Result<()> {
    out.write_all(serde_json::to_string_pretty(&threads()).unwrap().as_bytes())
}

pub use html::{dump_html, dump_html_custom};

// ======================= flamescope ===============================

mod flamescope {

use super::Span;
use super::StrCow;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedscopeFile {
    #[serde(rename = "$schema")]
    pub schema: &'static str,

    pub profiles: Vec<Profile>,
    pub shared: Shared,

    pub active_profile_index: Option<u64>,

    pub exporter: Option<String>,

    pub name: Option<String>,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
pub enum Profile {
    #[serde(rename_all = "camelCase")]
    Sampled {
        name: StrCow,
        unit: ValueUnit,
        start_value: u64,
        end_value: u64,
        samples: Vec<SampledStack>,
        weights: Vec<u64>,
    },
    #[serde(rename_all = "camelCase")]
    Evented {
        name: StrCow,
        unit: ValueUnit,
        start_value: u64,
        end_value: u64,
        events: Vec<Event>,
    },
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub at: u64,
    pub frame: usize,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum EventType {
    #[serde(rename = "O")]
    OpenFrame,
    #[serde(rename = "C")]
    CloseFrame,
}

type SampledStack = Vec<usize>;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Shared {
    pub frames: Vec<Frame>,
}

#[derive(Debug, PartialEq, Clone, Eq, Hash, Serialize, Deserialize)]
pub struct Frame {
    pub name: StrCow,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

impl Frame {
    #[inline]
    pub fn new(name: StrCow) -> Frame {
        Frame {
            name,
            file: None,
            line: None,
            col: None,
        }
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueUnit {
    Bytes,
    Microseconds,
    Milliseconds,
    Nanoseconds,
    None,
    Seconds,
}

extern crate indexmap;

use self::indexmap::IndexSet;

use std::io::Write;

const JSON_SCHEMA_URL: &str = "https://www.speedscope.app/file-format-schema.json";

/// Convert flame spans to the speedscope profile format.
pub fn spans_to_speedscope(spans: Vec<Span>) -> SpeedscopeFile {
    let mut frames = IndexSet::new();
    let profiles = spans
        .into_iter()
        .map(|span| Profile::Evented {
            name: span.name.clone(),
            unit: ValueUnit::Nanoseconds,
            start_value: span.start_ns,
            end_value: span.end_ns,
            events: {
                let mut events = Vec::new();
                span_extend_events(&mut frames, &mut events, span);
                events
            },
        })
        .collect();
    SpeedscopeFile {
        // always the same
        schema: JSON_SCHEMA_URL,
        active_profile_index: None,
        exporter: None,
        name: None,
        profiles,
        shared: Shared {
            frames: frames.into_iter().collect(),
        },
    }
}

fn span_extend_events(frames: &mut IndexSet<Frame>, events: &mut Vec<Event>, span: Span) {
    let (frame, _) = frames.insert_full(Frame::new(span.name));
    events.push(Event {
        event_type: EventType::OpenFrame,
        at: span.start_ns,
        frame,
    });
    for child in span.children {
        span_extend_events(frames, events, child);
    }
    events.push(Event {
        event_type: EventType::CloseFrame,
        at: span.end_ns,
        frame,
    });
}

#[inline]
pub fn dump(writer: impl Write) -> serde_json::Result<()> {
    write_spans(writer, super::spans())
}

#[inline]
pub fn write_spans(writer: impl Write, spans: Vec<Span>) -> serde_json::Result<()> {
    let speedscope = spans_to_speedscope(spans);
    serde_json::to_writer(writer, &speedscope)
}
}

// ============================ FFI  ================================
use std::ffi::CStr;
use std::fs::File;
use std::os::raw::c_char;

#[no_mangle]
pub extern fn flame_start(name: *const c_char) {
    let result = std::panic::catch_unwind(|| {
        let name = unsafe { CStr::from_ptr(name).to_str().unwrap().to_owned() };
        start(name); 
    });
    if result.is_err() {
        eprintln!("error: rust panicked");
    }
}

#[no_mangle]
pub extern fn flame_end(name: *const c_char) {
    let result = std::panic::catch_unwind(|| {
        let name = unsafe { CStr::from_ptr(name).to_str().unwrap().to_owned() };
        end(name); 
    });
    if result.is_err() {
        eprintln!("error: rust panicked");
    }
}

#[no_mangle]
pub extern fn flame_dump(path: *const c_char) {
    let path = unsafe { CStr::from_ptr(path).to_str().unwrap() };
    flamescope::dump(&mut File::create(path).unwrap()).unwrap();
}

#[no_mangle]
pub extern fn flame_dump_html(path: *const c_char) {
    let path = unsafe { CStr::from_ptr(path).to_str().unwrap() };
    dump_html(&mut File::create(path).unwrap()).unwrap();
}

#[no_mangle]
pub extern fn flame_debug() {
    debug();
}

#[no_mangle]
pub extern fn flame_dump_stdout() {
    dump_stdout();
}

#[no_mangle]
pub extern fn flame_clear() {
    clear();
}

