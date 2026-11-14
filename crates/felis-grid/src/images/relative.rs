//! Relative placements (`P=`, `Q=`, `H=`, `V=`): a placement positioned
//! off another and living only as long as it
//! (`docs/reference/protocols/kitty-graphics.md` "Relative placements").

use std::collections::{HashMap, HashSet};

use super::table::{Key, Keyed};
use super::{CellPos, ImageId, Placement, PlacementId, Placements, VirtualPlacement};
use crate::ScreenBuffer;

/// The longest chain of parent links a put may make: the spec's floor,
/// and kitty's `PARENT_DEPTH_LIMIT`.
pub const MAX_PARENT_DEPTH: usize = 8;

/// Where a relative placement hangs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "state-dump", derive(serde::Serialize, serde::Deserialize))]
pub struct Parent {
    pub image_id: ImageId,
    pub placement_id: PlacementId,
    /// The parent's [`Placement::serial`]: a placement put under its key
    /// after the parent left is not the parent.
    pub serial: u64,
    /// `H=`, in cells, positive to the right.
    pub cols: i32,
    /// `V=`, in cells, positive downward.
    pub rows: i32,
    /// The chain ended at a cell the last time it settled.
    pub shown: bool,
}

/// Why a put cannot hang where it asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentError {
    /// `ENOPARENT`: no such placement.
    Missing,
    /// `EINVAL`: the placement named itself.
    Itself,
    /// `ECYCLE`: the parent hangs, at some remove, off the placement.
    Cycle,
    /// `ETOODEEP`: the chain would pass [`MAX_PARENT_DEPTH`] links.
    TooDeep,
}

/// What [`Placements::settle_relatives`] changed.
#[derive(Debug, Default)]
pub struct Settled {
    /// Drawn at a new place, or drawn again.
    pub shown: Vec<Placement>,
    /// Kept, but not drawn.
    pub hidden: Vec<Placement>,
    /// Removed with their parent.
    pub removed: Vec<Placement>,
}

/// The top-left cell of each virtual placement's placeholder cells on
/// the screen: the least row and, apart, the least column.
pub type VirtualOrigins = HashMap<(ImageId, Option<PlacementId>), CellPos>;

const fn parent_key(parent: &Parent) -> Key {
    (parent.image_id, Some(parent.placement_id))
}

impl Placements {
    /// kitty's parent lookup: `Q=0` takes the image's first-created
    /// placement of either kind, any other `Q=` the placement a producer
    /// put under it.
    /// Returns the parent's id and serial.
    #[must_use]
    pub fn find_parent(&self, image_id: ImageId, placement_id: u32) -> Option<(PlacementId, u64)> {
        if placement_id == 0 {
            let direct = self
                .entries
                .of_image(image_id)
                .map(|p| (p.serial, p.placement_id));
            let virt = self
                .virtuals
                .of_image(image_id)
                .map(|v| (v.serial, v.placement_id));
            let (serial, id) = direct.chain(virt).min_by_key(|&(serial, _)| serial)?;
            return Some((id?, serial));
        }
        let key = (image_id, Some(PlacementId(placement_id)));
        let direct = self
            .entries
            .get(key)
            .filter(|p| !p.anonymous)
            .map(|p| p.serial);
        let virt = || {
            self.virtuals
                .get(key)
                .filter(|v| !v.anonymous)
                .map(|v| v.serial)
        };
        Some((PlacementId(placement_id), direct.or_else(virt)?))
    }

    /// kitty's `has_good_ancestry` for a put under `key` off `parent`.
    ///
    /// # Errors
    ///
    /// When `parent` leads back to `key` or past [`MAX_PARENT_DEPTH`] links.
    pub fn check_parent(
        &self,
        key: (ImageId, Option<PlacementId>),
        parent: (ImageId, PlacementId),
    ) -> Result<(), ParentError> {
        let mut node = (parent.0, Some(parent.1));
        if node == key {
            return Err(ParentError::Itself);
        }
        let mut links = 1;
        loop {
            if node == key {
                return Err(ParentError::Cycle);
            }
            let Some(next) = self.entries.get(node).and_then(|p| self.live_parent(p)) else {
                return Ok(());
            };
            links += 1;
            if links > MAX_PARENT_DEPTH {
                return Err(ParentError::TooDeep);
            }
            node = next;
        }
    }

    /// Whether the placement under `key`, of either kind, has an id no
    /// producer gave it.
    #[must_use]
    pub fn is_anonymous(&self, key: (ImageId, Option<PlacementId>)) -> bool {
        self.entries
            .get(key)
            .map(|p| p.anonymous)
            .or_else(|| self.virtuals.get(key).map(|v| v.anonymous))
            .unwrap_or(false)
    }

    /// The id of the image's placement created as `serial`, which a
    /// rekey keeps.
    #[must_use]
    pub fn id_of_serial(&self, image_id: ImageId, serial: u64) -> Option<PlacementId> {
        self.entries
            .of_image(image_id)
            .find(|p| p.serial == serial)
            .and_then(|p| p.placement_id)
            .or_else(|| {
                self.virtuals
                    .of_image(image_id)
                    .find(|v| v.serial == serial)
                    .and_then(|v| v.placement_id)
            })
    }

    /// The key of `p`'s parent while that placement is still the one it
    /// was put against.
    fn live_parent(&self, p: &Placement) -> Option<Key> {
        let parent = p.parent.as_ref()?;
        let key = parent_key(parent);
        let serial = self
            .entries
            .get(key)
            .map(|q| q.serial)
            .or_else(|| self.virtuals.get(key).map(|v| v.serial))?;
        (serial == parent.serial).then_some(key)
    }

    /// Whether [`Self::settle_relatives`] has anything to do.
    #[must_use]
    pub const fn has_relatives(&self) -> bool {
        self.relatives
    }

    /// Whether a chain may end at a virtual placement, which only the
    /// screen's placeholder cells place.
    #[must_use]
    pub fn needs_virtual_origins(&self) -> bool {
        self.relatives && !self.virtuals.is_empty()
    }

    /// kitty's `resolve_cell_ref` over `screen`'s placeholder cells.
    #[must_use]
    pub fn virtual_origins(
        &self,
        screen: &ScreenBuffer,
        images: &super::ImageStore,
        (cw, ch): (u16, u16),
    ) -> VirtualOrigins {
        let mut origins = VirtualOrigins::new();
        let mut last: Option<((ImageId, u32), Option<&VirtualPlacement>)> = None;
        for cell in screen.placeholder_cells() {
            let named = (cell.image_id, cell.placement_id);
            let shown = match last {
                Some((held, shown)) if held == named => shown,
                _ => {
                    let shown = self.virtual_for_cell(cell.image_id, cell.placement_id);
                    last = Some((named, shown));
                    shown
                }
            };
            // A tile outside the placement's box draws nothing.
            let Some(v) = shown.filter(|v| {
                let Some(image) = images.get(v.image_id) else {
                    return false;
                };
                let (cols, rows) = crate::image_layout::placeholder_grid_extent(
                    v.cols,
                    v.rows,
                    image.width,
                    image.height,
                    f32::from(cw),
                    f32::from(ch),
                );
                cell.tile_col < u32::from(cols) && cell.tile_row < u32::from(rows)
            }) else {
                continue;
            };
            let key = v.key();
            let at = CellPos {
                row: i32::from(cell.row) + 1,
                col: cell.col.saturating_add(1),
            };
            origins
                .entry(key)
                .and_modify(|o| {
                    o.row = o.row.min(at.row);
                    o.col = o.col.min(at.col);
                })
                .or_insert(at);
        }
        origins
    }

    /// Removes the relative placements whose parent is gone, transitively,
    /// and moves the rest onto their parents, as kitty does each time it
    /// lays out a frame. A chain ending at a virtual placement takes its
    /// origin from `origins`, and hides when it has none.
    pub fn settle_relatives(&mut self, origins: &VirtualOrigins) -> Settled {
        let mut settled = Settled::default();
        if !self.relatives {
            return settled;
        }
        loop {
            let orphans: Vec<Key> = self
                .entries
                .iter()
                .filter(|p| p.parent.is_some() && self.live_parent(p).is_none())
                .map(Keyed::key)
                .collect();
            if orphans.is_empty() {
                break;
            }
            settled.removed.extend(
                orphans
                    .into_iter()
                    .filter_map(|key| self.entries.remove(key)),
            );
        }
        let resolved: Vec<(Key, Option<CellPos>)> = self
            .entries
            .iter()
            .filter(|p| p.parent.is_some())
            .map(|p| (p.key(), self.resolve(p, origins)))
            .collect();
        self.relatives = !resolved.is_empty();
        for (key, at) in resolved {
            let Some(p) = self.entries.get_mut(key) else {
                continue;
            };
            let Some(parent) = p.parent.as_mut() else {
                continue;
            };
            match at {
                Some(at) if !parent.shown || p.anchor != at => {
                    parent.shown = true;
                    p.anchor = at;
                    settled.shown.push(p.clone());
                }
                None if parent.shown => {
                    parent.shown = false;
                    settled.hidden.push(p.clone());
                }
                _ => {}
            }
        }
        settled
    }

    /// The anchor `p`'s chain puts it at; the walk is bounded although a
    /// put cannot close a cycle.
    fn resolve(&self, p: &Placement, origins: &VirtualOrigins) -> Option<CellPos> {
        let (mut rows, mut cols) = (0i64, 0i64);
        let mut link = p.parent?;
        for _ in 0..=self.entries.len() {
            rows += i64::from(link.rows);
            cols += i64::from(link.cols);
            let key = parent_key(&link);
            let root = match self.entries.get(key) {
                Some(parent) => match parent.parent {
                    Some(next) => {
                        link = next;
                        continue;
                    }
                    None => parent.anchor,
                },
                None => *origins.get(&key)?,
            };
            let row = i64::from(root.row) + rows;
            let col = i64::from(root.col) + cols;
            return Some(CellPos {
                row: i32::try_from(row).unwrap_or(if row < 0 { i32::MIN } else { i32::MAX }),
                col: u16::try_from(col.max(1)).unwrap_or(u16::MAX),
            });
        }
        None
    }

    /// The relative placements whose chain ends at a virtual placement:
    /// they draw from the screen's placeholder cells, so a screen clear
    /// takes them.
    pub(super) fn virtually_rooted(&self) -> HashSet<Key> {
        if !self.relatives {
            return HashSet::new();
        }
        self.entries
            .iter()
            .filter(|p| {
                let mut node = Some(*p);
                for _ in 0..=self.entries.len() {
                    let Some(link) = node.and_then(|n| n.parent) else {
                        return false;
                    };
                    let key = parent_key(&link);
                    if self.virtuals.contains(key) {
                        return true;
                    }
                    node = self.entries.get(key);
                }
                false
            })
            .map(Keyed::key)
            .collect()
    }

    /// A parent moving off `from` to the fresh `to` takes its relative
    /// placements with it.
    pub(super) fn relink(&mut self, from: Key, to: PlacementId) {
        if !self.relatives {
            return;
        }
        for p in self.entries.values_mut() {
            if let Some(parent) = p.parent.as_mut()
                && parent_key(parent) == from
            {
                parent.placement_id = to;
            }
        }
    }
}
