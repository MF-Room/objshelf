//! objshelf on the MSeq board: RTIC 2 on its STM32F413, and the NOR backend on its W25Q16JV. The
//! same app runs on std, with the file system backend: `examples/std_fs.rs`.
//!
//! The app keeps four objects across reboots: a boot count, settings written each time they
//! change, and samples and a pattern written every few rounds. The pattern, of about 1 KiB, spans
//! several pages of the flash. At boot, the app prints the objects it found from the last run, or
//! created on the first one. After 20 rounds, it prints a report and stops. The report ends with
//! the time each objshelf function took: its calls, their mean and the longest, from the cycle
//! counter.
//!
//! ```text
//! cd examples/rtic_nor
//! cargo run --release
//! ```
//!
//! probe-rs flashes the board through a debug probe and prints its RTT output. **`REGION` of the
//! W25Q16JV is erased: what it held is lost.**

#![no_std]
#![no_main]

extern crate alloc;

mod w25q16jv;

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::MaybeUninit;
use core::time::Duration;

use cortex_m::peripheral::DWT;
use embedded_alloc::LlffHeap as Heap;
use get_size2::GetSize;
use panic_rtt_target as _;
use rtic::Mutex;
use rtt_target::rprintln;
use serde::{Deserialize, Serialize};

/// Rounds of the app, one per `PERIOD_MS`, before the report.
const ROUNDS: u32 = 20;
const PERIOD_MS: u32 = 250;
/// Rounds between two flushes of all the changes.
const SAVE_EVERY: u32 = 5;
/// Samples kept, the oldest dropped first.
const SAMPLES: usize = 64;
/// RAM budget of the objects.
const CACHE_BYTES: usize = 16 * 1024;

/// The core clock, which the cycle counter counts.
const SYSCLK_HZ: u32 = 100_000_000;

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

/// The handles of the objects the tasks use: `Copy`, as each handle is, so it can be a spawn
/// argument.
#[derive(Clone, Copy)]
struct Objects {
    settings: objshelf::Handle<Settings>,
    samples: objshelf::Handle<Samples>,
    pattern: objshelf::Handle<Pattern>,
}

/// The calls to one objshelf function, in cycles of the core clock.
#[derive(Default)]
struct Time {
    calls: u32,
    total: u64,
    longest: u32,
}

/// The calls to each objshelf function, by name, from all the tasks. Public: RTIC makes its
/// resources public.
#[derive(Default)]
pub struct Times(BTreeMap<&'static str, Time>);

/// With exclusive access, in `init`: locking is free.
impl Mutex for Times {
    type T = Self;

    fn lock<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        f(self)
    }
}

impl Times {
    fn print(&self) {
        rprintln!("time in objshelf:");
        rprintln!(
            "  {:<18} {:>5} {:>10} {:>10}",
            "function",
            "calls",
            "mean",
            "longest"
        );
        for (name, time) in &self.0 {
            let mean = time.total / u64::from(time.calls);
            let longest = time.longest.into();
            rprintln!(
                "  {:<18} {:>5} {:>10.1?} {:>10.1?}",
                name,
                time.calls,
                duration(mean),
                duration(longest),
            );
        }
    }
}

fn duration(cycles: u64) -> Duration {
    Duration::from_nanos(cycles * 1_000_000_000 / u64::from(SYSCLK_HZ))
}

/// Runs `call`, a call to the objshelf function `name`, and adds its time to `times`. In the tasks
/// below `app`, that includes the time a task above preempts them. The cycle counter wraps after
/// 42 s, far more than a call takes.
fn timed<R>(times: &mut impl Mutex<T = Times>, name: &'static str, call: impl FnOnce() -> R) -> R {
    let start = DWT::cycle_count();
    let result = call();
    let cycles = DWT::cycle_count().wrapping_sub(start);
    times.lock(|times| {
        let time = times.0.entry(name).or_default();
        time.calls += 1;
        time.total += u64::from(cycles);
        time.longest = time.longest.max(cycles);
    });
    result
}

/// The heap, for objshelf and the objects: twice the RAM budget, for the serialized objects of a
/// flush and the rest.
const HEAP_SIZE: usize = 32 * 1024;

#[global_allocator]
static HEAP: Heap = Heap::empty();

fn init_heap() {
    static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
    // SAFETY: called once, in `init`, before any allocation; nothing else uses `HEAP_MEM`.
    unsafe { HEAP.init(&raw mut HEAP_MEM as usize, HEAP_SIZE) }
}

// One dispatcher per priority of the tasks: `report`, the saves and `app`.
#[rtic::app(device = stm32f4xx_hal::pac, dispatchers = [ADC, DMA1_STREAM0, DMA2_STREAM5])]
mod app {
    use core::ops::Range;

    use embedded_hal_bus::spi::{ExclusiveDevice, NoDelay};
    use objshelf::backend::nor::{NorBackend, NorConfig};
    use objshelf::{Handle, ObjShelf};
    use rtic_monotonics::systick::prelude::*;
    use rtt_target::{ChannelMode, rprintln, rtt_init_print};
    use stm32f4xx_hal::gpio::{AnyPin, Output, PinState, Speed};
    use stm32f4xx_hal::pac::SPI2;
    use stm32f4xx_hal::prelude::*;
    use stm32f4xx_hal::rcc::Config;
    use stm32f4xx_hal::spi::{Mode, Phase, Polarity, Spi};

    use super::*;
    use crate::w25q16jv::W25q16jv;

    /// Part of the W25Q16JV used by objshelf: its first 64 sectors of 4 KiB. Erased when needed,
    /// so what it held is lost.
    const REGION: Range<u32> = 0..64 * 4096;

    systick_monotonic!(Mono, 1000);

    type Flash = W25q16jv<ExclusiveDevice<Spi<SPI2>, AnyPin<Output>, NoDelay>>;
    type Mem = ObjShelf<NorBackend<Flash>>;

    #[shared]
    struct Shared {
        mem: Mem,
        times: Times,
    }

    #[local]
    struct Local {
        objects: Objects,
    }

    #[init]
    fn init(cx: init::Context) -> (Shared, Local) {
        // Room for the whole report, printed at once.
        rtt_init_print!(ChannelMode::NoBlockSkip, 4096);
        init_heap();
        let mut rcc = cx
            .device
            .RCC
            .freeze(Config::hse(25.MHz()).sysclk(SYSCLK_HZ.Hz()));
        Mono::start(cx.core.SYST, SYSCLK_HZ);
        // The cycle counter, for the times of the objshelf functions.
        let mut dcb = cx.core.DCB;
        dcb.enable_trace();
        let mut dwt = cx.core.DWT;
        dwt.enable_cycle_counter();

        // SPI2 at 25 MHz (APB1 / 2), mode 0. The FRAM is on the same bus: its chip select stays
        // high.
        let gpiob = cx.device.GPIOB.split(&mut rcc);
        let sck = gpiob.pb13.into_alternate::<5>().speed(Speed::High);
        let miso = gpiob.pb14.into_alternate::<5>();
        let mosi = gpiob.pb15.into_alternate::<5>().speed(Speed::High);
        let cs = gpiob.pb12.into_push_pull_output_in_state(PinState::High);
        let _fram_cs = gpiob.pb10.into_push_pull_output_in_state(PinState::High);
        let mode = Mode {
            polarity: Polarity::IdleLow,
            phase: Phase::CaptureOnFirstTransition,
        };
        let pins = (Some(sck), Some(miso), Some(mosi));
        let spi = Spi::new(cx.device.SPI2, pins, mode, 25.MHz(), &mut rcc);
        let spi = ExclusiveDevice::new_no_delay(spi, cs.erase()).unwrap();
        let mut delay = cx.device.TIM5.delay_us(&mut rcc);
        let flash = W25q16jv::new(spi, &mut delay).unwrap();

        // Blocks while the flash is mounted. Objects in RAM take about 16 KiB. The closure runs
        // when unflushed changes fill that budget: here it starts the `save` task.
        let config = NorConfig {
            flash,
            range: REGION,
        };
        let mut times = Times::default();
        let mut mem = timed(&mut times, "mount", || {
            Mem::mount(config, CACHE_BYTES, || {
                save::spawn().ok();
            })
        })
        .unwrap();

        // Before `mem` is shared, the functions take it directly. Each object is found after a
        // reboot, or created on the first boot: the names stored by the earlier runs tell which.
        let stored = timed(&mut times, "list", || objshelf::list(&mut mem));
        let origin = |name: &str| {
            if stored.iter().any(|stored| stored == name) {
                "found"
            } else {
                "created"
            }
        };
        let boots = timed(&mut times, "get_or_insert_with", || {
            objshelf::get_or_insert_with(&mut mem, "boots", || 0u32)
        })
        .unwrap()
        .expect("fits");
        let boot = timed(&mut times, "with_mut", || {
            objshelf::with_mut(&mut mem, &boots, |n| {
                *n += 1;
                *n
            })
        })
        .unwrap()
        .expect("nothing else has the memory");
        timed(&mut times, "flush_object", || {
            objshelf::flush_object(&mut mem, &boots)
        })
        .unwrap();
        rprintln!("boot {}", boot);
        let settings = timed(&mut times, "get_or_insert_with", || {
            objshelf::get_or_insert_with(&mut mem, "settings", Settings::default)
        })
        .unwrap()
        .expect("fits");
        let samples = timed(&mut times, "get_or_insert_with", || {
            objshelf::get_or_insert_with(&mut mem, "samples", Samples::default)
        })
        .unwrap()
        .expect("fits");
        let pattern = timed(&mut times, "get_or_insert_with", || {
            objshelf::get_or_insert_with(&mut mem, "pattern", Pattern::default)
        })
        .unwrap()
        .expect("fits");

        // What the earlier runs left, before this one changes it.
        let (name, brightness) = timed(&mut times, "with", || {
            objshelf::with(&mut mem, &settings, |s| (s.name.clone(), s.brightness))
        })
        .unwrap()
        .expect("nothing else has the memory");
        rprintln!(
            "{} settings: {:?}, brightness {}",
            origin("settings"),
            name,
            brightness
        );
        let (count, last) = timed(&mut times, "with", || {
            objshelf::with(&mut mem, &samples, |s| (s.0.len(), s.0.last().copied()))
        })
        .unwrap()
        .expect("nothing else has the memory");
        rprintln!("{} samples: {}, last {:?}", origin("samples"), count, last);
        let (steps, edits, notes) = timed(&mut times, "with", || {
            objshelf::with(&mut mem, &pattern, Pattern::summary)
        })
        .unwrap()
        .expect("nothing else has the memory");
        rprintln!(
            "{} pattern: {} steps, {} edits, sum of notes {}",
            origin("pattern"),
            steps,
            edits,
            notes
        );

        let objects = Objects {
            settings,
            samples,
            pattern,
        };
        app::spawn().unwrap();
        (Shared { mem, times }, Local { objects })
    }

    // Above the tasks that write: it keeps running during a flush, so it can find the storage
    // busy, and then gets `Ok(None)`.
    #[task(priority = 3, shared = [mem, times], local = [objects])]
    async fn app(mut cx: app::Context) {
        let objects = *cx.local.objects;
        let Objects {
            settings,
            samples,
            pattern,
        } = objects;
        let mem = &mut cx.shared.mem;
        let times = &mut cx.shared.times;
        for round in 1..=ROUNDS {
            Mono::delay(PERIOD_MS.millis()).await;
            // Reads the object from the backend if it is not in RAM. The change stays in RAM
            // until `save_settings` writes it, straight away.
            let brightness = timed(times, "with_mut", || {
                objshelf::with_mut(mem, &settings, |s| {
                    s.brightness = s.brightness.wrapping_add(1);
                    s.brightness
                })
            })
            .unwrap();
            match brightness {
                Some(brightness) => {
                    rprintln!("round {}: brightness {}", round, brightness);
                    // Already spawned: the next flush writes this change.
                    save_settings::spawn(settings).ok();
                }
                // A flush has the storage and the settings are not in RAM, or unflushed changes
                // fill the budget (`on_full` spawned `save`): try again next round.
                None => {
                    rprintln!("round {}: busy, next round", round);
                }
            }

            let reading = read_sensor(round);
            let added = timed(times, "with_mut", || {
                objshelf::with_mut(mem, &samples, |s| {
                    if s.0.len() == SAMPLES {
                        s.0.remove(0);
                    }
                    s.0.push(reading);
                })
            })
            .unwrap();
            if added.is_none() {
                rprintln!("round {}: busy, sample {} dropped", round, reading);
            }
            let edited = timed(times, "with_mut", || {
                objshelf::with_mut(mem, &pattern, Pattern::edit)
            })
            .unwrap();
            if edited.is_none() {
                rprintln!("round {}: busy, pattern edit dropped", round);
            }
            // The samples and the pattern are written with the other changes, every few rounds.
            if round % SAVE_EVERY == 0 {
                save::spawn().ok();
            }
        }
        report::spawn(objects).ok();
    }

    // The tasks that write to the backend, below `app`, which keeps running meanwhile. The two
    // saves have the same priority: neither interrupts the other, so neither gets `None`.

    // Writes all the changes. Spawned by `on_full`: only a full flush frees the whole budget.
    #[task(priority = 2, shared = [mem, times])]
    async fn save(mut cx: save::Context) {
        let mem = &mut cx.shared.mem;
        timed(&mut cx.shared.times, "flush", || objshelf::flush(mem)).unwrap();
        rprintln!("saved all the changes");
    }

    // Writes only the settings: the other changes stay in RAM until `save` runs. A handle is
    // `Copy`, so it can be a spawn argument.
    #[task(priority = 2, shared = [mem, times])]
    async fn save_settings(mut cx: save_settings::Context, settings: Handle<Settings>) {
        let mem = &mut cx.shared.mem;
        timed(&mut cx.shared.times, "flush_object", || {
            objshelf::flush_object(mem, &settings)
        })
        .unwrap();
    }

    // The state of the memory once the rounds are done, then a stop. Below the saves, so it runs
    // after the last ones `app` spawns. Its flush writes at another priority than theirs, so a save
    // during it would get `None`, but nothing spawns one then: `app` is done, and `on_full` only
    // runs on changes, which `report` doesn't make.
    #[task(priority = 1, shared = [mem, times])]
    async fn report(mut cx: report::Context, objects: Objects) {
        let Objects {
            settings,
            samples,
            pattern,
        } = objects;
        let mem = &mut cx.shared.mem;
        let times = &mut cx.shared.times;
        timed(times, "flush", || objshelf::flush(mem)).unwrap();
        let mut names = timed(times, "list", || objshelf::list(mem));
        names.sort();
        rprintln!("objects: {:?}", names);
        let settings = timed(times, "with", || {
            objshelf::with(mem, &settings, |s| (s.name.clone(), s.brightness))
        });
        if let Some((name, brightness)) = settings.unwrap() {
            rprintln!("settings: {:?}, brightness {}", name, brightness);
        }
        let samples = timed(times, "with", || {
            objshelf::with(mem, &samples, |s| (s.0.len(), s.0.last().copied()))
        });
        if let Some((count, last)) = samples.unwrap() {
            rprintln!("samples: {}, last {:?}", count, last);
        }
        let pattern = timed(times, "with", || {
            objshelf::with(mem, &pattern, Pattern::summary)
        });
        if let Some((steps, edits, notes)) = pattern.unwrap() {
            rprintln!(
                "pattern: {} steps, {} edits, sum of notes {}",
                steps,
                edits,
                notes
            );
        }
        rprintln!(
            "usage: {:?}",
            timed(times, "usage", || objshelf::usage(mem))
        );
        if let Some(wear) = timed(times, "wear", || objshelf::wear(mem)) {
            rprintln!("wear: {:?}", wear);
        }
        times.lock(|times| times.print());
        // A semihosting exit: `probe-rs run` ends with success. Without a debugger, it faults.
        cortex_m_semihosting::debug::exit(cortex_m_semihosting::debug::EXIT_SUCCESS);
    }
}
