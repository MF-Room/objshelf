use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use super::mock::{Cut, MockError, MockFlash};
use super::{NorBackend, NorConfig, NorError};
use crate::ObjectId;
use crate::backend::{Backend, Usage};

/// 1 KiB sectors: a header page and 3 data pages, so garbage collection runs often.
type SmallFlash = MockFlash<1024>;

fn mount<const E: usize>(flash: MockFlash<E>) -> NorBackend<MockFlash<E>> {
    let range = 0..flash.data.len() as u32;
    NorBackend::mount(NorConfig { flash, range }).unwrap()
}

/// Mounts the same flash again, as after a reboot.
fn reboot<const E: usize>(backend: NorBackend<MockFlash<E>>) -> NorBackend<MockFlash<E>> {
    mount(MockFlash::with_data(backend.flash.data))
}

fn write<const E: usize, T: Serialize>(backend: &mut NorBackend<MockFlash<E>>, id: u32, value: &T) {
    backend.write(ObjectId(id), value).unwrap();
}

fn read<const E: usize>(backend: &mut NorBackend<MockFlash<E>>, id: u32) -> Option<Vec<u8>> {
    match backend.read(ObjectId(id)) {
        Ok(value) => Some(value),
        Err(NorError::NotFound) => None,
        Err(error) => panic!("reading object {id}: {error:?}"),
    }
}

fn bytes(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add(i as u8)).collect()
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Pattern {
    tempo: u16,
    steps: Vec<u8>,
}

#[test]
fn round_trip_and_reboot() {
    let mut backend = mount(SmallFlash::new(8));
    let pattern = Pattern {
        tempo: 120,
        steps: bytes(1, 16),
    };
    write(&mut backend, 1, &pattern);
    // 5 pages
    write(&mut backend, 2, &bytes(7, 1000));
    write(&mut backend, 3, &());

    let mut backend = reboot(backend);
    assert_eq!(backend.read::<Pattern>(ObjectId(1)).unwrap(), pattern);
    assert_eq!(read(&mut backend, 2), Some(bytes(7, 1000)));
    backend.read::<()>(ObjectId(3)).unwrap();
    assert_eq!(read(&mut backend, 4), None);
}

#[test]
fn w25q16jv_geometry() {
    let mut backend = mount(MockFlash::<4096>::new(512));
    write(&mut backend, 1, &bytes(3, 3000));
    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 1), Some(bytes(3, 3000)));
}

#[test]
fn update_and_remove() {
    let mut backend = mount(SmallFlash::new(4));
    write(&mut backend, 1, &bytes(1, 10));
    write(&mut backend, 1, &bytes(2, 300));
    assert_eq!(read(&mut backend, 1), Some(bytes(2, 300)));

    backend.remove(ObjectId(1)).unwrap();
    assert_eq!(read(&mut backend, 1), None);
    assert!(matches!(
        backend.remove(ObjectId(1)),
        Err(NorError::NotFound)
    ));

    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 1), None);
}

#[test]
fn garbage_collection_reuses_space() {
    let mut backend = mount(SmallFlash::new(4));
    for round in 0..300u32 {
        let id = round % 3;
        write(&mut backend, id, &bytes(round as u8, 100));
    }
    let mut backend = reboot(backend);
    for id in 0..3 {
        let last = 297 + id;
        assert_eq!(read(&mut backend, id), Some(bytes(last as u8, 100)));
    }
    // Wear is spread over every sector.
    let counts: Vec<u32> = backend.erase_counts().collect();
    let (min, max) = (counts.iter().min().unwrap(), counts.iter().max().unwrap());
    assert!(max - min <= 2, "erase counts {counts:?}");
}

#[test]
fn usage() {
    // 4 sectors of 3 data pages, one kept for garbage collection.
    let page = 235;
    let mut backend = mount(SmallFlash::new(4));
    let empty = Usage {
        capacity: 9 * page,
        used: 0,
        free: 9 * page,
        reclaimable: 0,
        objects: 0,
    };
    assert_eq!(backend.usage(), empty);

    write(&mut backend, 1, &bytes(1, 300));
    write(&mut backend, 2, &bytes(2, 10));
    let two_objects = Usage {
        used: 3 * page,
        free: 6 * page,
        objects: 2,
        ..empty
    };
    assert_eq!(backend.usage(), two_objects);

    // The old version of object 2 becomes garbage, then object 1 too.
    write(&mut backend, 2, &bytes(3, 10));
    backend.remove(ObjectId(1)).unwrap();
    let after_remove = Usage {
        used: page,
        free: 5 * page,
        reclaimable: 3 * page,
        objects: 1,
        ..empty
    };
    assert_eq!(backend.usage(), after_remove);
    assert_eq!(reboot(backend).usage(), after_remove);
}

#[test]
fn usage_adds_up_while_collecting() {
    let mut backend = mount(SmallFlash::new(4));
    for round in 0..100u32 {
        write(&mut backend, round % 3, &bytes(round as u8, 100));
        let usage = backend.usage();
        assert_eq!(usage.used + usage.free + usage.reclaimable, usage.capacity);
        assert_eq!(usage.objects, 3.min(round as usize + 1));
    }
}

#[test]
fn full_then_space_again() {
    let mut backend = mount(SmallFlash::new(3));
    let mut stored = 0;
    while backend.write(ObjectId(stored), &bytes(0, 50)).is_ok() {
        stored += 1;
    }
    // 3 sectors of 3 data pages, one sector kept for garbage collection.
    assert_eq!(stored, 6);
    assert!(matches!(
        backend.write(ObjectId(99), &0u8),
        Err(NorError::NoSpace)
    ));

    backend.remove(ObjectId(0)).unwrap();
    write(&mut backend, 99, &bytes(1, 50));
    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 99), Some(bytes(1, 50)));
    assert_eq!(read(&mut backend, 0), None);
}

#[test]
fn invalid_config() {
    let flash = SmallFlash::new(4);
    let result = NorBackend::mount(NorConfig {
        flash,
        range: 512..4096,
    });
    assert!(matches!(result, Err(NorError::InvalidConfig)));
}

enum Op {
    Write(u32, Vec<u8>),
    Remove(u32),
}

/// Updates and removals on a small flash, so that the sequence includes garbage collections.
fn ops() -> Vec<Op> {
    let mut ops = Vec::new();
    for round in 0..12u8 {
        ops.push(Op::Write(1, bytes(round, 20)));
        ops.push(Op::Write(2, bytes(round, 300)));
        if round % 3 == 2 {
            ops.push(Op::Remove(2));
        }
    }
    ops
}

fn apply(state: &mut BTreeMap<u32, Vec<u8>>, op: &Op) {
    match op {
        Op::Write(id, value) => {
            state.insert(*id, value.clone());
        }
        Op::Remove(id) => {
            state.remove(id);
        }
    }
}

/// Runs `ops` until one fails. Returns the flash and the index of the failed op.
fn run(flash: SmallFlash, ops: &[Op]) -> (SmallFlash, Option<usize>) {
    let mut backend = mount(flash);
    for (index, op) in ops.iter().enumerate() {
        let result = match op {
            Op::Write(id, value) => backend.write(ObjectId(*id), value),
            Op::Remove(id) => backend.remove(ObjectId(*id)),
        };
        if let Err(error) = result {
            assert!(
                matches!(error, NorError::Flash(MockError::PowerCut)),
                "{error:?}"
            );
            return (backend.flash, Some(index));
        }
    }
    (backend.flash, None)
}

fn stored(backend: &mut NorBackend<SmallFlash>) -> BTreeMap<u32, Vec<u8>> {
    [1, 2]
        .into_iter()
        .filter_map(|id| read(backend, id).map(|value| (id, value)))
        .collect()
}

#[test]
fn power_cut_at_every_operation() {
    let ops = ops();
    let (flash, failed) = run(SmallFlash::new(3), &ops);
    assert_eq!(failed, None);
    let total = flash.ops;
    let mut expected = BTreeMap::new();
    ops.iter().for_each(|op| apply(&mut expected, op));
    assert_eq!(
        stored(&mut mount(MockFlash::with_data(flash.data))),
        expected
    );

    for cut in [Cut::FirstHalf, Cut::SecondHalf] {
        for at in 0..total {
            let (flash, failed) = run(SmallFlash::new(3).cut_at(at, cut), &ops);
            let failed = failed.expect("the cut fails an operation");
            let mut before = BTreeMap::new();
            ops[..failed].iter().for_each(|op| apply(&mut before, op));
            let mut after = before.clone();
            apply(&mut after, &ops[failed]);

            let mut backend = mount(MockFlash::with_data(flash.data));
            let found = stored(&mut backend);
            assert!(
                found == before || found == after,
                "cut {cut:?} at operation {at} (op {failed}): {found:?}"
            );

            // Still usable, and consistent after another reboot.
            write(&mut backend, 3, &bytes(9, 50));
            let mut backend = reboot(backend);
            assert_eq!(stored(&mut backend), found);
            assert_eq!(read(&mut backend, 3), Some(bytes(9, 50)));
        }
    }
}
