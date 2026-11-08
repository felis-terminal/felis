//! The ring's cell storage: one anonymous OS mapping per buffer, reserved
//! at full capacity and committed as the length grows
//! (docs/explanation/data-model/scrollback.md "Structure"). The crate's one
//! audited `unsafe` boundary: every read relies on each byte of the mapping
//! being part of a written cell or zero, and on zero being a valid cell.
#![allow(unsafe_code)]

use std::alloc::{Layout, handle_alloc_error};
use std::fmt;
use std::io;
use std::ops::{Deref, DerefMut, Range};
use std::ptr::NonNull;

use crate::{Cell, Grapheme};

// SAFETY: const evaluation rejects a constant whose value is invalid for
// its type, so this compiles only while all-zero bytes are a valid `Cell`;
// the assertion below pins that value to `Cell::default()`.
const ZEROED: Cell = unsafe { std::mem::zeroed() };

const _: () = assert!(
    matches!(ZEROED.grapheme, Grapheme::Empty)
        && ZEROED.style.get() == Cell::BLANK.style.get()
        && ZEROED.link.is_none()
        && ZEROED.sizing.is_none(),
    "PagedCells reads fresh and discarded pages as cells: the all-zero Cell must equal Cell::default()",
);

pub(crate) struct PagedCells {
    /// While `cap > 0`, the start of a `map_bytes(cap)`-byte mapping this
    /// value alone owns; dangling, with nothing mapped, at `cap == 0`.
    /// Fresh, discarded and past-`len` pages all read as zero.
    ptr: NonNull<Cell>,
    len: usize,
    /// Requested capacity in cells, which [`Self::capacity`] reports.
    cap: usize,
    /// The mapping's length: `cap` cells rounded up to whole pages.
    map_bytes: usize,
    /// Committed prefix in bytes, a page multiple covering `len` cells.
    #[cfg(windows)]
    committed: usize,
}

// SAFETY: `PagedCells` uniquely owns its mapping, as `Vec<Cell>` owns its
// heap block, and `Cell` is plain `Send + Sync` data; no handle to the
// mapping escapes except through `&self` / `&mut self` borrows.
unsafe impl Send for PagedCells {}
// SAFETY: as for `Send`: shared access is `&[Cell]` reads only.
unsafe impl Sync for PagedCells {}

impl PagedCells {
    pub(crate) const fn new() -> Self {
        Self {
            ptr: NonNull::dangling(),
            len: 0,
            cap: 0,
            map_bytes: 0,
            #[cfg(windows)]
            committed: 0,
        }
    }

    /// `len` default cells, with room for `reserve` before the next
    /// [`Self::grow`] has to move them. Aborts like `Vec` when the system
    /// refuses the reservation.
    pub(crate) fn with_reserve(len: usize, reserve: usize) -> Self {
        Self::try_with_reserve(len, reserve).unwrap_or_else(|_| {
            handle_alloc_error(layout(len.max(reserve)).unwrap_or(Layout::new::<Cell>()))
        })
    }

    pub(crate) fn try_with_reserve(len: usize, reserve: usize) -> io::Result<Self> {
        let cap = len.max(reserve);
        if cap == 0 {
            return Ok(Self::new());
        }
        let map_bytes = map_bytes(cap)?;
        let ptr = sys::reserve(map_bytes)?;
        let mut cells = Self {
            ptr: ptr.cast(),
            len: 0,
            cap,
            map_bytes,
            #[cfg(windows)]
            committed: 0,
        };
        #[cfg(windows)]
        cells.commit(len)?;
        cells.len = len;
        Ok(cells)
    }

    /// Extends the length to `new_len` with `Cell::default()`. Within the
    /// capacity no cell moves and nothing is written: the cells past
    /// `len` are still zero.
    pub(crate) fn grow(&mut self, new_len: usize) {
        assert!(new_len >= self.len, "PagedCells only grows");
        if new_len > self.cap {
            let mut moved = Self::with_reserve(new_len, new_len);
            moved[..self.len].copy_from_slice(self);
            *self = moved;
            return;
        }
        #[cfg(windows)]
        if self.commit(new_len).is_err() {
            handle_alloc_error(layout(new_len).unwrap_or(Layout::new::<Cell>()));
        }
        self.len = new_len;
    }

    pub(crate) const fn capacity(&self) -> usize {
        self.cap
    }

    /// Hands the whole pages inside `cells` back to the OS. They stay mapped
    /// and writable and read back as their old cells or as
    /// `Cell::default()`, so callers treat the range as unspecified. A
    /// failure changes nothing. Call [`Self::reuse`] before the range holds
    /// live cells again (macOS accounts for it there).
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    pub(crate) fn discard(&mut self, cells: Range<usize>) -> io::Result<()> {
        let Some((offset, bytes)) = self.inner_pages(cells) else {
            return Ok(());
        };
        #[cfg(test)]
        if FAIL_DISCARD.get() {
            return Err(io::Error::other("injected discard failure"));
        }
        // SAFETY: `inner_pages` returns a page-aligned, page-multiple
        // span inside `[0, len)`, which lies in this buffer's mapping
        // (committed on Windows). Every primitive `sys::discard` uses
        // leaves those pages mapped and accessible, reading back as the
        // bytes they held or zero; both are valid cells (module invariant).
        unsafe { sys::discard(self.ptr.cast::<u8>().as_ptr().add(offset), bytes) }
    }

    /// Tells the OS a discarded range is live again (macOS
    /// `MADV_FREE_REUSE`, which restores its accounting); a no-op where
    /// the discard needs no pairing.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    pub(crate) fn reuse(&mut self, cells: Range<usize>) -> io::Result<()> {
        let Some((offset, bytes)) = self.inner_pages(cells) else {
            return Ok(());
        };
        // SAFETY: as for `discard`: a whole-page span inside `[0, len)`;
        // `sys::reuse` changes no byte.
        unsafe { sys::reuse(self.ptr.cast::<u8>().as_ptr().add(offset), bytes) }
    }

    /// The byte span of the whole pages inside `cells`, `None` when no
    /// page fits. Panics like slice indexing past `len`.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    fn inner_pages(&self, cells: Range<usize>) -> Option<(usize, usize)> {
        assert!(cells.start <= cells.end, "range {cells:?} is reversed");
        assert!(
            cells.end <= self.len,
            "range {cells:?} out of bounds for {} cells",
            self.len
        );
        let page = sys::page_size();
        let start = (cells.start * size_of::<Cell>()).next_multiple_of(page);
        let end = cells.end * size_of::<Cell>() / page * page;
        (start < end).then_some((start, end - start))
    }

    #[cfg(windows)]
    fn commit(&mut self, len: usize) -> io::Result<()> {
        let want = (len * size_of::<Cell>())
            .next_multiple_of(sys::page_size())
            .min(self.map_bytes);
        if want > self.committed {
            // SAFETY: `[committed, want)` lies inside the reservation:
            // `len <= cap` and the reservation is `cap` cells rounded up
            // to whole pages.
            unsafe {
                sys::commit(
                    self.ptr.cast::<u8>().as_ptr().add(self.committed),
                    want - self.committed,
                )?;
            }
            self.committed = want;
        }
        Ok(())
    }
}

impl Drop for PagedCells {
    fn drop(&mut self) {
        if self.cap == 0 {
            return;
        }
        // SAFETY: a nonzero `cap` means `ptr` starts the `map_bytes`-long
        // mapping this value owns (see `ptr`); it is released once, here,
        // and nothing borrows it past `drop`.
        unsafe { sys::release(self.ptr.cast(), self.map_bytes) };
    }
}

impl Default for PagedCells {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for PagedCells {
    type Target = [Cell];

    fn deref(&self) -> &[Cell] {
        // SAFETY: `[0, len)` is mapped, accessible and holds valid cells
        // (module invariant); with `cap == 0` the dangling pointer is
        // aligned and `len` is zero. The borrow ties the slice to `self`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for PagedCells {
    fn deref_mut(&mut self) -> &mut [Cell] {
        // SAFETY: as for `deref`; `&mut self` makes the borrow unique.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl<'a> IntoIterator for &'a PagedCells {
    type Item = &'a Cell;
    type IntoIter = std::slice::Iter<'a, Cell>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut PagedCells {
    type Item = &'a mut Cell;
    type IntoIter = std::slice::IterMut<'a, Cell>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

/// Keeps the source's capacity, so a clone grows without moving either.
impl Clone for PagedCells {
    fn clone(&self) -> Self {
        let mut cells = Self::with_reserve(self.len, self.cap);
        cells.copy_from_slice(self);
        cells
    }
}

impl PartialEq for PagedCells {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for PagedCells {}

impl fmt::Debug for PagedCells {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

fn layout(cells: usize) -> Option<Layout> {
    Layout::array::<Cell>(cells).ok()
}

fn map_bytes(cells: usize) -> io::Result<usize> {
    cells
        .checked_mul(size_of::<Cell>())
        .and_then(|bytes| bytes.checked_next_multiple_of(sys::page_size()))
        .filter(|&bytes| isize::try_from(bytes).is_ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "capacity overflow"))
}

#[cfg(test)]
thread_local! {
    static FAIL_DISCARD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes every [`PagedCells::discard`] on this thread fail until reset.
#[cfg(test)]
pub(crate) fn fail_discards(fail: bool) {
    FAIL_DISCARD.set(fail);
}

#[cfg(unix)]
mod sys {
    use std::io;
    use std::ptr::NonNull;

    pub(super) fn page_size() -> usize {
        // SAFETY: `sysconf` reads a process constant and has no
        // preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(page).unwrap_or(4096)
    }

    pub(super) fn reserve(bytes: usize) -> io::Result<NonNull<u8>> {
        // SAFETY: a fresh private anonymous mapping aliases no existing
        // memory; the kernel picks the address and zero-fills on first
        // touch. Failure is reported as `MAP_FAILED`, checked below.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        NonNull::new(ptr.cast()).ok_or_else(|| io::Error::other("mmap returned null"))
    }

    /// # Safety
    /// `ptr` and `bytes` must be exactly a mapping `reserve` returned,
    /// released once, with no borrow of it outstanding.
    pub(super) unsafe fn release(ptr: NonNull<u8>, bytes: usize) {
        // SAFETY: forwarded from the caller's contract.
        let rc = unsafe { libc::munmap(ptr.as_ptr().cast(), bytes) };
        debug_assert_eq!(rc, 0, "munmap of an owned mapping failed");
    }

    #[cfg(target_os = "macos")]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    const DISCARD: libc::c_int = libc::MADV_FREE_REUSABLE;
    #[cfg(not(target_os = "macos"))]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    const DISCARD: libc::c_int = libc::MADV_DONTNEED;

    /// # Safety
    /// `ptr..ptr + bytes` must be whole pages of a live private anonymous
    /// mapping. Linux refaults them as zero pages; macOS lets the kernel
    /// take them, after which they read as zero or as what they held.
    /// Neither advice unmaps or protects anything.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    pub(super) unsafe fn discard(ptr: *mut u8, bytes: usize) -> io::Result<()> {
        // SAFETY: forwarded from the caller's contract.
        let rc = unsafe { libc::madvise(ptr.cast(), bytes, DISCARD) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// # Safety
    /// As for [`discard`]; `MADV_FREE_REUSE` changes no byte.
    #[cfg(target_os = "macos")]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    pub(super) unsafe fn reuse(ptr: *mut u8, bytes: usize) -> io::Result<()> {
        // SAFETY: forwarded from the caller's contract.
        let rc = unsafe { libc::madvise(ptr.cast(), bytes, libc::MADV_FREE_REUSE) };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// # Safety
    /// None: Linux's discard needs no pairing.
    #[cfg(not(target_os = "macos"))]
    #[allow(clippy::unnecessary_wraps)]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    pub(super) const unsafe fn reuse(_ptr: *mut u8, _bytes: usize) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    use std::io;
    use std::ptr::NonNull;

    use windows_sys::Win32::System::Memory::{
        DiscardVirtualMemory, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEM_RESET, PAGE_NOACCESS,
        PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

    pub(super) fn page_size() -> usize {
        let mut info: SYSTEM_INFO = SYSTEM_INFO::default();
        // SAFETY: `GetSystemInfo` fills the struct it is pointed at and
        // cannot fail.
        unsafe { GetSystemInfo(&raw mut info) };
        info.dwPageSize as usize
    }

    /// Address space only: nothing counts against the commit charge until
    /// [`commit`].
    pub(super) fn reserve(bytes: usize) -> io::Result<NonNull<u8>> {
        // SAFETY: a fresh reservation aliases no existing memory; the
        // system picks the address. Failure is a null return.
        let ptr = unsafe { VirtualAlloc(std::ptr::null(), bytes, MEM_RESERVE, PAGE_NOACCESS) };
        NonNull::new(ptr.cast()).ok_or_else(io::Error::last_os_error)
    }

    /// # Safety
    /// `ptr..ptr + bytes` must be whole pages inside a live reservation
    /// from [`reserve`]. Newly committed pages read as zero.
    pub(super) unsafe fn commit(ptr: *mut u8, bytes: usize) -> io::Result<()> {
        // SAFETY: forwarded from the caller's contract.
        let got = unsafe { VirtualAlloc(ptr.cast(), bytes, MEM_COMMIT, PAGE_READWRITE) };
        if got.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// # Safety
    /// `ptr` must be the base of a reservation from [`reserve`], released
    /// once, with no borrow of it outstanding.
    pub(super) unsafe fn release(ptr: NonNull<u8>, _bytes: usize) {
        // SAFETY: forwarded from the caller's contract; `MEM_RELEASE`
        // takes size 0 and frees the whole reservation, committed or not.
        let ok = unsafe { VirtualFree(ptr.as_ptr().cast::<c_void>(), 0, MEM_RELEASE) };
        debug_assert_ne!(ok, 0, "VirtualFree of an owned reservation failed");
    }

    /// # Safety
    /// `ptr..ptr + bytes` must be whole committed pages of a reservation
    /// from [`reserve`]. Both `DiscardVirtualMemory` and `MEM_RESET` keep
    /// the pages committed and accessible; their contents become zero or
    /// stay as they were.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    pub(super) unsafe fn discard(ptr: *mut u8, bytes: usize) -> io::Result<()> {
        // SAFETY: forwarded from the caller's contract.
        if unsafe { DiscardVirtualMemory(ptr.cast(), bytes) } == 0 {
            return Ok(());
        }
        // SAFETY: as above; `MEM_RESET` ignores the protection argument.
        let got = unsafe { VirtualAlloc(ptr.cast(), bytes, MEM_RESET, PAGE_READWRITE) };
        if got.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// # Safety
    /// None: Windows' discard needs no pairing.
    #[allow(clippy::unnecessary_wraps)]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    pub(super) const unsafe fn reuse(_ptr: *mut u8, _bytes: usize) -> io::Result<()> {
        Ok(())
    }
}

/// Targets without an OS mapping API (wasm32) take a zeroed heap block:
/// still no write to fill it, but nothing to discard.
#[cfg(not(any(unix, windows)))]
mod sys {
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::io;
    use std::ptr::NonNull;

    pub(super) const fn page_size() -> usize {
        4096
    }

    fn layout(bytes: usize) -> io::Result<Layout> {
        Layout::from_size_align(bytes, page_size())
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    pub(super) fn reserve(bytes: usize) -> io::Result<NonNull<u8>> {
        let layout = layout(bytes)?;
        // SAFETY: `bytes` is nonzero (`cap > 0`) and the layout is valid.
        let ptr = unsafe { alloc_zeroed(layout) };
        NonNull::new(ptr).ok_or_else(|| io::Error::from(io::ErrorKind::OutOfMemory))
    }

    /// # Safety
    /// `ptr` and `bytes` must be exactly a block `reserve` returned,
    /// released once.
    pub(super) unsafe fn release(ptr: NonNull<u8>, bytes: usize) {
        // `reserve` built this layout from the same size, so it is valid.
        if let Ok(layout) = layout(bytes) {
            // SAFETY: forwarded from the caller's contract.
            unsafe { dealloc(ptr.as_ptr(), layout) };
        }
    }

    /// # Safety
    /// None: nothing is handed back.
    #[allow(clippy::unnecessary_wraps)]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    pub(super) const unsafe fn discard(_ptr: *mut u8, _bytes: usize) -> io::Result<()> {
        Ok(())
    }

    /// # Safety
    /// None.
    #[allow(clippy::unnecessary_wraps)]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "no production caller discards yet")
    )]
    pub(super) const unsafe fn reuse(_ptr: *mut u8, _bytes: usize) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
