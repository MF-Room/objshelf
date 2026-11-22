//! The app of `examples/rtic_nor`, on std: the same objects and the same steps, with threads in
//! place of the RTIC tasks, and the file system backend in place of the NOR flash.
//!
//! ```text
//! cargo run --example std_fs --features fs [dir]
//! ```
//!
//! The objects are kept in `dir`, by default `objshelf-std-fs` in the temporary directory. Run it
//! again: the boot count goes up, and the settings and samples carry over.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

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

/// Work for the saver thread, which stands in for the RTIC tasks of priority 1.
enum Save {
    /// `save`: writes all the changes.
    All,
    /// `save_settings`: writes only the settings.
    Settings(Handle<Settings>),
    /// `report`: prints the state of the memory, then the thread ends.
    Report(Handle<Settings>, Handle<Samples>),
}

/// The saver, for `on_full`: a `fn()` can't capture it, so it is in a static.
static SAVER: OnceLock<Sender<Save>> = OnceLock::new();

/// The functions of objshelf take anything that implements `objshelf::Mutex`. With RTIC, that is
/// the resource proxy (`&mut cx.shared.mem`); here, a std mutex shared by the threads.
struct Shared<'a>(&'a Mutex<Mem>);

impl objshelf::Mutex for Shared<'_> {
    type T = Mem;

    fn lock<R>(&mut self, f: impl FnOnce(&mut Mem) -> R) -> R {
        f(&mut self.0.lock().expect("no thread panics with the lock"))
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
    let mut mem = Mem::mount(config, CACHE_BYTES, || {
        if let Some(saver) = SAVER.get() {
            saver.send(Save::All).ok();
        }
    })?;

    // Before the memory is shared, the functions take it directly, as in RTIC's `init`. Each object
    // is found after a restart, or created on the first one.
    let boots = objshelf::get_or_insert_with(&mut mem, "boots", || 0u32)?.expect("fits");
    let boot = objshelf::with_mut(&mut mem, &boots, |n| {
        *n += 1;
        *n
    })?
    .expect("nothing else has the memory");
    objshelf::flush_object(&mut mem, &boots)?;
    println!("boot {boot}");
    let settings = objshelf::get_or_insert_with(&mut mem, "settings", Settings::default)?;
    let settings = settings.expect("fits");
    let samples = objshelf::get_or_insert_with(&mut mem, "samples", Samples::default)?;
    let samples = samples.expect("fits");

    let mem = Mutex::new(mem);
    thread::scope(|scope| {
        let thread = scope.spawn(|| save(Shared(&mem), work));
        let done = app(Shared(&mem), &saver, settings, samples);
        // Also on an error, so that the saver thread ends.
        saver.send(Save::Report(settings, samples)).ok();
        let saved = thread.join().expect("the saver thread doesn't panic");
        done.and(saved)
    })?;
    // Flushes what is left, as RTIC's `report` does.
    mem.into_inner()
        .expect("no thread panics with the lock")
        .unmount()
}

/// The RTIC task `app`, of priority 2. Here the threads run side by side, where RTIC's `app`
/// preempts the flushes: either way, it can find the storage busy, and then gets `Ok(None)`.
fn app(
    mut mem: Shared,
    saver: &Sender<Save>,
    settings: Handle<Settings>,
    samples: Handle<Samples>,
) -> Result<(), Error<FsError>> {
    for round in 1..=ROUNDS {
        thread::sleep(Duration::from_millis(PERIOD_MS.into()));
        // Reads the object from the backend if it is not in RAM. The change stays in RAM until
        // the saver writes it, straight away.
        let brightness = objshelf::with_mut(&mut mem, &settings, |s| {
            s.brightness = s.brightness.wrapping_add(1);
            s.brightness
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
        let added = objshelf::with_mut(&mut mem, &samples, |s| {
            if s.0.len() == SAMPLES {
                s.0.remove(0);
            }
            s.0.push(reading);
        })?;
        if added.is_none() {
            println!("round {round}: busy, sample {reading} dropped");
        }
        // The samples are written with the other changes, every few rounds.
        if round % SAVE_EVERY == 0 {
            saver.send(Save::All).ok();
        }
    }
    Ok(())
}

/// The RTIC tasks of priority 1: `save`, `save_settings` and `report`. One thread runs them all,
/// as RTIC runs tasks of the same priority one after the other, so no flush finds another one
/// running, and none gets `Ok(None)`.
fn save(mut mem: Shared, work: Receiver<Save>) -> Result<(), Error<FsError>> {
    for job in work {
        match job {
            Save::All => {
                objshelf::flush(&mut mem)?;
                println!("saved all the changes");
            }
            Save::Settings(settings) => {
                objshelf::flush_object(&mut mem, &settings)?;
            }
            Save::Report(settings, samples) => return report(mem, settings, samples),
        }
    }
    Ok(())
}

/// The RTIC task `report`: the state of the memory once the rounds are done.
fn report(
    mut mem: Shared,
    settings: Handle<Settings>,
    samples: Handle<Samples>,
) -> Result<(), Error<FsError>> {
    objshelf::flush(&mut mem)?;
    let mut names = objshelf::list(&mut mem);
    names.sort();
    println!("objects: {names:?}");
    if let Some((name, brightness)) =
        objshelf::with(&mut mem, &settings, |s| (s.name.clone(), s.brightness))?
    {
        println!("settings: {name:?}, brightness {brightness}");
    }
    if let Some((count, last)) =
        objshelf::with(&mut mem, &samples, |s| (s.0.len(), s.0.last().copied()))?
    {
        println!("samples: {count}, last {last:?}");
    }
    println!("usage: {:?}", objshelf::usage(&mut mem));
    match objshelf::wear(&mut mem) {
        Some(wear) => println!("wear: {wear:?}"),
        None => println!("wear: not tracked by this backend"),
    }
    Ok(())
}
