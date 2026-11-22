//! The app of `examples/rtic_nor`, on std: the same objects and the same steps, with threads in
//! place of the RTIC tasks, and the file system backend in place of the NOR flash.
//!
//! ```text
//! cargo run --example std_fs --features fs [dir]
//! ```
//!
//! The objects are kept in `dir`, by default `objshelf-std-fs` in the temporary directory. At
//! start, it prints the settings, samples and pattern it found from the last run, or created on the
//! first one. Run it again: the boot count goes up, and the other objects carry over.
//!
//! At the end, it prints the time each objshelf function took: its calls, their mean and the
//! longest. The flushes include the writes reaching the disk: the backend syncs each file.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use get_size2::GetSize;
use objshelf::backend::fs::{FsBackend, FsConfig, FsError};
use objshelf::{Error, Handle, ObjShelf};
use serde::{Deserialize, Serialize};

type Mem = ObjShelf<FsBackend>;

/// Rounds of the app, one per `PERIOD_MS`, before the report.
const ROUNDS: u32 = 20;
const PERIOD_MS: u32 = 250;
/// Rounds between two flushes of all the changes.
const SAVE_EVERY: u32 = 5;
/// Samples kept, the oldest dropped first.
const SAMPLES: usize = 64;
/// RAM budget of the objects.
const CACHE_BYTES: usize = 16 * 1024;

#[derive(Serialize, Deserialize, GetSize)]
struct Settings {
    brightness: u8,
    name: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            brightness: 0,
            name: "objshelf example".into(),
        }
    }
}

/// The last readings of a sensor, oldest first.
#[derive(Default, Serialize, Deserialize, GetSize)]
struct Samples(Vec<u16>);

/// A made-up sensor: a temperature in hundredths of a degree.
fn read_sensor(round: u32) -> u16 {
    2000 + (round * 37 % 100) as u16
}

/// Steps of the pattern: 8 tracks of 64.
const STEPS: usize = 512;

/// A sequencer pattern, of about 1 KiB serialized: more than a page of the NOR backend, which holds
/// 234 bytes of an object per page. It takes 5 of them, written and read back as one object.
#[derive(Serialize, Deserialize, GetSize)]
struct Pattern {
    steps: Vec<Step>,
    /// Steps edited since the pattern was created, one per round.
    edits: u32,
}

#[derive(Clone, Copy, Serialize, Deserialize, GetSize)]
struct Step {
    note: u8,
    velocity: u8,
}

impl Default for Pattern {
    fn default() -> Self {
        let step = Step {
            note: 60,
            velocity: 100,
        };
        Self {
            steps: vec![step; STEPS],
            edits: 0,
        }
    }
}

impl Pattern {
    /// A made-up edit: the next step, in turn, gets another note.
    fn edit(&mut self) {
        let step = self.edits as usize % STEPS;
        self.steps[step].note = 36 + (self.edits % 48) as u8;
        self.edits += 1;
    }

    /// Its steps, its edits and the sum of its notes. The sum covers every step, so every page of
    /// the object counts in it.
    fn summary(&self) -> (usize, u32, u32) {
        let notes = self.steps.iter().map(|step| u32::from(step.note)).sum();
        (self.steps.len(), self.edits, notes)
    }
}

/// The handles of the objects the tasks use: `Copy`, as each handle is.
#[derive(Clone, Copy)]
struct Objects {
    settings: Handle<Settings>,
    samples: Handle<Samples>,
    pattern: Handle<Pattern>,
}

/// Work for the saver thread, which stands in for the RTIC tasks below `app`.
enum Save {
    /// `save`: writes all the changes.
    All,
    /// `save_settings`: writes only the settings.
    Settings(Handle<Settings>),
    /// `report`: prints the state of the memory, then the thread ends.
    Report(Objects),
}

/// The saver, for `on_full`: a `fn()` can't capture it, so it is in a static.
static SAVER: OnceLock<Sender<Save>> = OnceLock::new();

/// Sends the report job when dropped, which ends the saver thread: after `app`, also when it fails
/// or panics. The channel never closes by itself, as `SAVER` keeps a sender.
struct SendReport<'a> {
    saver: &'a Sender<Save>,
    objects: Objects,
}

impl Drop for SendReport<'_> {
    fn drop(&mut self) {
        self.saver.send(Save::Report(self.objects)).ok();
    }
}

/// The functions of objshelf take anything that implements `objshelf::Mutex`. With RTIC, that is
/// the resource proxy (`&mut cx.shared.mem`); here, a std mutex shared by the threads.
struct Shared<'a>(&'a Mutex<Mem>);

impl objshelf::Mutex for Shared<'_> {
    type T = Mem;

    fn lock<R>(&mut self, f: impl FnOnce(&mut Mem) -> R) -> R {
        f(&mut self.0.lock().expect("no thread panics with the lock"))
    }
}

/// The calls to one objshelf function.
#[derive(Default)]
struct Time {
    calls: u32,
    total: Duration,
    longest: Duration,
}

/// The calls to each objshelf function, by name, from both threads.
static TIMES: Mutex<BTreeMap<&str, Time>> = Mutex::new(BTreeMap::new());

/// Runs `call`, a call to the objshelf function `name`, and adds its time to `TIMES`. On the app
/// thread, that includes waiting for the lock while the saver holds it, between two writes of a
/// flush.
fn timed<R>(name: &'static str, call: impl FnOnce() -> R) -> R {
    let start = Instant::now();
    let result = call();
    let elapsed = start.elapsed();
    let mut times = TIMES.lock().expect("no thread panics with the lock");
    let time = times.entry(name).or_default();
    time.calls += 1;
    time.total += elapsed;
    time.longest = time.longest.max(elapsed);
    result
}

fn print_times() {
    println!("time in objshelf:");
    println!(
        "  {:<18} {:>5} {:>10} {:>10}",
        "function", "calls", "mean", "longest"
    );
    for (name, time) in TIMES.lock().expect("no thread panics with the lock").iter() {
        let mean = time.total / time.calls;
        println!(
            "  {name:<18} {:>5} {mean:>10.1?} {:>10.1?}",
            time.calls, time.longest,
        );
    }
}

fn main() -> Result<(), Error<FsError>> {
    let dir = std::env::args_os().nth(1).map_or_else(
        || std::env::temp_dir().join("objshelf-std-fs"),
        PathBuf::from,
    );
    println!("objects in {}", dir.display());
    let config = FsConfig {
        dir,
        capacity: 1 << 20,
        direct_io: true,
    };
    let (saver, work) = mpsc::channel();
    SAVER.set(saver.clone()).ok();
    // Objects in RAM take about 16 KiB. The closure runs when unflushed changes fill that budget:
    // here it has the saver write them all.
    let mut mem = timed("mount", || {
        Mem::mount(config, CACHE_BYTES, || {
            if let Some(saver) = SAVER.get() {
                saver.send(Save::All).ok();
            }
        })
    })?;

    // Before the memory is shared, the functions take it directly, as in RTIC's `init`. Each object
    // is found after a restart, or created on the first one: the names stored by the earlier runs
    // tell which.
    let stored = timed("list", || objshelf::list(&mut mem));
    let origin = |name: &str| {
        if stored.iter().any(|stored| stored == name) {
            "found"
        } else {
            "created"
        }
    };
    let boots = timed("get_or_insert_with", || {
        objshelf::get_or_insert_with(&mut mem, "boots", || 0u32)
    })?
    .expect("fits");
    let boot = timed("with_mut", || {
        objshelf::with_mut(&mut mem, &boots, |n| {
            *n += 1;
            *n
        })
    })?
    .expect("nothing else has the memory");
    timed("flush_object", || objshelf::flush_object(&mut mem, &boots))?;
    println!("boot {boot}");
    let settings = timed("get_or_insert_with", || {
        objshelf::get_or_insert_with(&mut mem, "settings", Settings::default)
    })?;
    let settings = settings.expect("fits");
    let samples = timed("get_or_insert_with", || {
        objshelf::get_or_insert_with(&mut mem, "samples", Samples::default)
    })?;
    let samples = samples.expect("fits");
    let pattern = timed("get_or_insert_with", || {
        objshelf::get_or_insert_with(&mut mem, "pattern", Pattern::default)
    })?;
    let pattern = pattern.expect("fits");

    // What the earlier runs left, before this one changes it.
    let (name, brightness) = timed("with", || {
        objshelf::with(&mut mem, &settings, |s| (s.name.clone(), s.brightness))
    })?
    .expect("nothing else has the memory");
    println!(
        "{} settings: {name:?}, brightness {brightness}",
        origin("settings")
    );
    let (count, last) = timed("with", || {
        objshelf::with(&mut mem, &samples, |s| (s.0.len(), s.0.last().copied()))
    })?
    .expect("nothing else has the memory");
    println!("{} samples: {count}, last {last:?}", origin("samples"));
    let (steps, edits, notes) = timed("with", || {
        objshelf::with(&mut mem, &pattern, Pattern::summary)
    })?
    .expect("nothing else has the memory");
    println!(
        "{} pattern: {steps} steps, {edits} edits, sum of notes {notes}",
        origin("pattern")
    );

    let objects = Objects {
        settings,
        samples,
        pattern,
    };
    let mem = Mutex::new(mem);
    thread::scope(|scope| {
        let thread = scope.spawn(|| save(Shared(&mem), work));
        let done = {
            let _report = SendReport {
                saver: &saver,
                objects,
            };
            app(Shared(&mem), &saver, objects)
        };
        // A panic of the saver thread goes on in this one.
        let saved = thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        done.and(saved)
    })?;
    // Flushes what is left, as RTIC's `report` does.
    let mem = mem.into_inner().expect("no thread panics with the lock");
    timed("unmount", || mem.unmount())?;
    print_times();
    Ok(())
}

/// The RTIC task `app`, of the highest priority. Here the threads run side by side, where RTIC's
/// `app` preempts the flushes: either way, it can find the storage busy, and then gets `Ok(None)`.
fn app(mut mem: Shared, saver: &Sender<Save>, objects: Objects) -> Result<(), Error<FsError>> {
    let Objects {
        settings,
        samples,
        pattern,
    } = objects;
    for round in 1..=ROUNDS {
        thread::sleep(Duration::from_millis(PERIOD_MS.into()));
        // Reads the object from the backend if it is not in RAM. The change stays in RAM until
        // the saver writes it, straight away.
        let brightness = timed("with_mut", || {
            objshelf::with_mut(&mut mem, &settings, |s| {
                s.brightness = s.brightness.wrapping_add(1);
                s.brightness
            })
        })?;
        match brightness {
            Some(brightness) => {
                println!("round {round}: brightness {brightness}");
                saver.send(Save::Settings(settings)).ok();
            }
            // A flush has the storage and the settings are not in RAM, or unflushed changes
            // fill the budget (`on_full` asked for a flush): try again next round.
            None => println!("round {round}: busy, next round"),
        }

        let reading = read_sensor(round);
        let added = timed("with_mut", || {
            objshelf::with_mut(&mut mem, &samples, |s| {
                if s.0.len() == SAMPLES {
                    s.0.remove(0);
                }
                s.0.push(reading);
            })
        })?;
        if added.is_none() {
            println!("round {round}: busy, sample {reading} dropped");
        }
        let edited = timed("with_mut", || {
            objshelf::with_mut(&mut mem, &pattern, Pattern::edit)
        })?;
        if edited.is_none() {
            println!("round {round}: busy, pattern edit dropped");
        }
        // The samples and the pattern are written with the other changes, every few rounds.
        if round % SAVE_EVERY == 0 {
            saver.send(Save::All).ok();
        }
    }
    Ok(())
}

/// The RTIC tasks below `app`: `save`, `save_settings` and `report`. One thread runs them in the
/// order they are sent, as RTIC runs the saves one after the other and `report`, of a lower
/// priority, after them: no flush finds another one running, and none gets `Ok(None)`.
fn save(mut mem: Shared, work: Receiver<Save>) -> Result<(), Error<FsError>> {
    for job in work {
        match job {
            Save::All => {
                timed("flush", || objshelf::flush(&mut mem))?;
                println!("saved all the changes");
            }
            Save::Settings(settings) => {
                timed("flush_object", || {
                    objshelf::flush_object(&mut mem, &settings)
                })?;
            }
            Save::Report(objects) => return report(mem, objects),
        }
    }
    Ok(())
}

/// The RTIC task `report`: the state of the memory once the rounds are done.
fn report(mut mem: Shared, objects: Objects) -> Result<(), Error<FsError>> {
    let Objects {
        settings,
        samples,
        pattern,
    } = objects;
    timed("flush", || objshelf::flush(&mut mem))?;
    let mut names = timed("list", || objshelf::list(&mut mem));
    names.sort();
    println!("objects: {names:?}");
    let settings = timed("with", || {
        objshelf::with(&mut mem, &settings, |s| (s.name.clone(), s.brightness))
    })?;
    if let Some((name, brightness)) = settings {
        println!("settings: {name:?}, brightness {brightness}");
    }
    let samples = timed("with", || {
        objshelf::with(&mut mem, &samples, |s| (s.0.len(), s.0.last().copied()))
    })?;
    if let Some((count, last)) = samples {
        println!("samples: {count}, last {last:?}");
    }
    let pattern = timed("with", || {
        objshelf::with(&mut mem, &pattern, Pattern::summary)
    })?;
    if let Some((steps, edits, notes)) = pattern {
        println!("pattern: {steps} steps, {edits} edits, sum of notes {notes}");
    }
    println!("usage: {:?}", timed("usage", || objshelf::usage(&mut mem)));
    match timed("wear", || objshelf::wear(&mut mem)) {
        Some(wear) => println!("wear: {wear:?}"),
        None => println!("wear: not tracked by this backend"),
    }
    Ok(())
}
