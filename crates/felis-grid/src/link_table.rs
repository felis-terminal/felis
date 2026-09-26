//! Interned OSC 8 hyperlink registry
//! (`docs/explanation/data-model/grid-and-cells.md` "Hyperlink interning").
//! The table grows monotonically: cells retain handles, so entries are never
//! freed. Only the daemon mints handles; clients install them as rows arrive.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::num::NonZeroU16;

use foldhash::fast::FixedState;

use crate::HyperlinkEntry;

/// One half of an `OSC 8` anchor (the `id=` or the URI), at most
/// [`Self::CAP`] bytes. A type rather than a check at the interner
/// because a `GridMsg::Hyperlink` off the wire reaches the table
/// without passing the parser, and the peer chooses its length.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LinkText(Box<str>);

impl LinkText {
    /// The parser's own limit, so nothing a producer can emit is
    /// refused here.
    pub const CAP: usize = felis_vt::OSC_BUFFER_LIMIT;

    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        (text.len() <= Self::CAP).then(|| Self(text.into()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LinkText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<&str> for LinkText {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

/// Maximum bytes one grid's hyperlink table may charge for.
/// 8 MiB stays under the default scrollback memory cost while admitting
/// the whole handle space at realistic URI lengths (~64 bytes).
pub const LINK_TABLE_BYTE_CAP: usize = 8 * 1024 * 1024;

/// Keyed by digest rather than the strings so the index holds no
/// second copy of every URI; a hit is confirmed against the entry, so a
/// collision costs a duplicate row, never a wrong link.
type LinkMap = HashMap<u64, NonZeroU16, FixedState>;

/// Grid-owned registry mapping [`NonZeroU16`] handles to
/// [`HyperlinkEntry`] values.
#[derive(Debug, Clone, Default)]
pub struct LinkTable {
    /// `None` is a gap: a handle whose entry has not arrived. The
    /// visible-first rehydrate feeding [`Self::install`] sends entry
    /// 100 before 1–99.
    entries: Vec<Option<HyperlinkEntry>>,
    dedup: LinkMap,
    bytes: usize,
}

impl PartialEq for LinkTable {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for LinkTable {}

impl LinkTable {
    /// `None` for a handle past the table or on a gap, which a
    /// reattaching client can hold before the `Hyperlink` message
    /// streams.
    #[must_use]
    pub fn get(&self, id: NonZeroU16) -> Option<&HyperlinkEntry> {
        self.entries.get(id.get() as usize - 1)?.as_ref()
    }

    /// Handles the table spans, gaps included.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `None` when the id space is exhausted or the entry would push
    /// the table past [`LINK_TABLE_BYTE_CAP`]; the caller leaves the
    /// pen unset.
    pub fn intern(&mut self, id: Option<&str>, uri: &str) -> Option<NonZeroU16> {
        let key = Self::digest(id, uri);
        if let Some(&existing) = self.dedup.get(&key)
            && let Some(entry) = self.get(existing)
            && entry.id.as_ref().map(LinkText::as_str) == id
            && entry.uri == uri
        {
            return Some(existing);
        }
        let uri = LinkText::new(uri)?;
        let id = match id {
            Some(id) => Some(LinkText::new(id)?),
            None => None,
        };
        let charge = Self::charge(id.as_ref().map(LinkText::as_str), uri.as_str());
        if self.bytes + charge > LINK_TABLE_BYTE_CAP {
            return None;
        }
        // Minted before the push: a table already holding `u16::MAX`
        // entries must not grow by one no cell could name.
        let handle = NonZeroU16::new(u16::try_from(self.entries.len() + 1).ok()?)?;
        self.entries.push(Some(HyperlinkEntry { id, uri }));
        self.bytes += charge;
        self.dedup.insert(key, handle);
        Some(handle)
    }

    /// Install an entry at a daemon-assigned handle (the client side),
    /// growing the table with gaps past the current span. `false` when
    /// the id already holds an entry, whose handle cells carry, and when
    /// the entry would push the table past [`LINK_TABLE_BYTE_CAP`], so a
    /// peer that does not cap its own table cannot grow this one.
    pub fn install(&mut self, id: NonZeroU16, entry: HyperlinkEntry) -> bool {
        let idx = usize::from(id.get() - 1);
        if self.get(id).is_some() {
            return false;
        }
        let charge = Self::charge(entry.id.as_ref().map(LinkText::as_str), entry.uri.as_str());
        let key = Self::digest(entry.id.as_ref().map(LinkText::as_str), entry.uri.as_str());
        // Before the slot vector grows: a refused entry must leave no
        // gap behind.
        let after = self.bytes + charge;
        if after > LINK_TABLE_BYTE_CAP {
            return false;
        }
        if idx >= self.entries.len() {
            self.entries.resize_with(idx + 1, || None);
        }
        self.bytes = after;
        self.entries[idx] = Some(entry);
        self.dedup.insert(key, id);
        true
    }

    fn charge(id: Option<&str>, uri: &str) -> usize {
        size_of::<HyperlinkEntry>()
            + size_of::<(u64, NonZeroU16)>()
            + uri.len()
            + id.map_or(0, str::len)
    }

    fn digest(id: Option<&str>, uri: &str) -> u64 {
        FixedState::default().hash_one((id, uri))
    }
}

#[cfg(test)]
mod tests;
