use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::vec::Vec;

use crc::{CRC_32_ISO_HDLC, Crc};
use serde::{Deserialize, Serialize};

use super::layout::{FORMAT_VERSION, PAGE_SIZE, PageHeader};
use super::mock::{Cut, MockError, MockFlash};
use super::{NorBackend, NorConfig, NorError, WEAR_GAP};
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
    while backend.write(ObjectId(stored), "", &bytes(0, 50)).is_ok() {
        stored += 1;
    }
    // 3 sectors of 3 data pages, one sector kept for garbage collection.
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
            Op::Write(id, value) => backend.write(ObjectId(*id), "", value),
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
fn a_page_cut_in_an_update_or_a_relocation_is_written_again() {
    let ops = updates();
    let first_update = run(SmallFlash::new(3), &ops[..3]).0.ops;
    let total = run(SmallFlash::new(3), &ops).0.ops;
    let mut weak = 0;
    for at in first_update..total {
        // Only the first half of the cut write is done. When that holds the header and payload,
        // the page reads whole, as a page whose write the cut left with weak bits.
        let (flash, _) = run(SmallFlash::new(3).cut_at(at, Cut::FirstHalf), &ops);
        let Some(offset) = flash.cut_write else {
            // An erase.
            continue;
        };
        let cut = offset as usize / PAGE_SIZE;
        let left: [u8; PAGE_SIZE] = flash.data[cut * PAGE_SIZE..][..PAGE_SIZE]
            .try_into()
            .unwrap();
        // A whole page, not a marker.
        if (offset as usize).is_multiple_of(PAGE_SIZE) && PageHeader::decode(&left).is_some() {
            weak += 1;
        }

        // The page can be erased and written again by the mount, as with any other page.
        let backend = mount(SmallFlash::with_data(flash.data));
        let mut live = backend.objects.values().flat_map(|object| &object.pages);
        let left_live = live.any(|&page| usize::from(page) == cut)
            && backend.flash.data[cut * PAGE_SIZE..][..PAGE_SIZE] == left;
        assert!(
            !left_live,
            "cut at operation {at}: page {cut} is live as the cut left it"
        );
    }
    assert!(weak > 0);
}

#[test]
fn a_clean_mount_writes_nothing() {
    let (flash, _) = run(SmallFlash::new(3), &updates());
    assert_eq!(mount(SmallFlash::with_data(flash.data)).flash.ops, 0);
}

#[test]
fn a_page_that_reads_back_wrong_is_written_on_the_next_page() {
    let mut flash = SmallFlash::new(4);
    // The first data page of sector 0, the first sector opened.
    flash.worn = vec![1];
    let mut backend = mount(flash);
    write(&mut backend, 1, &bytes(1, 100));
    assert_eq!(backend.objects[&ObjectId(1)].pages, [2]);
    assert_eq!(backend.usage().reclaimable, 235);

    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 1), Some(bytes(1, 100)));
    // The failed page waits for garbage collection.
    assert_eq!(backend.usage().reclaimable, 235);
}

#[test]
fn a_sector_header_that_reads_back_wrong_leaves_the_sector_unformatted() {
    let mut flash = SmallFlash::new(4);
    // The header page of sector 0.
    flash.worn = vec![0];
    let mut backend = mount(flash);
    write(&mut backend, 1, &bytes(1, 100));
    // Sector 0 counts its erase and stays unformatted: sector 1 is opened instead.
    assert_eq!(backend.objects[&ObjectId(1)].pages, [5]);
    assert_eq!(backend.erase_counts().collect::<Vec<_>>(), [1, 1, 0, 0]);

    let mut backend = reboot(backend);
    assert_eq!(read(&mut backend, 1), Some(bytes(1, 100)));
    // Sector 1 is the only one with a header: the others get its count.
    assert_eq!(backend.erase_counts().collect::<Vec<_>>(), [1, 1, 1, 1]);
}

#[test]
fn a_page_that_reads_back_wrong_three_times_fails_the_write() {
    let mut backend = mount(SmallFlash::new(4));
    write(&mut backend, 1, &bytes(1, 100));
    // Every data page wears out, header pages excepted.
    backend.flash_mut().worn = (0..16).filter(|page| page % 4 != 0).collect();
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
    flash.worn = vec![2, 7, 13];
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
            let failed = failed.expect("the cut fails an operation");
            let mut before = initial.clone();
            ops[..failed].iter().for_each(|op| apply(&mut before, op));
            let mut after = before.clone();
            apply(&mut after, &ops[failed]);

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
    let capacity = (sectors - 1) * 15;
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
