use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::vec::Vec;

use crc::{CRC_32_ISO_HDLC, Crc};
use serde::{Deserialize, Serialize};

use super::layout::{
    COMMIT_OFFSET, COMMITTED, FORMAT_VERSION, PAGE_SIZE, PAYLOAD_SIZE, PageHeader,
};
use super::mock::{Cut, Done, MockError, MockFlash};
use super::{NorBackend, NorConfig, NorError, RESERVED_SECTORS, WEAR_GAP};
use crate::ObjectId;
use crate::backend::{Backend, Usage, Wear, type_tag};

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

/// Writes `value` as the object `id`, named after its id.
fn write<const E: usize, T: Serialize>(backend: &mut NorBackend<MockFlash<E>>, id: u32, value: &T) {
    backend.write(ObjectId(id), &id.to_string(), value).unwrap();
}

fn read<const E: usize>(backend: &mut NorBackend<MockFlash<E>>, id: u32) -> Option<Vec<u8>> {
    match backend.read(ObjectId(id)) {
        Ok(Some(value)) => Some(value),
        Ok(None) => panic!("object {id} is stored with another type"),
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
    assert_eq!(backend.read::<Pattern>(ObjectId(1)).unwrap(), Some(pattern));
    assert_eq!(read(&mut backend, 2), Some(bytes(7, 1000)));
    assert_eq!(backend.read::<()>(ObjectId(3)).unwrap(), Some(()));
    assert_eq!(read(&mut backend, 4), None);
    for id in 1..=3 {
        assert_eq!(backend.name(ObjectId(id)).unwrap(), id.to_string());
    }
    assert!(matches!(backend.name(ObjectId(4)), Err(NorError::NotFound)));
}

#[test]
fn a_name_longer_than_a_page() {
    let mut backend = mount(SmallFlash::new(4));
    let name = "n".repeat(300);
    backend.write(ObjectId(1), &name, &5u32).unwrap();
    let mut backend = reboot(backend);
    assert_eq!(backend.name(ObjectId(1)).unwrap(), name);
    assert_eq!(backend.read::<u32>(ObjectId(1)).unwrap(), Some(5));
}

/// The tag is checked before the value is decoded: without it, the bytes of `5u32` decode as
/// `-3i32`.
#[test]
fn reading_with_another_type_gives_none() {
    let mut backend = mount(SmallFlash::new(4));
    write(&mut backend, 1, &5u32);
    let mut backend = reboot(backend);
    assert_eq!(backend.read::<i32>(ObjectId(1)).unwrap(), None);
    assert_eq!(backend.read::<u32>(ObjectId(1)).unwrap(), Some(5));
    // Part of the stored format: another hash would make every stored object read as another type.
    assert_eq!(type_tag::<u32>(), 0x05f3_74b1);
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
        // Relocated with the pages.
        assert_eq!(backend.name(ObjectId(id)).unwrap(), id.to_string());
    }
    // Wear is spread over every sector.
    let counts: Vec<u32> = backend.erase_counts().collect();
    let (min, max) = (counts.iter().min().unwrap(), counts.iter().max().unwrap());
    assert!(max - min <= 2, "erase counts {counts:?}");
    let wear = Wear {
        sectors: 4,
        most_worn: *max,
        total_erases: counts.iter().map(|&count| u64::from(count)).sum(),
    };
    assert_eq!(backend.wear(), Some(wear));
}

#[test]
fn usage() {
    // 5 sectors of 3 data pages, two kept for garbage collection.
    let page = PAYLOAD_SIZE;
    let mut backend = mount(SmallFlash::new(5));
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
    let mut backend = mount(SmallFlash::new(4));
    let mut stored = 0;
    while backend.write(ObjectId(stored), "", &bytes(0, 50)).is_ok() {
        stored += 1;
    }
    // 4 sectors of 3 data pages, two sectors kept for garbage collection.
    assert_eq!(stored, 6);
    assert!(matches!(
        backend.write(ObjectId(99), "", &0u8),
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

#[test]
fn another_format_version_fails_the_mount() {
    let mut backend = mount(SmallFlash::new(4));
    write(&mut backend, 1, &bytes(1, 300));
    let mut data = backend.flash.data;
    // Firmware with the next format version rewrote the sector headers, keeping them valid.
    let next = FORMAT_VERSION + 1;
    let mut headers = 0;
    for header in data.chunks_mut(1024) {
        if header[..2] == 0x4F53u16.to_le_bytes() {
            header[2..4].copy_from_slice(&next.to_le_bytes());
            let crc = Crc::<u32>::new(&CRC_32_ISO_HDLC).checksum(&header[..8]);
            header[8..12].copy_from_slice(&crc.to_le_bytes());
            headers += 1;
        }
    }
    assert!(headers > 0);

    // A write or erase would hit the cut first: the mount fails without writing anything.
    let flash = SmallFlash::with_data(data).cut_at(0, Cut::FirstHalf);
    let range = 0..flash.data.len() as u32;
    let mounted = NorBackend::mount(NorConfig { flash, range });
    assert!(matches!(mounted, Err(NorError::UnsupportedFormat(v)) if v == next));
}

#[derive(Debug, Clone)]
enum Op {
    Write(u32, Vec<u8>),
    Remove(u32),
}

impl Op {
    fn id(&self) -> u32 {
        match self {
            Op::Write(id, _) | Op::Remove(id) => *id,
        }
    }

    fn apply_to(&self, backend: &mut NorBackend<SmallFlash>) -> Result<(), NorError<MockError>> {
        match self {
            Op::Write(id, value) => backend.write(ObjectId(*id), "", value),
            Op::Remove(id) => backend.remove(ObjectId(*id)),
        }
    }
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
        if let Err(error) = op.apply_to(&mut backend) {
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
    let (flash, failed) = run(SmallFlash::new(4), &ops);
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
            let (flash, failed) = run(SmallFlash::new(4).cut_at(at, cut), &ops);
            // None if the cut hit the marks of the old version in the last op: the new version
            // is whole, so the op succeeds.
            let failed = failed.unwrap_or(ops.len());
            let mut before = BTreeMap::new();
            ops[..failed].iter().for_each(|op| apply(&mut before, op));
            let mut after = before.clone();
            if let Some(op) = ops.get(failed) {
                apply(&mut after, op);
            }

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

type State = BTreeMap<u32, Vec<u8>>;

/// Every object the backend holds, with its value.
fn contents(backend: &mut NorBackend<SmallFlash>) -> State {
    let ids = backend.ids();
    ids.into_iter()
        .map(|id| (id.0, read(backend, id.0).expect("listed")))
        .collect()
}

/// What a reboot now finds: the objects of a mount of a copy of the flash, and that mount.
fn reboot_copy(backend: &NorBackend<SmallFlash>) -> (State, NorBackend<SmallFlash>) {
    let mut rebooted = mount(SmallFlash::with_data(backend.flash.data.clone()));
    (contents(&mut rebooted), rebooted)
}

/// `state`, then each of `ops` done or not.
fn either(state: &State, ops: &[&Op]) -> Vec<State> {
    let mut states = vec![state.clone()];
    for op in ops {
        let done: Vec<State> = (states.iter())
            .map(|state| {
                let mut state = state.clone();
                apply(&mut state, op);
                state
            })
            .collect();
        states.extend(done);
    }
    states
}

/// Runs `ops` on a flash whose operations `fail` fail without a power cut, and checks the backend
/// after each op. As `ObjShelf` does, an op that fails is retried after the next op, or before it
/// if both are on the same object. Returns the errors.
fn run_with_errors(ops: &[Op], fail: &[(usize, Done)]) -> usize {
    let flash = (fail.iter()).fold(SmallFlash::new(5), |flash, &(at, done)| {
        flash.fail_at(at, done)
    });
    let mut backend = mount(flash);
    let mut expected = State::new();
    let mut errors = 0;
    // The op that failed, to retry after the next one.
    let mut failed: Option<&Op> = None;
    for (index, op) in ops.iter().enumerate() {
        let at = format!("{fail:?}, op {index}");
        let pending = failed.take();
        if let Some(pending) = pending.filter(|pending| pending.id() == op.id()) {
            errors += until_done(&mut backend, pending, &[], &mut expected, &at);
        }
        let pending = pending.filter(|pending| pending.id() != op.id());
        match op.apply_to(&mut backend) {
            Ok(()) => apply(&mut expected, op),
            Err(error) => {
                errors += 1;
                let undone: Vec<&Op> = [op].into_iter().chain(pending).collect();
                check_error(&mut backend, error, &expected, &undone, &at);
                if pending.is_none() {
                    failed = Some(op);
                    continue;
                }
                errors += until_done(&mut backend, op, &[pending.unwrap()], &mut expected, &at);
            }
        }
        if let Some(pending) = pending {
            // Meanwhile, the flash holds the object of the failed op as before or after it.
            let (found, _) = reboot_copy(&backend);
            let states = either(&expected, &[pending]);
            assert!(
                states.contains(&found),
                "{at}: reboot, {pending:?} pending: {found:?}"
            );
            errors += until_done(&mut backend, pending, &[], &mut expected, &at);
        }
        if errors > 0 {
            check(&mut backend, &expected, &at);
        }
    }
    if let Some(failed) = failed {
        errors += until_done(&mut backend, failed, &[], &mut expected, "last op");
        check(&mut backend, &expected, "last op");
    }
    errors
}

/// Runs `op` until it succeeds, then applies it to `expected`; `pending` failed and is not retried
/// yet. Returns the errors.
fn until_done(
    backend: &mut NorBackend<SmallFlash>,
    op: &Op,
    pending: &[&Op],
    expected: &mut State,
    at: &str,
) -> usize {
    let mut errors = 0;
    while let Err(error) = op.apply_to(backend) {
        errors += 1;
        assert!(errors < 10, "{at}: {op:?} keeps failing");
        let undone: Vec<&Op> = [op].into_iter().chain(pending.iter().copied()).collect();
        check_error(backend, error, expected, &undone, at);
    }
    apply(expected, op);
    errors
}

/// After an op that failed: the backend still holds `expected`, the state before the ops
/// `undone`, and a reboot finds each of them done or not.
fn check_error(
    backend: &mut NorBackend<SmallFlash>,
    error: NorError<MockError>,
    expected: &State,
    undone: &[&Op],
    at: &str,
) {
    assert!(
        matches!(error, NorError::Flash(MockError::Failed)),
        "{at}: {error:?}"
    );
    assert_eq!(&contents(backend), expected, "{at}: after the error");
    let (found, _) = reboot_copy(backend);
    assert!(
        either(expected, undone).contains(&found),
        "{at}: reboot, {undone:?} undone: {found:?}"
    );
}

/// With no failed op left: the backend holds `expected`, every page left to clean up is in
/// `unmarked`, and a reboot finds `expected` and the same used space.
fn check(backend: &mut NorBackend<SmallFlash>, expected: &State, at: &str) {
    assert_eq!(&contents(backend), expected, "{at}");
    let (found, rebooted) = reboot_copy(backend);
    assert_eq!(&found, expected, "{at}: reboot");
    assert_eq!(rebooted.usage().used, backend.usage().used, "{at}: used");
    if backend.unmarked.is_empty() {
        assert_eq!(rebooted.flash.ops, 0, "{at}: the mount cleaned up");
    }
}

/// Writes, updates and removals of 3 objects on a small flash, with garbage collections that move
/// pages, and removals right after writes.
fn mixed_ops() -> Vec<Op> {
    let mut ops = vec![Op::Write(3, bytes(3, 300))];
    for round in 0..12u8 {
        ops.push(Op::Write(1, bytes(round, 20)));
        ops.push(Op::Write(2, bytes(round, 300)));
        if round % 3 == 2 {
            ops.push(Op::Remove(2));
        }
        if round % 4 == 3 {
            ops.push(Op::Remove(3));
            ops.push(Op::Write(3, bytes(round, 300)));
        }
    }
    ops
}

const DONE: [Done; 4] = [
    Done::Nothing,
    Done::Half(Cut::FirstHalf),
    Done::Half(Cut::SecondHalf),
    Done::All,
];

#[test]
fn a_flash_error_at_every_operation_changes_nothing_and_is_retried() {
    let ops = mixed_ops();
    let total = run(SmallFlash::new(5), &ops).0.ops;
    let mut errors = 0;
    for done in DONE {
        for at in 0..total {
            // None when a mark of a released version fails: it is marked later.
            let run_errors = run_with_errors(&ops, &[(at, done)]);
            assert!(run_errors <= 1);
            errors += run_errors;
        }
    }
    // Most runs see the error.
    assert!(errors > 3 * total);
}

#[test]
fn a_flash_error_around_a_garbage_collection_and_a_removal() {
    // Object 1 is left alone in sector 0 with 2 pages of garbage, so the garbage collection of
    // the 6th op picks that sector and moves it.
    let setup = [
        Op::Write(1, bytes(1, 20)),
        Op::Write(2, bytes(2, 300)),
        Op::Write(2, bytes(3, 300)),
        Op::Write(3, bytes(4, 300)),
        Op::Write(3, bytes(5, 300)),
    ];
    let collect = Op::Write(2, bytes(6, 300));
    let remove = Op::Remove(1);
    // A failed collection, then the removal of the object it moved; a failed removal, then the
    // collection of its sector.
    let (flash, _) = run(SmallFlash::new(5), &setup);
    let setup_ops = flash.ops;
    let mut moved = mount(SmallFlash::with_data(flash.data));
    collect.apply_to(&mut moved).unwrap();
    assert_ne!(moved.objects[&ObjectId(1)].pages, [1], "object 1 is moved");

    for last in [[&collect, &remove], [&remove, &collect]] {
        let ops: Vec<Op> = setup.iter().chain(last).cloned().collect();
        let total = run(SmallFlash::new(5), &ops).0.ops;
        for done in DONE {
            for at in setup_ops..total {
                run_with_errors(&ops, &[(at, done)]);
            }
        }
    }
}

#[test]
fn two_flash_errors_at_every_pair_of_operations() {
    // An update then a removal of the same object, between writes of others.
    let ops = [
        Op::Write(3, bytes(3, 300)),
        Op::Write(2, bytes(1, 300)),
        Op::Write(2, bytes(2, 300)),
        Op::Remove(2),
        Op::Write(1, bytes(1, 20)),
        Op::Remove(3),
    ];
    let total = run(SmallFlash::new(5), &ops).0.ops;
    for first in 0..total {
        // The error shifts the later operations by one at most.
        for second in first + 1..=total {
            for (a, b) in DONE.into_iter().flat_map(|a| DONE.map(|b| (a, b))) {
                run_with_errors(&ops, &[(first, a), (second, b)]);
            }
        }
    }
}

/// Three objects, then only updates of two of them: every later write has an old version to
/// replace, and garbage collection moves the pages of object 3.
fn updates() -> Vec<Op> {
    let round = |round| {
        [
            Op::Write(1, bytes(round, 20)),
            Op::Write(2, bytes(round, 300)),
        ]
    };
    let mut ops = vec![Op::Write(3, bytes(0, 20))];
    ops.extend((0..12).flat_map(round));
    ops
}

#[test]
fn a_page_cut_in_any_write_is_not_live_as_the_cut_left_it() {
    let ops = updates();
    let first_update = run(SmallFlash::new(4), &ops[..3]).0.ops;
    let total = run(SmallFlash::new(4), &ops).0.ops;
    // Pages left whole by a cut in a first write, and in an update or a relocation.
    let (mut weak_first, mut weak_later) = (0, 0);
    for at in 0..total {
        // Only the first half of the cut write is done. When that holds the header and payload,
        // the page reads whole, as a page whose write the cut left with weak bits.
        let (flash, _) = run(SmallFlash::new(4).cut_at(at, Cut::FirstHalf), &ops);
        let Some(offset) = flash.cut_write else {
            // An erase.
            continue;
        };
        let cut = offset as usize / PAGE_SIZE;
        let left: [u8; PAGE_SIZE] = flash.data[cut * PAGE_SIZE..][..PAGE_SIZE]
            .try_into()
            .unwrap();
        // A whole page, not a marker or a commit.
        if (offset as usize).is_multiple_of(PAGE_SIZE) && PageHeader::decode(&left).is_some() {
            if at < first_update {
                weak_first += 1;
            } else {
                weak_later += 1;
            }
        }

        // A first write is dropped, an update or a relocation written again. The page can be
        // erased and written again by the mount, as with any other page.
        let backend = mount(SmallFlash::with_data(flash.data));
        let mut live = backend.objects.values().flat_map(|object| &object.pages);
        let left_live = live.any(|&page| usize::from(page) == cut)
            && backend.flash.data[cut * PAGE_SIZE..][..PAGE_SIZE] == left;
        assert!(
            !left_live,
            "cut at operation {at}: page {cut} is live as the cut left it"
        );
    }
    assert!(weak_first > 0 && weak_later > 0);
}

#[test]
fn a_commit_byte_cut_half_programmed_gets_the_object_written_again() {
    let mut backend = mount(SmallFlash::new(4));
    write(&mut backend, 1, &bytes(1, 300));
    let last = *backend.objects[&ObjectId(1)].pages.last().unwrap();
    // The cut left some bits of the commit byte programmed, maybe weak.
    backend.flash.data[usize::from(last) * PAGE_SIZE + COMMIT_OFFSET] = 0x0F;

    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 1), Some(bytes(1, 300)));
    let pages = backend.objects[&ObjectId(1)].pages.clone();
    assert!(!pages.contains(&last), "{pages:?} still holds page {last}");
    let new_last = usize::from(*pages.last().unwrap());
    assert_eq!(
        backend.flash.data[new_last * PAGE_SIZE + COMMIT_OFFSET],
        COMMITTED
    );

    let backend = reboot(backend);
    assert_eq!(backend.flash.ops, 0);
}

#[test]
fn a_clean_mount_writes_nothing() {
    let (flash, _) = run(SmallFlash::new(4), &updates());
    assert_eq!(mount(SmallFlash::with_data(flash.data)).flash.ops, 0);
}

#[test]
fn a_page_that_reads_back_wrong_is_written_on_the_next_page() {
    let mut flash = SmallFlash::new(4);
    // The first data page of sector 0, the first sector opened.
    flash.worn = vec![PAGE_SIZE];
    let mut backend = mount(flash);
    write(&mut backend, 1, &bytes(1, 100));
    assert_eq!(backend.objects[&ObjectId(1)].pages, [2]);
    assert_eq!(backend.usage().reclaimable, PAYLOAD_SIZE);

    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 1), Some(bytes(1, 100)));
    // The failed page waits for garbage collection.
    assert_eq!(backend.usage().reclaimable, PAYLOAD_SIZE);
}

#[test]
fn a_commit_byte_that_reads_back_wrong_moves_the_last_page() {
    let mut flash = SmallFlash::new(4);
    // The commit byte of the second data page of sector 0, the first sector opened.
    flash.worn = vec![2 * PAGE_SIZE + COMMIT_OFFSET];
    let mut backend = mount(flash);
    write(&mut backend, 1, &bytes(1, 300));
    assert_eq!(backend.objects[&ObjectId(1)].pages, [1, 3]);
    assert_eq!(backend.usage().reclaimable, PAYLOAD_SIZE);

    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 1), Some(bytes(1, 300)));
    assert_eq!(backend.flash.ops, 0);
}

#[test]
fn a_sector_header_or_commit_that_reads_back_wrong_leaves_the_sector_unformatted() {
    // A byte of the header of sector 0, then its commit byte.
    for worn in [0, COMMIT_OFFSET] {
        let mut flash = SmallFlash::new(4);
        flash.worn = vec![worn];
        let mut backend = mount(flash);
        write(&mut backend, 1, &bytes(1, 100));
        // Sector 0 counts its erase and stays unformatted: sector 1 is opened instead.
        assert_eq!(backend.objects[&ObjectId(1)].pages, [5], "worn {worn}");
        assert!(!backend.sectors[0].formatted);
        assert_eq!(backend.erase_counts().collect::<Vec<_>>(), [1, 1, 0, 0]);

        let mut backend = reboot(backend);
        assert_eq!(read(&mut backend, 1), Some(bytes(1, 100)));
        assert!(!backend.sectors[0].formatted, "worn {worn}");
        // Sectors 2 and 3 have no header: they get the average count of the others, 1.
        assert_eq!(backend.erase_counts().collect::<Vec<_>>(), [1, 1, 1, 1]);
    }
}

#[test]
fn a_sector_header_cut_before_its_commit_is_not_used_as_the_cut_left_it() {
    let ops = updates();
    let total = run(SmallFlash::new(4), &ops).0.ops;
    let mut cuts = 0;
    for at in 0..total {
        // Only the first half of the cut write is done: of a 1-byte commit, nothing. The header
        // before it is whole, as a header whose write the cut left with weak bits.
        let (flash, _) = run(SmallFlash::new(4).cut_at(at, Cut::FirstHalf), &ops);
        let Some(offset) = flash.cut_write.map(|offset| offset as usize) else {
            // An erase.
            continue;
        };
        if offset % 1024 != COMMIT_OFFSET {
            // Not the commit of a sector header.
            continue;
        }
        cuts += 1;
        let sector = offset / 1024;
        let left = flash.data[sector * 1024..][..PAGE_SIZE].to_vec();

        // The sector is erased again before use, while the writes open every sector.
        let mut backend = mount(SmallFlash::with_data(flash.data));
        for round in 0..10 {
            let used_as_left = backend.sectors[sector].formatted
                && backend.flash.data[sector * 1024..][..PAGE_SIZE] == left[..];
            assert!(
                !used_as_left,
                "cut at operation {at}, round {round}: sector {sector} is used as the cut left it"
            );
            write(&mut backend, 3, &bytes(round, 20));
        }
        assert_eq!(read(&mut backend, 3), Some(bytes(9, 20)));
    }
    // In the first open of a sector, and in garbage collections.
    assert!(cuts > 3, "{cuts}");
}

#[test]
fn a_sector_keeps_its_data_when_the_commit_of_its_header_decays() {
    let mut backend = mount(SmallFlash::new(4));
    write(&mut backend, 1, &bytes(1, 300));
    assert_eq!(backend.objects[&ObjectId(1)].pages, [1, 2]);
    let mut data = backend.flash.data;
    // All 8 bits of the commit byte of sector 0 flipped back.
    data[COMMIT_OFFSET] = 0xFF;

    // Not erased while other writes open every sector.
    let mut backend = mount(SmallFlash::with_data(data));
    for round in 0..20 {
        write(&mut backend, 2, &bytes(round, 100));
    }
    assert_eq!(read(&mut backend, 1), Some(bytes(1, 300)));
    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 1), Some(bytes(1, 300)));
}

#[test]
fn a_write_never_takes_the_free_sectors_kept_for_garbage_collection() {
    let mut backend = mount(SmallFlash::new(5));
    for id in 1..=6 {
        write(&mut backend, id, &bytes(id as u8, 100));
    }
    write(&mut backend, 1, &bytes(11, 100));
    write(&mut backend, 2, &bytes(12, 100));
    // Sector 0 holds 2 pages of garbage. The write fits in the last page of sector 2, the
    // active one, and only sectors 3 and 4, kept for garbage collection, are free.
    assert_eq!(backend.active, Some(2));
    assert_eq!(backend.active_free_pages(), 1);
    assert_eq!(backend.free_sectors(), 2);
    assert_eq!(backend.garbage(0), 2);

    // That page reads back wrong. The write does not go on in sector 3: it collects sector 0,
    // whose live page moves to sector 3, and starts again there.
    backend.flash_mut().worn = vec![11 * PAGE_SIZE];
    write(&mut backend, 3, &bytes(13, 100));
    assert_eq!(backend.objects[&ObjectId(3)].pages, [14]);
    assert_eq!(backend.free_sectors(), 2);
    assert!(backend.is_free(0) && backend.is_free(4));

    let mut backend = reboot(backend);
    let expected = [11, 12, 13, 4, 5, 6];
    for (id, seed) in (1..=6).zip(expected) {
        assert_eq!(read(&mut backend, id), Some(bytes(seed, 100)));
    }
}

#[test]
fn a_collection_goes_on_in_the_second_free_sector() {
    let mut backend = mount(SmallFlash::new(5));
    for id in 1..=8 {
        write(&mut backend, id, &bytes(id as u8, 100));
    }
    write(&mut backend, 1, &bytes(11, 100));
    // Sector 0 holds 1 page of garbage and 2 live ones. Sector 2, the active one, is full, and
    // only sectors 3 and 4, kept for garbage collection, are free.
    assert_eq!(backend.garbage(0), 1);
    assert_eq!(backend.active_free_pages(), 0);
    assert!(backend.is_free(3) && backend.is_free(4));

    // The next write collects sector 0. Its 2 live pages go to sector 3, where 2 pages read back
    // wrong: the first one takes its last page, the second one goes on in sector 4.
    backend.flash_mut().worn = vec![13 * PAGE_SIZE, 14 * PAGE_SIZE];
    write(&mut backend, 2, &bytes(12, 100));
    assert_eq!(backend.objects[&ObjectId(3)].pages, [17]);
    // Sector 0 is then the only free sector: a second collection reclaims sector 3, whose live
    // page goes to sector 4 too, before the write.
    assert!(backend.is_free(0) && backend.is_free(3));
    assert_eq!(backend.objects[&ObjectId(2)].pages, [19]);

    let mut backend = reboot(backend);
    let expected = [11, 12, 3, 4, 5, 6, 7, 8];
    for (id, seed) in (1..=8).zip(expected) {
        assert_eq!(read(&mut backend, id), Some(bytes(seed, 100)));
    }
}

#[test]
fn a_page_that_reads_back_wrong_three_times_fails_the_write() {
    let mut backend = mount(SmallFlash::new(4));
    write(&mut backend, 1, &bytes(1, 100));
    // Every data page wears out, header pages excepted.
    backend.flash_mut().worn = (0..16)
        .filter(|page| page % 4 != 0)
        .map(|page| page * PAGE_SIZE)
        .collect();
    let written = backend.write(ObjectId(2), "2", &bytes(2, 100));
    assert!(
        matches!(written, Err(NorError::VerifyFailed)),
        "{written:?}"
    );
    assert_eq!(read(&mut backend, 1), Some(bytes(1, 100)));
    assert_eq!(read(&mut backend, 2), None);

    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 1), Some(bytes(1, 100)));
    assert_eq!(read(&mut backend, 2), None);
}

#[test]
fn worn_pages_lose_nothing() {
    let mut flash = SmallFlash::new(4);
    // A worn data page in three of the four sectors.
    flash.worn = vec![2 * PAGE_SIZE, 7 * PAGE_SIZE, 13 * PAGE_SIZE];
    let mut backend = mount(flash);
    // Never updated: garbage collection moves it, and some of its moves hit a worn page.
    write(&mut backend, 9, &bytes(9, 100));
    for round in 0..300u32 {
        write(&mut backend, round % 3, &bytes(round as u8, 100));
    }
    let mut backend = reboot(backend);
    for id in 0..3 {
        let last = 297 + id;
        assert_eq!(read(&mut backend, id), Some(bytes(last as u8, 100)));
    }
    assert_eq!(read(&mut backend, 9), Some(bytes(9, 100)));
}

#[test]
fn the_wear_leaves_out_the_estimated_counts_of_sectors_without_a_header() {
    // 3 data pages per sector: 4 writes of a page format 2 of the 8 sectors, the others stay blank.
    let mut backend = mount(SmallFlash::new(8));
    for round in 0..4u32 {
        write(&mut backend, round % 2, &bytes(round as u8, 100));
    }
    let mut backend = mount(backend.into_flash().rebooted());
    let done = backend.flash.erases.clone();
    assert_eq!(done, [1, 1, 0, 0, 0, 0, 0, 0]);
    // The blank sectors get the average count, for wear leveling, but the wear only counts the
    // erases done.
    assert_eq!(backend.erase_counts().collect::<Vec<_>>(), [1; 8]);
    let wear = Wear {
        sectors: 8,
        most_worn: 1,
        total_erases: 2,
    };
    assert_eq!(backend.wear(), Some(wear));

    // Once erased, a sector counts again, from its estimate.
    for round in 4..100u32 {
        write(&mut backend, round % 2, &bytes(round as u8, 100));
    }
    assert!(backend.flash.erases.iter().all(|&erases| erases > 0));
    let counts: Vec<u32> = backend.erase_counts().collect();
    let total = counts.iter().map(|&count| u64::from(count)).sum();
    assert_eq!(backend.wear().unwrap().total_erases, total);
}

#[test]
fn a_sector_without_a_header_gets_the_average_erase_count() {
    let mut backend = mount(SmallFlash::new(4));
    for round in 0..300u32 {
        write(&mut backend, round % 3, &bytes(round as u8, 100));
    }
    let counts: Vec<u32> = backend.erase_counts().collect();
    assert!(counts.iter().all(|&count| count > 10), "{counts:?}");

    // A sector without live data, whose erase the power cut in its first half: no header.
    let lost = (0..4).find(|&s| backend.sectors[s].live == 0).unwrap();
    let mut data = backend.flash.data;
    data[lost * 1024..][..512].fill(0xFF);
    let others: Vec<u32> = (0..4).filter(|&s| s != lost).map(|s| counts[s]).collect();
    let average = others.iter().sum::<u32>() / others.len() as u32;

    let mut backend = mount(SmallFlash::with_data(data));
    assert_eq!(backend.erase_counts().nth(lost), Some(average));
    for id in 0..3 {
        let last = 297 + id;
        assert_eq!(read(&mut backend, id), Some(bytes(last as u8, 100)));
    }
}

/// A flash where object 9 fills sector 0 and never changes.
fn with_static_object() -> NorBackend<SmallFlash> {
    let mut backend = mount(SmallFlash::new(4));
    backend.write(ObjectId(9), "", &bytes(9, 600)).unwrap();
    assert_eq!(backend.objects[&ObjectId(9)].pages, [1, 2, 3]);
    backend
}

/// Write number `round` of the objects that keep changing, 1 and 2.
fn hot(round: u32) -> Op {
    Op::Write(1 + round % 2, bytes(round as u8, 100))
}

#[test]
fn data_that_never_changes_still_moves_to_level_the_wear() {
    let mut backend = with_static_object();
    for round in 0..3000 {
        let Op::Write(id, value) = hot(round) else {
            unreachable!()
        };
        backend.write(ObjectId(id), "", &value).unwrap();
        let usage = backend.usage();
        assert_eq!(usage.used + usage.free + usage.reclaimable, usage.capacity);
    }
    // Without static wear leveling, sector 0 would stay at 1 erase.
    let counts: Vec<u32> = backend.erase_counts().collect();
    let (min, max) = (counts.iter().min().unwrap(), counts.iter().max().unwrap());
    assert!(*max > 2 * WEAR_GAP, "erase counts {counts:?}");
    assert!(max - min <= WEAR_GAP + 1, "erase counts {counts:?}");

    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 9), Some(bytes(9, 600)));
    assert_eq!(read(&mut backend, 1), Some(bytes(2998u32 as u8, 100)));
    assert_eq!(read(&mut backend, 2), Some(bytes(2999u32 as u8, 100)));
}

/// The flash of `with_static_object` after the writes of `hot` before the first one that moves
/// object 9 out of sector 0, and the round of that write.
fn before_first_static_move() -> (NorBackend<SmallFlash>, u32) {
    let first_move = {
        let mut backend = with_static_object();
        (0..)
            .find(|&round| {
                hot(round).apply_to(&mut backend).unwrap();
                backend.objects[&ObjectId(9)].pages != [1, 2, 3]
            })
            .unwrap()
    };
    let mut backend = with_static_object();
    for round in 0..first_move {
        hot(round).apply_to(&mut backend).unwrap();
    }
    (backend, first_move)
}

/// Write `round` of `hot`, with the data pages `bad` reading back wrong during it. Panics if it
/// loops.
fn hot_with_bad_pages(
    backend: &mut NorBackend<SmallFlash>,
    round: u32,
    bad: &[usize],
) -> Result<(), NorError<MockError>> {
    let flash = backend.flash_mut();
    flash.worn = bad.iter().map(|page| page * PAGE_SIZE).collect();
    let mut programs = 0;
    flash.on_write = Some(Box::new(move || {
        programs += 1;
        assert!(programs < 10_000, "the write loops");
    }));
    let result = hot(round).apply_to(backend);
    let flash = backend.flash_mut();
    flash.worn.clear();
    flash.on_write = None;
    result
}

#[test]
fn a_static_wear_leveling_move_that_runs_out_stops_and_the_write_goes_on() {
    // Object 9 moves to sector 1, the most worn free sector.
    let (mut backend, round) = before_first_static_move();
    hot_with_bad_pages(&mut backend, round, &[]).unwrap();
    assert_eq!(backend.objects[&ObjectId(9)].pages, [5, 6, 7]);

    // Every page of sector 1 reads back wrong: the move stops at its first page, the write goes
    // on. Collecting sector 1 then makes it the most worn free sector again: the move is not
    // tried again in that write.
    let (mut backend, round) = before_first_static_move();
    hot_with_bad_pages(&mut backend, round, &[5, 6, 7]).unwrap();
    assert_eq!(backend.objects[&ObjectId(9)].pages, [1, 2, 3]);

    // A later write moves it.
    for round in round + 1..round + 50 {
        hot(round).apply_to(&mut backend).unwrap();
    }
    assert_ne!(backend.objects[&ObjectId(9)].pages, [1, 2, 3]);
    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 9), Some(bytes(9, 600)));
    assert_eq!(stored(&mut backend), {
        let mut expected = State::new();
        (round + 48..round + 50).for_each(|round| apply(&mut expected, &hot(round)));
        expected
    });
}

#[test]
fn collections_that_free_nothing_end_the_write_with_no_space() {
    // The first page of sectors 1, 2 and 3 reads back wrong: each collection moves 2 live pages
    // to the next free sector, where they take its last 2 pages. It frees nothing, and the next
    // collection does the same, from that sector.
    let (mut backend, round) = before_first_static_move();
    let before = stored(&mut backend);
    let result = hot_with_bad_pages(&mut backend, round, &[5, 9, 13]);
    assert!(matches!(result, Err(NorError::NoSpace)), "{result:?}");

    assert_eq!(stored(&mut backend), before);
    assert_eq!(read(&mut backend, 9), Some(bytes(9, 600)));
    hot(round).apply_to(&mut backend).unwrap();
    let mut backend = reboot(backend);
    let mut expected = before;
    apply(&mut expected, &hot(round));
    assert_eq!(stored(&mut backend), expected);
    assert_eq!(read(&mut backend, 9), Some(bytes(9, 600)));
}

#[test]
fn power_cut_while_static_data_moves() {
    // The first write that moves object 9.
    let mut backend = with_static_object();
    let first_move = (0..)
        .find(|&round| {
            let Op::Write(id, value) = hot(round) else {
                unreachable!()
            };
            backend.write(ObjectId(id), "", &value).unwrap();
            backend.objects[&ObjectId(9)].pages != [1, 2, 3]
        })
        .unwrap();

    // The flash a few writes before, then the writes around the move.
    let start = first_move - 3;
    let mut backend = with_static_object();
    for round in 0..start {
        let Op::Write(id, value) = hot(round) else {
            unreachable!()
        };
        backend.write(ObjectId(id), "", &value).unwrap();
    }
    let data = backend.flash.data;
    let ops: Vec<Op> = (start..first_move + 30).map(hot).collect();
    let initial = stored(&mut mount(SmallFlash::with_data(data.clone())));
    let (flash, failed) = run(SmallFlash::with_data(data.clone()), &ops);
    assert_eq!(failed, None);
    let total = flash.ops;
    let moved = mount(SmallFlash::with_data(flash.data));
    assert_ne!(moved.objects[&ObjectId(9)].pages, [1, 2, 3]);

    for cut in [Cut::FirstHalf, Cut::SecondHalf] {
        for at in 0..total {
            let flash = SmallFlash::with_data(data.clone()).cut_at(at, cut);
            let (flash, failed) = run(flash, &ops);
            // None if the cut hit the marks of the old version in the last op.
            let failed = failed.unwrap_or(ops.len());
            let mut before = initial.clone();
            ops[..failed].iter().for_each(|op| apply(&mut before, op));
            let mut after = before.clone();
            if let Some(op) = ops.get(failed) {
                apply(&mut after, op);
            }

            let mut backend = mount(SmallFlash::with_data(flash.data));
            let found = stored(&mut backend);
            assert!(
                found == before || found == after,
                "cut {cut:?} at operation {at} (op {failed}): {found:?}"
            );
            assert_eq!(
                read(&mut backend, 9),
                Some(bytes(9, 600)),
                "cut {cut:?} at {at}"
            );
        }
    }
}

#[test]
fn a_worn_out_flash_fails_writes_and_keeps_the_data() {
    let (sectors, limit) = (8, 30);
    let mut flash = SmallFlash::new(sectors);
    flash.erase_limit = Some(limit);
    let mut backend = mount(flash);
    // Object 9 never changes, objects 1 to 4 keep changing.
    let mut expected = BTreeMap::new();
    backend.write(ObjectId(9), "", &bytes(9, 600)).unwrap();
    expected.insert(9, bytes(9, 600));
    let mut writes = 0u32;
    let error = loop {
        let (id, value) = (1 + writes % 4, bytes(writes as u8, 100));
        match backend.write(ObjectId(id), "", &value) {
            Ok(()) => {
                expected.insert(id, value);
                writes += 1;
            }
            Err(error) => break error,
        }
    };
    assert!(
        matches!(error, NorError::VerifyFailed | NorError::NoSpace),
        "{error:?}"
    );

    // Every object keeps its last value written, before and after a reboot.
    for (&id, value) in &expected {
        assert_eq!(read(&mut backend, id).as_ref(), Some(value), "object {id}");
    }
    let mut backend = mount(backend.into_flash().rebooted());
    for (&id, value) in &expected {
        assert_eq!(read(&mut backend, id).as_ref(), Some(value), "object {id}");
    }

    // The wear was spread: the store used most of what the flash endures, one page per write at
    // best (3 data pages per sector).
    let best = sectors as u32 * limit * 3;
    assert!(writes * 10 >= best * 7, "{writes} writes of {best} at best");
}

/// Pages written per sector erase with 4 KiB sectors, by how full the flash is, for one-page
/// objects updated at random: the basis of the lifetime estimate in the README. Prints a table:
/// `cargo test --release -- --ignored --nocapture pages_per_erase`.
#[test]
#[ignore]
fn pages_per_erase() {
    let sectors = 64;
    let capacity = (sectors - RESERVED_SECTORS) * 15;
    for percent in [10, 25, 50, 75, 90] {
        let mut backend = mount(MockFlash::<4096>::new(sectors));
        let objects = (capacity * percent / 100) as u32;
        for id in 0..objects {
            write(&mut backend, id, &bytes(id as u8, 200));
        }
        // xorshift32: the same updates on every run.
        let mut state = 0x2545_F491u32;
        let mut update = |backend: &mut NorBackend<MockFlash<4096>>| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            write(backend, state % objects, &bytes(state as u8, 200));
        };
        // Until the garbage collection runs at its steady pace.
        for _ in 0..20_000 {
            update(&mut backend);
        }
        let before = backend.wear().unwrap().total_erases;
        let writes = 50_000;
        for _ in 0..writes {
            update(&mut backend);
        }
        let erases = backend.wear().unwrap().total_erases - before;
        let per_erase = f64::from(writes) / erases as f64;
        std::eprintln!("{percent:>3}% full: {per_erase:.1} pages written per erase");
    }
}
