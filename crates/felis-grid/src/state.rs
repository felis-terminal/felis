//! Grid and image state an in-place daemon upgrade carries across `execve`
//! to its immediate successor, never on the wire
//! (`docs/explanation/architecture/overview.md` "In-place upgrade"). A struct
//! with no natural `Default` takes `#[serde(default)]` on any field added later.
//! The style table rides ahead of the row-codec rows so re-interning keeps ids.

use std::num::{NonZeroU16, NonZeroU32};

use bytes::Bytes;
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use felis_vt::state::StateError;

use crate::{
    ApcBody, Attributes, Cell, ClusterText, Cursor, CursorStyle, Damage, Grapheme, Grid,
    HyperlinkEntry, KITTY_KBD_STACK_LIMIT, LinkText, Margins, NOTIFY_OUTBOX_CAP, PtyEffect,
    ScreenBuffer, ShellPrompt, Sizing, SizingHandle, StyleTable, SyncOutput, TITLE_STACK_LIMIT,
    images::{ImageEntry, ImageId},
    pty_effects::{APC_OUTBOX_BYTES, APC_OUTBOX_CAP},
    screen::SavedScreen,
    wire::{MAX_CELLS_PER_ROW, ROW_CODEC_VERSION, RowEncode, decode_row, encode_row},
};

fn check(ok: bool, what: &str) -> Result<(), StateError> {
    if ok {
        Ok(())
    } else {
        Err(StateError(what.to_owned()))
    }
}

impl Grid {
    /// What a field absent from a dump takes: a fresh terminal's value.
    #[must_use]
    pub fn dump_default() -> Self {
        Self::with_scrollback(1, 1, 0)
    }

    /// Rejects a restored grid no sequence of output could produce, before
    /// anything indexes through it.
    ///
    /// # Errors
    /// The first inconsistency found.
    pub fn check_restored(&self) -> Result<(), StateError> {
        let screen = &self.screen;
        let (rows, cols) = (screen.rows, screen.cols);
        check(
            screen.cursor.row < rows && screen.cursor.col < cols,
            "cursor outside the screen",
        )?;
        let m = self.margins;
        check(
            m.top <= m.bottom && m.bottom < rows && m.left <= m.right && m.right < cols,
            "scroll margins",
        )?;
        check(self.tab_stops.len() == usize::from(cols), "tab stops")?;
        check(
            (self.pen_style.get() as usize) < screen.style_table.len()
                && *screen.style_table.resolve(self.pen_style) == self.pen,
            "pen id",
        )?;
        check(
            screen.band_len == 0
                || (screen.band_top + screen.band_len <= usize::from(rows)
                    && screen.band_rot < screen.band_len),
            "scroll band",
        )?;
        check(
            screen.damage.check_restored(usize::from(rows)),
            "damage rows",
        )?;
        check(
            self.current_link
                .is_none_or(|link| usize::from(link.get()) <= screen.link_table.len()),
            "pen hyperlink",
        )?;
        check(
            (1..=5).contains(&self.conformance_level),
            "conformance level",
        )?;
        check(
            self.kitty_kbd.stack.len() <= KITTY_KBD_STACK_LIMIT,
            "keyboard flag stack",
        )?;
        check(self.title_stack.len() <= TITLE_STACK_LIMIT, "title stack")?;
        check(
            self.pointer_shape_stack.len() <= Self::POINTER_SHAPE_STACK_CAP,
            "pointer shape stack",
        )?;
        check(self.pending_bidi.check_restored(), "pending bidi controls")?;
        check(
            self.dcs
                .as_ref()
                .is_none_or(|dcs| dcs.body.len() <= crate::DCS_BUFFER_LIMIT),
            "DCS body",
        )?;
        let apcs: Vec<&ApcBody> = self
            .pty_effects
            .effects()
            .iter()
            .filter_map(|effect| match effect {
                PtyEffect::Apc(body) => Some(body),
                _ => None,
            })
            .collect();
        check(
            apcs.len() <= APC_OUTBOX_CAP
                && apcs.iter().map(|apc| apc.body.len()).sum::<usize>() <= APC_OUTBOX_BYTES
                && apcs
                    .iter()
                    .all(|apc| apc.body.len() <= felis_vt::APC_BUFFER_LIMIT),
            "queued APC bodies",
        )?;
        check(
            self.notifications.len() <= NOTIFY_OUTBOX_CAP,
            "queued notifications",
        )?;
        check(
            self.osc99_pending.len() <= crate::NOTIFY_REASSEMBLY_IDS
                && self.osc99_pending.values().all(|partial| {
                    partial.title.len() + partial.body.len() <= crate::NOTIFY_REASSEMBLY_BYTES
                }),
            "notification reassembly",
        )?;
        self.utf8.check_restored()?;
        self.graphics_tracker.check_restored()
    }
}

impl Default for ShellPrompt {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for Margins {
    fn default() -> Self {
        Self::new(1, 1)
    }
}

/// The parse thread reads no clock, so only an anchored deadline holds
/// an `Instant`; it crosses as the time left on it.
pub(crate) mod sync_output {
    use std::time::{Duration, Instant};

    use super::{Deserialize, Deserializer, Serialize, Serializer, SyncOutput};

    #[derive(Serialize, Deserialize)]
    enum Dump {
        Off,
        On { deadline_in_ms: Option<u64> },
    }

    pub(crate) fn serialize<S: Serializer>(
        value: &SyncOutput,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            SyncOutput::Off => Dump::Off,
            SyncOutput::On { deadline } => Dump::On {
                deadline_in_ms: deadline.map(|at| {
                    u64::try_from(at.saturating_duration_since(Instant::now()).as_millis())
                        .unwrap_or(u64::MAX)
                }),
            },
        }
        .serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<SyncOutput, D::Error> {
        Ok(match Dump::deserialize(deserializer)? {
            Dump::Off => SyncOutput::Off,
            Dump::On { deadline_in_ms } => SyncOutput::On {
                deadline: deadline_in_ms
                    .and_then(|ms| Instant::now().checked_add(Duration::from_millis(ms))),
            },
        })
    }
}

/// A frame's pixels, base64 like every other byte payload.
pub(crate) mod shared_b64 {
    use super::{Bytes, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        bytes: &Bytes,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        felis_vt::state::b64::serialize(bytes, serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Bytes, D::Error> {
        felis_vt::state::b64::deserialize(deserializer).map(Bytes::from)
    }
}

/// The store's entries as a list: a JSON object would need string keys,
/// and the order is the eviction order.
pub(crate) mod image_entries {
    use super::{Deserialize, Deserializer, ImageEntry, ImageId, IndexMap, Serializer};
    use serde::de::Error as _;

    pub(crate) fn serialize<S: Serializer>(
        entries: &IndexMap<ImageId, ImageEntry>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(entries.iter())
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<IndexMap<ImageId, ImageEntry>, D::Error> {
        let list = Vec::<(ImageId, ImageEntry)>::deserialize(deserializer)?;
        let len = list.len();
        let entries: IndexMap<_, _> = list.into_iter().collect();
        if entries.len() != len {
            return Err(D::Error::custom("an image id appears twice"));
        }
        Ok(entries)
    }
}

/// One cell ring: the live screen's, or a saved primary or alternate.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct RingDump {
    rows: u16,
    cols: u16,
    base: usize,
    phys_cap: usize,
    history_len: usize,
    cap: usize,
    /// One row-codec payload per physical row, clipped at the row's
    /// occupancy: past it the ring holds bytes no reader looks at.
    cells: Vec<String>,
    /// `(physical row, column, handle)` for every cell with a sizing.
    sizings: Vec<(usize, u16, u16)>,
}

#[derive(Serialize, Deserialize)]
struct LinkDump {
    id: Option<String>,
    uri: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SavedDump {
    ring: RingDump,
    cursor: Cursor,
    pen: Attributes,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct ScreenDump {
    /// The row codec every payload in `ring` and the saved rings is in.
    row_codec: u8,
    ring: RingDump,
    band_top: usize,
    band_len: usize,
    band_rot: usize,
    cursor: Cursor,
    cursor_style: CursorStyle,
    cursor_blink: bool,
    styles: Vec<Attributes>,
    damage: Damage,
    saved_primary: Option<SavedDump>,
    saved_alternate: Option<SavedDump>,
    links: Vec<LinkDump>,
    clusters: Vec<String>,
    sizing_table: Vec<Sizing>,
    geometry_gen: u64,
    scroll_seq: u64,
}

struct RingRef<'a> {
    rows: u16,
    cols: u16,
    base: usize,
    phys_cap: usize,
    history_len: usize,
    cap: usize,
    cells: &'a [Cell],
    soft_wrap: &'a [bool],
    occupancy: &'a [u16],
}

impl<'a> RingRef<'a> {
    fn of_saved(saved: &'a SavedScreen) -> Self {
        Self {
            rows: saved.rows,
            cols: saved.cols,
            base: saved.base,
            phys_cap: saved.phys_cap,
            history_len: saved.history_len,
            cap: saved.cap,
            cells: &saved.cells,
            soft_wrap: &saved.soft_wrap,
            occupancy: &saved.occupancy,
        }
    }

    fn dump(&self, styles: &StyleTable) -> Result<RingDump, String> {
        let cols = usize::from(self.cols);
        let mut cells = Vec::with_capacity(self.phys_cap);
        let mut sizings = Vec::new();
        for row in 0..self.phys_cap {
            let start = row * cols;
            let occupied = usize::from(self.occupancy.get(row).copied().unwrap_or(0));
            let slice = self
                .cells
                .get(start..start + occupied)
                .ok_or("a ring row past its cells")?;
            let payload = encode_row(
                RowEncode {
                    cells: slice,
                    pad_to: occupied,
                    sized_cells: &[],
                    soft_wrap_continued: self.soft_wrap.get(row).copied().unwrap_or(false),
                },
                styles,
            )
            .map_err(|err| err.to_string())?;
            cells.push(felis_vt::state::b64::encode(&payload));
            for (col, cell) in slice.iter().enumerate() {
                if let Some(handle) = cell.sizing {
                    sizings.push((row, col as u16, handle.get()));
                }
            }
        }
        Ok(RingDump {
            rows: self.rows,
            cols: self.cols,
            base: self.base,
            phys_cap: self.phys_cap,
            history_len: self.history_len,
            cap: self.cap,
            cells,
            sizings,
        })
    }
}

struct Ring {
    cells: Vec<Cell>,
    soft_wrap: Box<[bool]>,
    occupancy: Box<[u16]>,
}

/// The handle spaces a ring's cells may name.
struct Tables<'a> {
    styles: &'a mut StyleTable,
    links: usize,
    clusters: usize,
}

impl RingDump {
    fn restore(&self, tables: &mut Tables<'_>) -> Result<Ring, String> {
        let (rows, cols) = (usize::from(self.rows), usize::from(self.cols));
        if rows == 0 || cols == 0 || cols > MAX_CELLS_PER_ROW {
            return Err("ring geometry".to_owned());
        }
        let ring_rows = rows.checked_add(self.cap).ok_or("ring capacity")?;
        if self.phys_cap < rows
            || self.phys_cap > ring_rows
            || self.history_len > self.cap
            || self.history_len + rows > self.phys_cap
            || self.base >= self.phys_cap
            || self.cells.len() != self.phys_cap
        {
            return Err("ring geometry".to_owned());
        }
        let len = self.phys_cap * cols;
        let mut cells = Vec::new();
        // The reservation `ring_cells` takes, so history growth after the
        // restore never reallocates either.
        let reserve = ring_rows.checked_mul(cols).ok_or("ring capacity")?;
        cells
            .try_reserve_exact(reserve.max(len))
            .map_err(|err| err.to_string())?;
        cells.resize(len, Cell::default());
        let mut soft_wrap = vec![false; self.phys_cap].into_boxed_slice();
        let mut occupancy = vec![0u16; self.phys_cap].into_boxed_slice();
        let pens = tables.styles.len();
        for (row, text) in self.cells.iter().enumerate() {
            let payload =
                felis_protocol::base64::decode(text.as_bytes()).ok_or("a row is not base64")?;
            let decoded = decode_row(&payload, tables.styles).map_err(|err| err.to_string())?;
            if decoded.cells.len() > cols || !decoded.sized_cells.is_empty() {
                return Err(format!("row {row} is wider than its ring"));
            }
            for cell in &decoded.cells {
                let link_ok = cell
                    .link
                    .is_none_or(|link| usize::from(link.get()) <= tables.links);
                let cluster_ok = match cell.grapheme {
                    Grapheme::Cluster(handle) => handle.get() as usize <= tables.clusters,
                    _ => true,
                };
                if !link_ok || !cluster_ok {
                    return Err(format!("row {row} names a handle past its table"));
                }
            }
            let start = row * cols;
            cells[start..start + decoded.cells.len()].copy_from_slice(&decoded.cells);
            soft_wrap[row] = decoded.soft_wrap_continued;
            occupancy[row] = decoded.cells.len() as u16;
        }
        if tables.styles.len() != pens {
            return Err("a row names a pen the style table lacks".to_owned());
        }
        for &(row, col, handle) in &self.sizings {
            let handle = SizingHandle::new(handle).ok_or("sizing handle 0")?;
            if row >= self.phys_cap || col >= occupancy[row] {
                return Err("a sizing names an empty cell".to_owned());
            }
            cells[row * cols + usize::from(col)].sizing = Some(handle);
        }
        Ok(Ring {
            cells,
            soft_wrap,
            occupancy,
        })
    }
}

impl SavedDump {
    fn of(saved: &SavedScreen, styles: &StyleTable) -> Result<Self, String> {
        Ok(Self {
            ring: RingRef::of_saved(saved).dump(styles)?,
            cursor: saved.cursor,
            pen: saved.pen,
        })
    }

    fn restore(&self, tables: &mut Tables<'_>) -> Result<SavedScreen, String> {
        let ring = self.ring.restore(tables)?;
        if self.cursor.row >= self.ring.rows || self.cursor.col >= self.ring.cols {
            return Err("saved cursor outside its screen".to_owned());
        }
        Ok(SavedScreen {
            cells: ring.cells,
            rows: self.ring.rows,
            cols: self.ring.cols,
            base: self.ring.base,
            phys_cap: self.ring.phys_cap,
            history_len: self.ring.history_len,
            cap: self.ring.cap,
            soft_wrap: ring.soft_wrap,
            occupancy: ring.occupancy,
            cursor: self.cursor,
            pen: self.pen,
        })
    }
}

impl ScreenDump {
    fn of(screen: &ScreenBuffer) -> Result<Self, String> {
        let styles = &screen.style_table;
        let ring = RingRef {
            rows: screen.rows,
            cols: screen.cols,
            base: screen.base,
            phys_cap: screen.phys_cap,
            history_len: screen.history_len,
            cap: screen.cap,
            cells: &screen.cells,
            soft_wrap: &screen.soft_wrap,
            occupancy: &screen.occupancy,
        }
        .dump(styles)?;
        let links = (1..=screen.link_table.len())
            .map(|handle| {
                let entry = u16::try_from(handle)
                    .ok()
                    .and_then(NonZeroU16::new)
                    .and_then(|handle| screen.link_table.get(handle))
                    .ok_or("a gap in the hyperlink table")?;
                Ok(LinkDump {
                    id: entry.id.as_ref().map(|id| id.as_str().to_owned()),
                    uri: entry.uri.as_str().to_owned(),
                })
            })
            .collect::<Result<_, String>>()?;
        let clusters = (1..=screen.cluster_table.len())
            .map(|handle| {
                u32::try_from(handle)
                    .ok()
                    .and_then(NonZeroU32::new)
                    .and_then(|handle| screen.cluster_table.get(handle))
                    .map(str::to_owned)
                    .ok_or_else(|| "a gap in the cluster table".to_owned())
            })
            .collect::<Result<_, String>>()?;
        Ok(Self {
            row_codec: ROW_CODEC_VERSION,
            ring,
            band_top: screen.band_top,
            band_len: screen.band_len,
            band_rot: screen.band_rot,
            cursor: screen.cursor,
            cursor_style: screen.cursor_style,
            cursor_blink: screen.cursor_blink,
            styles: styles.entries().to_vec(),
            damage: screen.damage.clone(),
            saved_primary: screen
                .saved_primary
                .as_ref()
                .map(|saved| SavedDump::of(saved, styles))
                .transpose()?,
            saved_alternate: screen
                .saved_alternate
                .as_ref()
                .map(|saved| SavedDump::of(saved, styles))
                .transpose()?,
            links,
            clusters,
            sizing_table: screen.sizing_table.clone(),
            geometry_gen: screen.geometry_gen,
            scroll_seq: screen.scroll_seq,
        })
    }

    fn restore(self) -> Result<ScreenBuffer, String> {
        if self.row_codec != ROW_CODEC_VERSION {
            return Err(format!(
                "row codec {} is not the version {ROW_CODEC_VERSION} this build reads",
                self.row_codec
            ));
        }
        let mut style_table =
            StyleTable::from_entries(&self.styles).ok_or("the style table is not a set of pens")?;
        let mut link_table = crate::LinkTable::default();
        for (slot, link) in self.links.into_iter().enumerate() {
            let handle = u16::try_from(slot + 1)
                .ok()
                .and_then(NonZeroU16::new)
                .ok_or("hyperlink table past its handle space")?;
            let entry = HyperlinkEntry {
                id: link
                    .id
                    .map(|id| LinkText::new(&id).ok_or("hyperlink id past its cap"))
                    .transpose()?,
                uri: LinkText::new(&link.uri).ok_or("hyperlink past its cap")?,
            };
            if !link_table.install(handle, entry) {
                return Err("hyperlink table past its cap".to_owned());
            }
        }
        let mut cluster_table = crate::ClusterTable::default();
        for (slot, text) in self.clusters.into_iter().enumerate() {
            let handle = u32::try_from(slot + 1)
                .ok()
                .and_then(NonZeroU32::new)
                .ok_or("cluster table past its handle space")?;
            let text = ClusterText::new(&text).ok_or("cluster past its cap")?;
            if !cluster_table.install(handle, text) {
                return Err("cluster table past its cap".to_owned());
            }
        }
        let mut tables = Tables {
            styles: &mut style_table,
            links: link_table.len(),
            clusters: cluster_table.len(),
        };
        let ring = self.ring.restore(&mut tables)?;
        let saved_primary = self
            .saved_primary
            .map(|saved| saved.restore(&mut tables))
            .transpose()?;
        let saved_alternate = self
            .saved_alternate
            .map(|saved| saved.restore(&mut tables))
            .transpose()?;
        let mut screen = ScreenBuffer {
            rows: self.ring.rows,
            cols: self.ring.cols,
            cells: ring.cells,
            base: self.ring.base,
            band_top: self.band_top,
            band_len: self.band_len,
            band_rot: self.band_rot,
            soft_wrap: ring.soft_wrap,
            occupancy: ring.occupancy,
            cursor: self.cursor,
            cursor_style: self.cursor_style,
            cursor_blink: self.cursor_blink,
            style_table,
            damage: self.damage,
            cap: self.ring.cap,
            phys_cap: self.ring.phys_cap,
            history_len: self.ring.history_len,
            saved_primary,
            saved_alternate,
            link_table,
            cluster_table,
            sizing_table: self.sizing_table,
            has_sized_cells: false,
            geometry_gen: self.geometry_gen,
            scroll_seq: self.scroll_seq,
        };
        screen.refresh_has_sized_cells();
        Ok(screen)
    }
}

pub(crate) mod screen {
    use super::{Deserialize, Deserializer, ScreenBuffer, ScreenDump, Serialize, Serializer};
    use serde::{de::Error as _, ser::Error as _};

    pub(crate) fn serialize<S: Serializer>(
        screen: &ScreenBuffer,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        ScreenDump::of(screen)
            .map_err(S::Error::custom)?
            .serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ScreenBuffer, D::Error> {
        ScreenDump::deserialize(deserializer)?
            .restore()
            .map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests;
