//! Buffers the CPU rewrites between frames. Why unified memory skips
//! `Queue::write_buffer`: docs/explanation/rendering/pipeline.md
//! "wgpu surface and resources".

use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use wgpu::{
    Adapter, BindGroup, Buffer, BufferAddress, BufferDescriptor, BufferUsages, Device, DeviceType,
    Features, MapMode, PollType, Queue, SubmissionIndex,
};

/// Deeper than the surface's frame latency, so the slot about to be
/// written was last read by a submission that has already retired.
const RING: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UploadMode {
    Staged,
    Mapped,
}

impl UploadMode {
    /// `MAPPABLE_PRIMARY_BUFFERS` on a discrete GPU puts vertex data in
    /// host memory the GPU reads across the bus, hence the device-type gate.
    pub(crate) fn for_adapter(adapter: &Adapter) -> Self {
        let unified = matches!(
            adapter.get_info().device_type,
            DeviceType::IntegratedGpu | DeviceType::Cpu
        );
        if unified
            && adapter
                .features()
                .contains(Features::MAPPABLE_PRIMARY_BUFFERS)
        {
            Self::Mapped
        } else {
            Self::Staged
        }
    }

    pub(crate) const fn required_features(self) -> Features {
        match self {
            Self::Staged => Features::empty(),
            Self::Mapped => Features::MAPPABLE_PRIMARY_BUFFERS,
        }
    }
}

/// One logical buffer: a single `COPY_DST` buffer in staged mode, a ring
/// of mappable buffers in mapped mode.
pub(crate) struct BufferRing {
    label: &'static str,
    usage: BufferUsages,
    store: Store,
    ranges: Vec<Range<usize>>,
}

/// Row `r` of a buffer of row slots plus a tail holds bytes from
/// `r * row_bytes` and last changed at `row_version[r]` of this `epoch`.
#[derive(Clone, Copy)]
pub(crate) struct RowUpdates<'a> {
    pub(crate) epoch: u64,
    pub(crate) version: u64,
    pub(crate) row_version: &'a [u64],
    pub(crate) row_bytes: usize,
}

enum Store {
    Staged(Slot),
    Mapped {
        slots: Box<[Slot; RING]>,
        current: usize,
    },
}

#[derive(Default)]
struct Slot {
    buffer: Option<Buffer>,
    capacity_bytes: BufferAddress,
    generation: u64,
    mapped: Arc<AtomicBool>,
    map_pending: bool,
    last_submission: Option<SubmissionIndex>,
    /// `(epoch, version)` of the [`RowUpdates`] this buffer holds.
    synced: Option<(u64, u64)>,
}

/// Identifies the buffer object `BufferRing::current` returns, so a
/// bind group built over it can be reused until it changes.
pub(crate) type BufferKey = (usize, u64);

impl BufferRing {
    pub(crate) fn new(label: &'static str, usage: BufferUsages, mode: UploadMode) -> Self {
        let store = match mode {
            UploadMode::Staged => Store::Staged(Slot::default()),
            UploadMode::Mapped => Store::Mapped {
                slots: Box::default(),
                current: 0,
            },
        };
        Self {
            label,
            usage,
            store,
            ranges: Vec::new(),
        }
    }

    pub(crate) const fn current(&self) -> Option<&Buffer> {
        self.current_slot().1.buffer.as_ref()
    }

    pub(crate) const fn current_key(&self) -> BufferKey {
        let (index, slot) = self.current_slot();
        (index, slot.generation)
    }

    const fn current_slot(&self) -> (usize, &Slot) {
        match &self.store {
            Store::Staged(slot) => (0, slot),
            Store::Mapped { slots, current } => (*current, &slots[*current]),
        }
    }

    /// `bytes.len()` must be a multiple of 4 (`wgpu::MAP_ALIGNMENT` for the
    /// mapped path, `COPY_BUFFER_ALIGNMENT` for the staged one).
    pub(crate) fn write(&mut self, device: &Device, queue: &Queue, bytes: &[u8]) {
        self.write_rows(device, queue, bytes, None);
    }

    /// Writes only what changed since the target buffer last took
    /// `rows`; each ring slot keeps its own mark, since a slot misses
    /// the frames the other slots took. `row_bytes` must be a multiple
    /// of 8 (`wgpu::MAP_ALIGNMENT`).
    pub(crate) fn write_rows(
        &mut self,
        device: &Device,
        queue: &Queue,
        bytes: &[u8],
        rows: Option<RowUpdates<'_>>,
    ) {
        let needed = bytes.len() as BufferAddress;
        if needed == 0 {
            return;
        }
        let synced = rows.map(|u| (u.epoch, u.version));
        match &mut self.store {
            Store::Staged(slot) => {
                if needed > slot.capacity_bytes {
                    slot.grow(
                        device,
                        self.label,
                        needed,
                        self.usage | BufferUsages::COPY_DST,
                        false,
                    );
                }
                let Some(buf) = slot.buffer.as_ref() else {
                    return;
                };
                changed_ranges(slot.synced, bytes.len(), rows, &mut self.ranges);
                for range in &self.ranges {
                    queue.write_buffer(buf, range.start as BufferAddress, &bytes[range.clone()]);
                }
                slot.synced = synced;
            }
            Store::Mapped { slots, current } => {
                *current = (*current + 1) % RING;
                let slot = &mut slots[*current];
                if needed > slot.capacity_bytes {
                    slot.grow(
                        device,
                        self.label,
                        needed,
                        self.usage | BufferUsages::MAP_WRITE,
                        true,
                    );
                } else if !slot.acquire(device) {
                    return;
                }
                changed_ranges(slot.synced, bytes.len(), rows, &mut self.ranges);
                slot.write(bytes, &self.ranges);
                slot.synced = synced;
            }
        }
    }

    /// Records the submission that read the current slot, so reusing it
    /// waits on that submission rather than on whatever is newest.
    pub(crate) fn submitted(&mut self, index: &SubmissionIndex) {
        if let Store::Mapped { slots, current } = &mut self.store {
            slots[*current].last_submission = Some(index.clone());
        }
    }
}

pub(crate) struct GrowingInstanceBuffer<T> {
    ring: BufferRing,
    _marker: std::marker::PhantomData<T>,
}

impl<T> GrowingInstanceBuffer<T> {
    pub(crate) fn new(label: &'static str, mode: UploadMode) -> Self {
        Self {
            ring: BufferRing::new(label, BufferUsages::VERTEX, mode),
            _marker: std::marker::PhantomData,
        }
    }

    pub(crate) const fn buffer(&self) -> Option<&Buffer> {
        self.ring.current()
    }

    pub(crate) fn upload(
        &mut self,
        device: &Device,
        queue: &Queue,
        bytes: &[u8],
        rows: Option<RowUpdates<'_>>,
    ) {
        self.ring.write_rows(device, queue, bytes, rows);
    }

    pub(crate) fn submitted(&mut self, index: &SubmissionIndex) {
        self.ring.submitted(index);
    }
}

/// A uniform block plus the bind group built over whichever ring buffer
/// is current.
pub(crate) struct UniformRing {
    ring: BufferRing,
    bind_group: Option<(BufferKey, BindGroup)>,
}

impl UniformRing {
    pub(crate) fn new(label: &'static str, mode: UploadMode) -> Self {
        Self {
            ring: BufferRing::new(label, BufferUsages::UNIFORM, mode),
            bind_group: None,
        }
    }

    pub(crate) fn write(&mut self, device: &Device, queue: &Queue, bytes: &[u8]) {
        self.ring.write(device, queue, bytes);
    }

    /// For a bind group that also holds resources outside the ring (the
    /// post stage's offscreen target), after one of those changes.
    pub(crate) fn invalidate(&mut self) {
        self.bind_group = None;
    }

    /// Rebuilds the bind group if the current buffer changed since the
    /// last call; `bind_group` then returns it.
    pub(crate) fn prepare_bind_group(&mut self, make: impl FnOnce(&Buffer) -> BindGroup) {
        let key = self.ring.current_key();
        if self.bind_group.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        self.bind_group = self.ring.current().map(|buffer| (key, make(buffer)));
    }

    pub(crate) fn bind_group(&self) -> Option<&BindGroup> {
        self.bind_group.as_ref().map(|(_, g)| g)
    }

    pub(crate) fn submitted(&mut self, index: &SubmissionIndex) {
        self.ring.submitted(index);
    }
}

/// The rows changed after `synced` plus the whole tail, or all of
/// `len` when the buffer holds another layout (or none).
fn changed_ranges(
    synced: Option<(u64, u64)>,
    len: usize,
    rows: Option<RowUpdates<'_>>,
    out: &mut Vec<Range<usize>>,
) {
    out.clear();
    let (u, since) = match (rows, synced) {
        (Some(u), Some((epoch, since))) if epoch == u.epoch => (u, since),
        _ => return out.push(0..len),
    };
    let mut run: Option<Range<usize>> = None;
    for r in (0..u.row_version.len()).filter(|&r| u.row_version[r] > since) {
        let bytes = r * u.row_bytes..(r + 1) * u.row_bytes;
        match run.as_mut() {
            Some(open) if open.end == bytes.start => open.end = bytes.end,
            _ => out.extend(run.replace(bytes)),
        }
    }
    out.extend(run);
    let tail = (u.row_version.len() * u.row_bytes).min(len);
    if tail < len {
        out.push(tail..len);
    }
}

fn grown_capacity(current: BufferAddress, needed: BufferAddress) -> BufferAddress {
    let mut cap = current.max(64);
    while cap < needed {
        cap *= 2;
    }
    cap
}

impl Slot {
    fn grow(
        &mut self,
        device: &Device,
        label: &'static str,
        needed: BufferAddress,
        usage: BufferUsages,
        mapped_at_creation: bool,
    ) {
        let cap = grown_capacity(self.capacity_bytes, needed);
        self.buffer = Some(device.create_buffer(&BufferDescriptor {
            label: Some(label),
            size: cap,
            usage,
            mapped_at_creation,
        }));
        self.capacity_bytes = cap;
        self.generation += 1;
        self.mapped = Arc::new(AtomicBool::new(mapped_at_creation));
        self.map_pending = false;
        self.last_submission = None;
        self.synced = None;
    }

    /// Maps the slot for writing. `false` leaves the previous contents in
    /// place for this frame; the device-lost path recovers the renderer.
    fn acquire(&mut self, device: &Device) -> bool {
        let Some(buf) = self.buffer.as_ref() else {
            return false;
        };
        if self.mapped.load(Ordering::Acquire) {
            return true;
        }
        if !self.map_pending {
            let flag = Arc::clone(&self.mapped);
            buf.map_async(MapMode::Write, .., move |res| {
                if res.is_ok() {
                    flag.store(true, Ordering::Release);
                }
            });
            self.map_pending = true;
        }
        if let Err(err) = device.poll(PollType::Wait {
            submission_index: self.last_submission.clone(),
            timeout: None,
        }) {
            tracing::warn!(?err, "buffer ring map: device poll failed");
        }
        let ok = self.mapped.load(Ordering::Acquire);
        if ok {
            self.map_pending = false;
        }
        ok
    }

    fn write(&self, bytes: &[u8], ranges: &[Range<usize>]) {
        let Some(buf) = self.buffer.as_ref() else {
            return;
        };
        for range in ranges {
            let span = range.start as BufferAddress..range.end as BufferAddress;
            match buf.get_mapped_range_mut(span) {
                Ok(mut view) => view.copy_from_slice(&bytes[range.clone()]),
                Err(err) => tracing::warn!(?err, "buffer ring map: range refused"),
            }
        }
        buf.unmap();
        self.mapped.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// A ring slot written only its changed ranges equals the source
        /// after every frame, across layout changes and tail lengths.
        #[test]
        fn ring_slots_track_the_source_through_ranged_writes(
            frames in proptest::collection::vec(
                (proptest::collection::vec(0usize..6, 0..4), any::<bool>(), 0usize..3, any::<u8>()),
                1..30,
            ),
        ) {
            const ROWS: usize = 6;
            const ROW_BYTES: usize = 8;
            let mut source = vec![0u8; ROWS * ROW_BYTES];
            let mut row_version = vec![0u64; ROWS];
            let mut epoch = 0;
            let mut slots: [_; RING] = std::array::from_fn(|_| (Vec::new(), None));
            let mut ranges = Vec::new();
            for (version, (dirty, relayout, tail, fill)) in (1u64..).zip(frames) {
                source.truncate(ROWS * ROW_BYTES);
                if relayout {
                    epoch += 1;
                    source.fill(fill);
                    row_version.fill(version);
                }
                for r in dirty {
                    source[r * ROW_BYTES..(r + 1) * ROW_BYTES].fill(fill);
                    row_version[r] = version;
                }
                source.extend(std::iter::repeat_n(fill, tail * ROW_BYTES));
                let rows = RowUpdates { epoch, version, row_version: &row_version, row_bytes: ROW_BYTES };
                let (slot, synced) = &mut slots[version as usize % RING];
                slot.resize(source.len(), 0xAA);
                changed_ranges(*synced, source.len(), Some(rows), &mut ranges);
                for range in &ranges {
                    slot[range.clone()].copy_from_slice(&source[range.clone()]);
                }
                *synced = Some((epoch, version));
                prop_assert_eq!(&*slot, &source);
            }
        }
    }

    #[test]
    fn capacity_doubles_from_a_64_byte_floor_until_it_fits() {
        assert_eq!(grown_capacity(0, 1), 64);
        assert_eq!(grown_capacity(0, 65), 128);
        assert_eq!(grown_capacity(128, 129), 256);
        assert_eq!(grown_capacity(256, 1000), 1024);
    }

    #[test]
    fn staged_mode_asks_for_no_extra_device_feature() {
        assert_eq!(UploadMode::Staged.required_features(), Features::empty());
        assert_eq!(
            UploadMode::Mapped.required_features(),
            Features::MAPPABLE_PRIMARY_BUFFERS
        );
    }
}
