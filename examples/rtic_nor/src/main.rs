//! objshelf on the MSeq board: RTIC 2 on its STM32F413, and the NOR backend on its W25Q16JV. The
//! same app runs on std, with the file system backend: `examples/std_fs.rs`.
//!
//! The app keeps three objects across reboots: a boot count, settings written each time they
//! change, and samples written every few rounds. After 20 rounds, it prints a report and stops.
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

use alloc::string::String;
use alloc::vec::Vec;
use core::mem::MaybeUninit;

use embedded_alloc::LlffHeap as Heap;
use get_size2::GetSize;
use panic_rtt_target as _;
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

#[rtic::app(device = stm32f4xx_hal::pac, dispatchers = [ADC, DMA1_STREAM0])]
mod app {
    use core::ops::Range;

    use embedded_hal_bus::spi::{ExclusiveDevice, NoDelay};
    use objshelf::backend::nor::{NorBackend, NorConfig};
    use objshelf::{Handle, ObjShelf};
    use rtic_monotonics::systick::prelude::*;
    use rtt_target::{rprintln, rtt_init_print};
    use stm32f4xx_hal::gpio::{AnyPin, Output, PinState, Speed};
    use stm32f4xx_hal::pac::SPI2;
    use stm32f4xx_hal::prelude::*;
    use stm32f4xx_hal::rcc::Config;
    use stm32f4xx_hal::spi::{Mode, Phase, Polarity, Spi};

    use super::*;
    use crate::w25q16jv::W25q16jv;

    const SYSCLK_HZ: u32 = 100_000_000;

    /// Part of the W25Q16JV used by objshelf: its first 64 sectors of 4 KiB. Erased when needed,
    /// so what it held is lost.
    const REGION: Range<u32> = 0..64 * 4096;

    systick_monotonic!(Mono, 1000);

    type Flash = W25q16jv<ExclusiveDevice<Spi<SPI2>, AnyPin<Output>, NoDelay>>;
    type Mem = ObjShelf<NorBackend<Flash>>;

    #[shared]
    struct Shared {
        mem: Mem,
    }

    #[local]
    struct Local {
        // A handle is a `Copy` id of the object.
        settings: Handle<Settings>,
        samples: Handle<Samples>,
    }

    #[init]
    fn init(cx: init::Context) -> (Shared, Local) {
        rtt_init_print!();
        init_heap();
        let mut rcc = cx
            .device
            .RCC
            .freeze(Config::hse(25.MHz()).sysclk(SYSCLK_HZ.Hz()));
        Mono::start(cx.core.SYST, SYSCLK_HZ);

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
        let mut mem = Mem::mount(config, CACHE_BYTES, || {
            save::spawn().ok();
        })
        .unwrap();

        // Before `mem` is shared, the functions take it directly. Each object is found after a
        // reboot, or created on the first boot.
        let boots = objshelf::get_or_insert_with(&mut mem, "boots", || 0u32)
            .unwrap()
            .expect("fits");
        let boot = objshelf::with_mut(&mut mem, &boots, |n| {
            *n += 1;
            *n
        })
        .unwrap()
        .expect("nothing else has the memory");
        objshelf::flush_object(&mut mem, &boots).unwrap();
        rprintln!("boot {}", boot);
        let settings = objshelf::get_or_insert_with(&mut mem, "settings", Settings::default)
            .unwrap()
            .expect("fits");
        let samples = objshelf::get_or_insert_with(&mut mem, "samples", Samples::default)
            .unwrap()
            .expect("fits");

        app::spawn().unwrap();
        (Shared { mem }, Local { settings, samples })
    }

    // Above the tasks that write: it keeps running during a flush, so it can find the storage
    // busy, and then gets `Ok(None)`.
    #[task(priority = 2, shared = [mem], local = [settings, samples])]
    async fn app(mut cx: app::Context) {
        let (settings, samples) = (*cx.local.settings, *cx.local.samples);
        let mem = &mut cx.shared.mem;
        for round in 1..=ROUNDS {
            Mono::delay(PERIOD_MS.millis()).await;
            // Reads the object from the backend if it is not in RAM. The change stays in RAM
            // until `save_settings` writes it, straight away.
            let brightness = objshelf::with_mut(mem, &settings, |s| {
                s.brightness = s.brightness.wrapping_add(1);
                s.brightness
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
            let added = objshelf::with_mut(mem, &samples, |s| {
                if s.0.len() == SAMPLES {
                    s.0.remove(0);
                }
                s.0.push(reading);
            })
            .unwrap();
            if added.is_none() {
                rprintln!("round {}: busy, sample {} dropped", round, reading);
            }
            // The samples are written with the other changes, every few rounds.
            if round % SAVE_EVERY == 0 {
                save::spawn().ok();
            }
        }
        report::spawn(settings, samples).ok();
    }

    // The tasks that write to the backend, at the lowest priority: every task above them keeps
    // running meanwhile. With the same priority, none interrupts another, so none gets `None`.

    // Writes all the changes. Spawned by `on_full`: only a full flush frees the whole budget.
    #[task(priority = 1, shared = [mem])]
    async fn save(mut cx: save::Context) {
        objshelf::flush(&mut cx.shared.mem).unwrap();
        rprintln!("saved all the changes");
    }

    // Writes only the settings: the other changes stay in RAM until `save` runs. A handle is
    // `Copy`, so it can be a spawn argument.
    #[task(priority = 1, shared = [mem])]
    async fn save_settings(mut cx: save_settings::Context, settings: Handle<Settings>) {
        objshelf::flush_object(&mut cx.shared.mem, &settings).unwrap();
    }

    // The state of the memory once the rounds are done, then a stop.
    #[task(priority = 1, shared = [mem])]
    async fn report(mut cx: report::Context, settings: Handle<Settings>, samples: Handle<Samples>) {
        let mem = &mut cx.shared.mem;
        objshelf::flush(mem).unwrap();
        let mut names = objshelf::list(mem);
        names.sort();
        rprintln!("objects: {:?}", names);
        let settings = objshelf::with(mem, &settings, |s| (s.name.clone(), s.brightness));
        if let Some((name, brightness)) = settings.unwrap() {
            rprintln!("settings: {:?}, brightness {}", name, brightness);
        }
        let samples = objshelf::with(mem, &samples, |s| (s.0.len(), s.0.last().copied()));
        if let Some((count, last)) = samples.unwrap() {
            rprintln!("samples: {}, last {:?}", count, last);
        }
        rprintln!("usage: {:?}", objshelf::usage(mem));
        if let Some(wear) = objshelf::wear(mem) {
            rprintln!("wear: {:?}", wear);
        }
        // A semihosting exit: `probe-rs run` ends with success. Without a debugger, it faults.
        cortex_m_semihosting::debug::exit(cortex_m_semihosting::debug::EXIT_SUCCESS);
    }
}
