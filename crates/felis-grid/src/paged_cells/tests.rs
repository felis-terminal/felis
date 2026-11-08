use super::*;

fn marked(i: usize) -> Cell {
    Cell {
        grapheme: Grapheme::Ascii(b'a' + (i % 26) as u8),
        ..Cell::default()
    }
}

fn filled(len: usize, reserve: usize) -> PagedCells {
    let mut cells = PagedCells::with_reserve(len, reserve);
    for (i, cell) in cells.iter_mut().enumerate() {
        *cell = marked(i);
    }
    cells
}

/// Cells per page, so a test can name page-aligned ranges.
fn page_cells() -> usize {
    sys::page_size() / size_of::<Cell>()
}

#[test]
fn zeroed_bytes_are_the_default_cell() {
    assert_eq!(ZEROED, Cell::default());
    assert_eq!(Cell::default(), Cell::BLANK);
}

#[test]
fn a_fresh_buffer_reads_default_cells_and_reports_the_requested_capacity() {
    let cells = PagedCells::with_reserve(1000, 5000);
    assert_eq!(cells.len(), 1000);
    assert_eq!(cells.capacity(), 5000);
    assert!(cells.iter().all(|c| *c == Cell::default()));
}

#[test]
fn an_empty_buffer_maps_nothing() {
    let cells = PagedCells::new();
    assert!(cells.is_empty());
    assert_eq!(cells.capacity(), 0);
    let reserved_nothing = PagedCells::with_reserve(0, 0);
    assert!(reserved_nothing.is_empty());
    assert_eq!(*cells, *reserved_nothing);
}

#[test]
fn growing_within_capacity_keeps_cells_in_place_and_adds_defaults() {
    let mut cells = filled(100, 10 * page_cells());
    let origin = cells.as_ptr();
    cells.grow(5 * page_cells());
    assert_eq!(cells.as_ptr(), origin);
    assert!((0..100).all(|i| cells[i] == marked(i)));
    assert!(cells[100..].iter().all(|c| *c == Cell::default()));
}

#[test]
fn growing_past_capacity_moves_the_cells_intact() {
    let mut cells = filled(100, 100);
    cells.grow(3 * page_cells());
    assert!(cells.capacity() >= 3 * page_cells());
    assert!((0..100).all(|i| cells[i] == marked(i)));
    assert!(cells[100..].iter().all(|c| *c == Cell::default()));
}

#[test]
fn a_clone_keeps_the_contents_and_the_capacity() {
    let cells = filled(300, 4 * page_cells());
    let copy = cells.clone();
    assert_eq!(*copy, *cells);
    assert_eq!(copy.capacity(), cells.capacity());
    assert_ne!(copy.as_ptr(), cells.as_ptr());
}

/// Discard rounds inward, so the partial pages at both ends keep their
/// cells; inside, every cell is its old value or the default and stays
/// writable.
#[test]
fn discard_keeps_partial_pages_and_leaves_every_cell_valid_and_writable() {
    let page = page_cells();
    let len = 8 * page;
    let mut cells = filled(len, len);
    let range = page / 2..6 * page + page / 2;
    cells.discard(range.clone()).expect("discard");
    for i in 0..len {
        let inside = (page..6 * page).contains(&i);
        if inside {
            assert!(
                cells[i] == marked(i) || cells[i] == Cell::default(),
                "cell {i}"
            );
        } else {
            assert_eq!(cells[i], marked(i), "cell {i} outside the whole pages");
        }
    }
    #[cfg(target_os = "linux")]
    assert!(cells[page..6 * page].iter().all(|c| *c == Cell::default()));
    cells.reuse(range.clone()).expect("reuse");
    for i in range.clone() {
        cells[i] = marked(i + 1);
    }
    assert!(range.into_iter().all(|i| cells[i] == marked(i + 1)));
    cells.grow(len + page);
    assert!(cells[len..].iter().all(|c| *c == Cell::default()));
}

#[test]
fn a_discard_smaller_than_a_page_changes_nothing() {
    let page = page_cells();
    let mut cells = filled(4 * page, 4 * page);
    cells.discard(page + 1..2 * page + 1).expect("discard");
    assert!((0..4 * page).all(|i| cells[i] == marked(i)));
}

#[test]
fn a_failed_discard_changes_nothing_and_leaves_the_buffer_usable() {
    let page = page_cells();
    let mut cells = filled(4 * page, 8 * page);
    fail_discards(true);
    let result = cells.discard(0..4 * page);
    fail_discards(false);
    assert!(result.is_err());
    assert!((0..4 * page).all(|i| cells[i] == marked(i)));
    cells[0] = marked(7);
    cells.grow(8 * page);
    assert_eq!(cells[0], marked(7));
    assert!(cells[4 * page..].iter().all(|c| *c == Cell::default()));
}

#[test]
#[should_panic(expected = "out of bounds")]
fn a_discard_past_the_length_panics() {
    let mut cells = PagedCells::with_reserve(10, 100);
    assert!(cells.discard(0..11).is_err());
}

/// Linux reports per-page residency exactly (`mincore`), so the pages a
/// discard returns are observable.
#[cfg(target_os = "linux")]
#[test]
fn a_discard_returns_its_pages_to_the_os() {
    let page = page_cells();
    let mut cells = PagedCells::with_reserve(16 * page, 64 * page);
    assert_eq!(resident_pages(&cells), 0, "a fresh mapping is not resident");
    for (i, cell) in cells.iter_mut().enumerate() {
        *cell = marked(i);
    }
    assert_eq!(resident_pages(&cells), 16);
    cells.discard(0..12 * page).expect("discard");
    assert_eq!(resident_pages(&cells), 4);
}

#[cfg(windows)]
mod windows {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// `(state, region bytes)` of the run of same-state pages at `addr`.
    fn region(addr: *const u8) -> (u32, usize) {
        let mut info = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `VirtualQuery` only describes the address it is given and
        // fills the struct it is pointed at.
        let n = unsafe { VirtualQuery(addr.cast(), &raw mut info, size_of_val(&info)) };
        assert_ne!(n, 0, "VirtualQuery: {}", io::Error::last_os_error());
        (info.State, info.RegionSize)
    }

    fn working_set_pages(cells: &PagedCells) -> usize {
        let page = sys::page_size();
        let pages = cells.len() * size_of::<Cell>() / page;
        let base = cells.as_ptr().cast::<u8>();
        let mut info: Vec<PSAPI_WORKING_SET_EX_INFORMATION> = (0..pages)
            .map(|i| PSAPI_WORKING_SET_EX_INFORMATION {
                // SAFETY: `i` pages from the base stays inside the mapping.
                VirtualAddress: unsafe { base.add(i * page) }.cast_mut().cast(),
                ..Default::default()
            })
            .collect();
        // SAFETY: the buffer holds `pages` entries of the size passed.
        let ok = unsafe {
            K32QueryWorkingSetEx(
                GetCurrentProcess(),
                info.as_mut_ptr().cast(),
                u32::try_from(size_of_val(info.as_slice())).expect("small query"),
            )
        };
        assert_ne!(ok, 0, "QueryWorkingSetEx: {}", io::Error::last_os_error());
        // SAFETY: `Flags` is the whole bitfield; bit 0 is `Valid`.
        info.iter()
            .filter(|i| unsafe { i.VirtualAttributes.Flags } & 1 != 0)
            .count()
    }

    #[test]
    fn commit_follows_the_length_not_the_reservation() {
        let page = page_cells();
        let mut cells = PagedCells::with_reserve(3 * page + 1, 1024 * page);
        let base = cells.as_ptr().cast::<u8>();
        let page_bytes = sys::page_size();
        assert_eq!(region(base), (MEM_COMMIT, 4 * page_bytes));
        // SAFETY: four pages into a 1024-page reservation.
        let past = unsafe { base.add(4 * page_bytes) };
        assert_eq!(region(past).0, MEM_RESERVE);
        cells.grow(10 * page);
        assert_eq!(region(base), (MEM_COMMIT, 10 * page_bytes));
    }

    /// Windows lowers the working set on discard but keeps the commit
    /// charge, so the two are reported apart.
    #[test]
    fn a_discard_keeps_the_commit_and_reports_the_working_set() {
        let page = page_cells();
        let mut cells = filled(16 * page, 16 * page);
        let base = cells.as_ptr().cast::<u8>();
        let before = working_set_pages(&cells);
        cells.discard(0..16 * page).expect("discard");
        let after = working_set_pages(&cells);
        eprintln!("working set: {before} -> {after} of 16 pages; commit unchanged");
        assert_eq!(region(base), (MEM_COMMIT, 16 * sys::page_size()));
        assert!(after <= before);
        assert!((0..16 * page).all(|i| cells[i] == marked(i) || cells[i] == Cell::default()));
    }
}
