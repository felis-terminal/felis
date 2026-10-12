//! One session's OSC 7501 program-status records
//! (`docs/reference/protocols/program-status.md`). The grid only decodes
//! and queues; replace, clear, eviction and lifetimes are applied here.

use std::collections::BTreeMap;

use felis_protocol::messages::ProgramStatusRecord;
use felis_vt::program_status::limits::RECORDS_MAX;
use felis_vt::program_status::{Report, State};

#[derive(Debug)]
pub struct ProgramStatusStore {
    records: BTreeMap<String, Entry>,
    cap: usize,
    clock: u64,
    generation: u64,
}

#[derive(Debug)]
struct Entry {
    report: Report,
    updated: u64,
}

impl Default for ProgramStatusStore {
    fn default() -> Self {
        Self::with_cap(RECORDS_MAX)
    }
}

impl ProgramStatusStore {
    const fn with_cap(cap: usize) -> Self {
        Self {
            records: BTreeMap::new(),
            cap,
            clock: 0,
            generation: 0,
        }
    }

    /// Moves whenever the record set changes, so a mirror rebuilds its
    /// snapshot only then.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The report becomes the whole record: a key it leaves out is gone.
    pub fn report(&mut self, report: Report) {
        if !self.records.contains_key(&report.id) && self.records.len() >= self.cap {
            self.evict_least_recently_updated();
        }
        self.clock += 1;
        let updated = self.clock;
        self.records
            .insert(report.id.clone(), Entry { report, updated });
        self.generation += 1;
    }

    /// `""` removes every record; otherwise `id` and its descendants.
    pub fn clear(&mut self, id: &str) {
        let before = self.records.len();
        if id.is_empty() {
            self.records.clear();
        } else {
            self.records
                .retain(|key, _| !(key == id || is_descendant(key, id)));
        }
        if self.records.len() != before {
            self.generation += 1;
        }
    }

    /// A new shell prompt or the end of the PTY: drops working and
    /// blocked records, keeps idle, done and error.
    pub fn expire_transient(&mut self) {
        let before = self.records.len();
        self.records
            .retain(|_, entry| !matches!(entry.report.state, State::Working | State::Blocked));
        if self.records.len() != before {
            self.generation += 1;
        }
    }

    /// RIS.
    pub fn reset(&mut self) {
        self.clear("");
    }

    /// Sorted by id, every value as the program sent it.
    #[must_use]
    pub fn snapshot(&self) -> Vec<ProgramStatusRecord> {
        self.records
            .values()
            .map(|entry| record_of(&entry.report))
            .collect()
    }

    fn evict_least_recently_updated(&mut self) {
        let oldest = self
            .records
            .iter()
            .min_by_key(|(_, entry)| entry.updated)
            .map(|(id, _)| id.clone());
        if let Some(id) = oldest {
            self.records.remove(&id);
        }
    }
}

fn is_descendant(key: &str, ancestor: &str) -> bool {
    key.strip_prefix(ancestor)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn record_of(report: &Report) -> ProgramStatusRecord {
    ProgramStatusRecord {
        id: report.id.clone(),
        state: report.state.as_str().to_owned(),
        kind: report.kind.map(|kind| kind.as_str().to_owned()),
        progress: report.progress,
        app: report.app.clone(),
        title: report.title.clone(),
        msg: report.msg.clone(),
    }
}

#[cfg(test)]
mod tests;
