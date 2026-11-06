use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::future::{Future, poll_fn};
use core::pin::pin;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll, Waker};
use std::task::Wake;

use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use get_size2::GetSize;

use super::{Runner, TierMem};
use crate::backend::Backend;
use crate::backend::nor::mock::{Cut, MockError, MockFlash};
use crate::backend::nor::{NorBackend, NorConfig, NorError};
use crate::{Error, Handle, Mutex, ObjectId, flush, mounted, storable, unmount, with, with_mut};

type Flash = MockFlash<1024>;
type Mem = TierMem<NorBackend<Flash>>;

const CACHE: usize = 4096;

/// A lock like RTIC's. The `RefCell` panics if a lock is taken while another is held, as when a
/// lock is kept across an `.await`.
#[derive(Clone, Copy)]
struct Shared<'a>(&'a RefCell<Mem>);

impl Mutex for Shared<'_> {
    type T = Mem;

    fn lock<R>(&mut self, f: impl FnOnce(&mut Mem) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
}

struct Flag(AtomicBool);

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Polls only after a wake-up, like an RTIC executor: a lost wake-up panics instead of hanging.
fn block_on<T>(future: impl Future<Output = T>) -> T {
    let flag = Arc::new(Flag(AtomicBool::new(true)));
    let waker = Waker::from(flag.clone());
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    for _ in 0..1_000_000 {
        assert!(
            flag.0.swap(false, Ordering::SeqCst),
            "stalled: pending without a wake-up"
        );
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
    panic!("still pending after 1000000 polls");
}

async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            return Poll::Ready(());
        }
        yielded = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    })
    .await;
}

/// Waits until the runner has nothing left to do.
async fn idle(mem: &mut Shared<'_>) {
    loop {
        yield_now().await;
        if mem.lock(|m| m.runner_waker.is_some()) {
            return;
        }
    }
}

fn config(flash: Flash) -> NorConfig<Flash> {
    let range = 0..flash.data.len() as u32;
    NorConfig { flash, range }
}

/// Runs `body` next to the runner, then drops both as on a power cut. Returns the result of
/// `body` and the flash, to mount it again.
fn session<R>(
    flash: Flash,
    cache_bytes: usize,
    body: impl AsyncFnOnce(Shared<'_>) -> R,
) -> (R, Flash) {
    let cell = RefCell::new(Mem::new(cache_bytes));
    let mut runner = Runner::new(config(flash));
    let result = block_on(async {
        let mut lock = Shared(&cell);
        match select(runner.run(&mut lock), body(Shared(&cell))).await {
            Either::First(result) => panic!("the runner stopped: {result:?}"),
            Either::Second(result) => result,
        }
    });
    let flash = runner.into_backend().expect("not mounted").into_flash();
    (result, flash)
}

fn reboot(flash: Flash) -> Flash {
    Flash::with_data(flash.data)
}

fn names(mem: &mut Shared<'_>) -> Vec<String> {
    mem.lock(|m| m.list().unwrap().map(String::from).collect())
}

#[storable]
#[derive(Debug, Default, Clone, PartialEq)]
struct Settings {
    brightness: u8,
    name: String,
}

#[storable]
#[derive(Debug, Clone, PartialEq)]
struct Blob(Vec<u8>);

#[test]
fn insert_flush_reboot_and_load() {
    let ((), flash) = session(Flash::new(8), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        let settings = Settings {
            brightness: 3,
            name: "main".to_string(),
        };
        let handle = mem.lock(|m| m.insert("settings", settings)).unwrap();
        with_mut(&mut mem, &handle, |s| s.brightness = 7)
            .await
            .unwrap();
        flush(&mut mem).await.unwrap();
    });

    session(reboot(flash), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        assert_eq!(names(&mut mem), ["settings"]);
        let handle: Handle<Settings> = mem.lock(|m| m.get("settings")).unwrap();
        // Not in RAM after a mount: the first try queues the load.
        assert_eq!(
            mem.lock(|m| m.try_with(&handle, |s| s.brightness)).unwrap(),
            None
        );
        let name = with(&mut mem, &handle, |s| s.name.clone()).await.unwrap();
        assert_eq!(name, "main");
        assert_eq!(
            mem.lock(|m| m.try_with(&handle, |s| s.brightness)).unwrap(),
            Some(7)
        );
    });
}

#[test]
fn try_with_returns_once_loaded() {
    let ((), flash) = session(Flash::new(8), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        mem.lock(|m| m.insert("blob", Blob(vec![1, 2, 3]))).unwrap();
        flush(&mut mem).await.unwrap();
    });

    session(reboot(flash), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        let handle: Handle<Blob> = mem.lock(|m| m.get("blob")).unwrap();
        let mut tries = 0;
        loop {
            tries += 1;
            let done = mem.lock(|m| m.try_with_mut(&handle, |b| b.0.push(4)));
            if done.unwrap().is_some() {
                break;
            }
            yield_now().await;
        }
        assert!(tries > 1);
        let blob = with(&mut mem, &handle, Blob::clone).await.unwrap();
        assert_eq!(blob, Blob(vec![1, 2, 3, 4]));
    });
}

#[test]
fn changes_not_flushed_are_lost_on_reset() {
    let ((), flash) = session(Flash::new(8), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        let handle = mem.lock(|m| m.insert("kept", 1u32)).unwrap();
        flush(&mut mem).await.unwrap();
        with_mut(&mut mem, &handle, |v| *v = 2).await.unwrap();
        mem.lock(|m| m.insert("lost", 3u32)).unwrap();
    });

    session(reboot(flash), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        assert_eq!(names(&mut mem), ["kept"]);
        let handle: Handle<u32> = mem.lock(|m| m.get("kept")).unwrap();
        assert_eq!(with(&mut mem, &handle, |v| *v).await.unwrap(), 1);
    });
}

#[test]
fn get_or_insert_with_inserts_once() {
    let ((), flash) = session(Flash::new(8), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        let first = mem
            .lock(|m| m.get_or_insert_with("settings", Settings::default))
            .unwrap();
        let again = mem
            .lock(|m| m.get_or_insert_with("settings", || -> Settings { unreachable!() }))
            .unwrap();
        assert_eq!(first.id, again.id);
        flush(&mut mem).await.unwrap();
    });

    session(reboot(flash), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        let handle = mem
            .lock(|m| m.get_or_insert_with("settings", || -> Settings { unreachable!() }))
            .unwrap();
        let settings = with(&mut mem, &handle, Settings::clone).await.unwrap();
        assert_eq!(settings, Settings::default());
    });
}

#[test]
fn eviction_writes_back_and_reloads() {
    // Each blob takes 24 + 100 bytes: two fit.
    let cache = 300;
    let blob = |i: u8| Blob(vec![i; 100]);
    let ((), flash) = session(Flash::new(16), cache, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        let mut handles = Vec::new();
        for i in 0..6 {
            let name = alloc::format!("blob{i}");
            handles.push(mem.lock(|m| m.insert(&name, blob(i))).unwrap());
            idle(&mut mem).await;
            assert!(mem.lock(|m| m.used) <= cache);
        }
        for (i, handle) in handles.iter().enumerate() {
            let read = with(&mut mem, handle, Blob::clone).await.unwrap();
            assert_eq!(read, blob(i as u8));
        }
        idle(&mut mem).await;
        assert!(mem.lock(|m| m.used) <= cache);
    });

    // Evicted objects were written with a directory naming them, without a flush.
    session(reboot(flash), cache, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        let names = names(&mut mem);
        assert!(!names.is_empty());
        for name in names {
            let i: u8 = name["blob".len()..].parse().unwrap();
            let handle: Handle<Blob> = mem.lock(|m| m.get(&name)).unwrap();
            assert_eq!(with(&mut mem, &handle, Blob::clone).await.unwrap(), blob(i));
        }
    });
}

#[test]
fn remove_reaches_the_backend_on_flush() {
    let ((), flash) = session(Flash::new(8), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        let a = mem.lock(|m| m.insert("a", 1u32)).unwrap();
        mem.lock(|m| m.insert("b", 2u32)).unwrap();
        flush(&mut mem).await.unwrap();
        // The directory and both objects.
        assert_eq!(mem.lock(|m| m.usage()).unwrap().objects, 3);

        mem.lock(|m| m.remove(a)).unwrap();
        assert!(matches!(mem.lock(|m| m.remove(a)), Err(Error::NotFound)));
        assert!(matches!(
            with(&mut mem, &a, |v| *v).await,
            Err(Error::NotFound)
        ));
        flush(&mut mem).await.unwrap();
        assert_eq!(mem.lock(|m| m.usage()).unwrap().objects, 2);
    });

    session(reboot(flash), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        assert_eq!(names(&mut mem), ["b"]);
    });
}

#[test]
fn remove_before_first_write() {
    session(Flash::new(8), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        let a = mem.lock(|m| m.insert("a", 1u32)).unwrap();
        mem.lock(|m| m.remove(a)).unwrap();
        flush(&mut mem).await.unwrap();
        assert_eq!(names(&mut mem), Vec::<String>::new());
        // Only the directory, which changed.
        assert_eq!(mem.lock(|m| m.usage()).unwrap().objects, 1);
        // The name can be used again.
        mem.lock(|m| m.insert("a", 2u32)).unwrap();
    });
}

#[test]
fn mount_removes_orphans() {
    let mut backend = block_on(NorBackend::mount(config(Flash::new(8)))).unwrap();
    block_on(backend.write(ObjectId(5), &7u32)).unwrap();
    let flash = reboot(backend.into_flash());

    let ((), flash) = session(flash, CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        assert_eq!(mem.lock(|m| m.usage()).unwrap().objects, 0);
        // The orphan's id can be reused: nothing is left of it.
        let handle = mem.lock(|m| m.insert("new", 1u32)).unwrap();
        flush(&mut mem).await.unwrap();
        assert_eq!(with(&mut mem, &handle, |v| *v).await.unwrap(), 1);
    });

    session(reboot(flash), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        let handle: Handle<u32> = mem.lock(|m| m.get("new")).unwrap();
        assert_eq!(with(&mut mem, &handle, |v| *v).await.unwrap(), 1);
    });
}

#[test]
fn errors() {
    let mut mem = Mem::new(64);
    assert!(matches!(mem.get::<u32>("x"), Err(Error::Unavailable)));
    let missing = Handle::<u32>::new(ObjectId(1));
    assert!(matches!(
        mem.try_with(&missing, |v| *v),
        Err(Error::Unavailable)
    ));

    session(Flash::new(8), 64, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        assert!(matches!(
            mem.lock(|m| m.get::<u32>("x")),
            Err(Error::NotFound)
        ));
        assert!(matches!(
            mem.lock(|m| m.try_with(&missing, |v| *v)),
            Err(Error::NotFound)
        ));
        assert!(matches!(
            mem.lock(|m| m.insert("big", Blob(vec![0; 100]))),
            Err(Error::TooLarge)
        ));
        mem.lock(|m| m.insert("x", 1u32)).unwrap();
        assert!(matches!(
            mem.lock(|m| m.insert("x", 2u32)),
            Err(Error::AlreadyExists)
        ));
        let wrong: Handle<u8> = mem.lock(|m| m.get("x")).unwrap();
        assert!(matches!(
            with(&mut mem, &wrong, |v| *v).await,
            Err(Error::WrongType)
        ));
    });
}

#[test]
fn flush_returns_backend_errors() {
    // The first erase, when the first object is written, cuts the power.
    let flash = Flash::new(8).cut_at(0, Cut::FirstHalf);
    session(flash, CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        mem.lock(|m| m.insert("a", 1u32)).unwrap();
        let result = flush(&mut mem).await;
        assert!(matches!(
            result,
            Err(Error::Backend(NorError::Flash(MockError::PowerCut)))
        ));
        assert!(!mem.lock(|m| m.is_clean()));
        assert!(matches!(unmount(&mut mem).await, Err(Error::Backend(_))));
    });
}

#[test]
fn unmount_stops_the_runner() {
    let cell = RefCell::new(Mem::new(CACHE));
    let mut runner = Runner::new(config(Flash::new(8)));
    let (run, result) = block_on(async {
        let mut lock = Shared(&cell);
        let mut mem = Shared(&cell);
        join(runner.run(&mut lock), async {
            mounted(&mut mem).await?;
            let handle = mem.lock(|m| m.insert("x", 1u32))?;
            with_mut(&mut mem, &handle, |v| *v = 2).await?;
            unmount(&mut mem).await
        })
        .await
    });
    run.unwrap();
    result.unwrap();
    assert!(matches!(
        cell.borrow().get::<u32>("x"),
        Err(Error::Unavailable)
    ));
    assert!(matches!(
        block_on(runner.run(&mut Shared(&cell))),
        Err(Error::Unavailable)
    ));
}

#[test]
fn slow_flash_interleaves_tasks() {
    let ((), flash) = session(Flash::new(8).pending(3), CACHE, async |mut mem| {
        mounted(&mut mem).await.unwrap();
        for i in 0..4u32 {
            mem.lock(|m| m.insert(&alloc::format!("n{i}"), i)).unwrap();
        }
        flush(&mut mem).await.unwrap();
    });

    session(reboot(flash).pending(3), CACHE, async |mem| {
        let (mut a, mut b) = (mem, mem);
        let task_a = async {
            mounted(&mut a).await.unwrap();
            for i in 0..4u32 {
                let handle: Handle<u32> = a.lock(|m| m.get(&alloc::format!("n{i}"))).unwrap();
                with_mut(&mut a, &handle, |v| *v += 10).await.unwrap();
            }
            flush(&mut a).await.unwrap();
        };
        let task_b = async {
            mounted(&mut b).await.unwrap();
            for i in (0..4u32).rev() {
                let handle: Handle<u32> = b.lock(|m| m.get(&alloc::format!("n{i}"))).unwrap();
                let value = with(&mut b, &handle, |v| *v).await.unwrap();
                assert!(value == i || value == i + 10);
            }
        };
        join(task_a, task_b).await;
        let mut mem = mem;
        for i in 0..4u32 {
            let handle: Handle<u32> = mem.lock(|m| m.get(&alloc::format!("n{i}"))).unwrap();
            assert_eq!(with(&mut mem, &handle, |v| *v).await.unwrap(), i + 10);
        }
    });
}

fn twice_len(text: &str) -> usize {
    2 * text.len()
}

#[storable]
#[derive(Debug, PartialEq)]
struct Wrapper<T> {
    items: Vec<T>,
    #[storable(ignore)]
    ignored: String,
    #[storable(heap_size = 10)]
    fixed: u8,
    #[storable(heap_size_fn = twice_len)]
    text: String,
}

#[storable]
#[derive(Debug, PartialEq)]
enum Shape {
    Empty,
    Circle(u32),
    Named { name: String },
}

#[test]
fn storable_macro() {
    let wrapper = Wrapper {
        items: vec![1u16, 2, 3],
        ignored: "not counted".to_string(),
        fixed: 0,
        text: "abcd".to_string(),
    };
    assert_eq!(wrapper.get_heap_size(), 3 * 2 + 10 + 2 * 4);
    let bytes = postcard::to_allocvec(&wrapper).unwrap();
    assert_eq!(
        postcard::from_bytes::<Wrapper<u16>>(&bytes).unwrap(),
        wrapper
    );

    assert_eq!(Shape::Empty.get_heap_size(), 0);
    assert_eq!(Shape::Circle(3).get_heap_size(), 0);
    let named = Shape::Named {
        name: "ab".to_string(),
    };
    assert_eq!(named.get_heap_size(), 2);
    let bytes = postcard::to_allocvec(&named).unwrap();
    assert_eq!(postcard::from_bytes::<Shape>(&bytes).unwrap(), named);

    assert_eq!(Blob(vec![0; 5]).get_heap_size(), 5);
}
