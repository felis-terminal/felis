use felis_vt::program_status::{Kind, Report, State};
use proptest::prelude::*;

use felis_vt::program_status::limits::RECORDS_MAX;

use super::ProgramStatusStore;

fn report(id: &str, state: State) -> Report {
    Report {
        id: id.to_owned(),
        state,
        kind: None,
        progress: None,
        app: None,
        title: None,
        msg: None,
    }
}

fn ids(store: &ProgramStatusStore) -> Vec<String> {
    store.snapshot().into_iter().map(|r| r.id).collect()
}

fn states(store: &ProgramStatusStore) -> Vec<(String, String)> {
    store
        .snapshot()
        .into_iter()
        .map(|r| (r.id, r.state))
        .collect()
}

#[test]
fn a_report_replaces_its_record_whole() {
    let mut store = ProgramStatusStore::default();
    store.report(Report {
        kind: Some(Kind::Permission),
        progress: Some(40),
        app: Some("cargo".into()),
        title: Some("tests".into()),
        msg: Some("allow network?".into()),
        ..report("build", State::Blocked)
    });
    store.report(report("build", State::Working));

    let records = store.snapshot();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.state, "working");
    assert_eq!(
        (
            &record.kind,
            record.progress,
            &record.app,
            &record.title,
            &record.msg
        ),
        (&None, None, &None, &None, &None),
        "keys the new report left out are gone"
    );
}

#[test]
fn a_record_ships_every_key_as_sent() {
    let mut store = ProgramStatusStore::default();
    store.report(Report {
        kind: Some(Kind::Question),
        progress: Some(0),
        app: Some("claude-code".into()),
        title: Some("refactor".into()),
        msg: Some("which branch?".into()),
        ..report("agent/1", State::Blocked)
    });
    let record = &store.snapshot()[0];
    assert_eq!(record.id, "agent/1");
    assert_eq!(record.state, "blocked");
    assert_eq!(record.kind.as_deref(), Some("question"));
    assert_eq!(record.progress, Some(0));
    assert_eq!(record.app.as_deref(), Some("claude-code"));
    assert_eq!(record.title.as_deref(), Some("refactor"));
    assert_eq!(record.msg.as_deref(), Some("which branch?"));
}

/// `app` is the record's own: a child without one does not take its
/// parent's.
#[test]
fn a_child_record_does_not_inherit_its_parents_app() {
    let mut store = ProgramStatusStore::default();
    store.report(Report {
        app: Some("cargo".into()),
        ..report("build", State::Working)
    });
    store.report(report("build/test", State::Working));
    let records = store.snapshot();
    assert_eq!(records[1].id, "build/test");
    assert_eq!(records[1].app, None);
}

#[test]
fn the_snapshot_is_sorted_by_id_with_the_root_first() {
    let mut store = ProgramStatusStore::default();
    for id in ["b", "a/z", "", "a"] {
        store.report(report(id, State::Idle));
    }
    assert_eq!(ids(&store), ["", "a", "a/z", "b"]);
}

#[test]
fn clearing_an_id_removes_it_and_its_descendants_only() {
    let mut store = ProgramStatusStore::default();
    for id in ["", "a", "a/b", "a/b/c", "ab", "a.b", "a-b/c", "b/a"] {
        store.report(report(id, State::Working));
    }
    store.clear("a");
    assert_eq!(ids(&store), ["", "a-b/c", "a.b", "ab", "b/a"]);
}

#[test]
fn clearing_a_child_leaves_its_parent() {
    let mut store = ProgramStatusStore::default();
    for id in ["a", "a/b", "a/b/c", "a/bc"] {
        store.report(report(id, State::Working));
    }
    store.clear("a/b");
    assert_eq!(ids(&store), ["a", "a/bc"]);
}

#[test]
fn clearing_the_root_id_removes_every_record() {
    let mut store = ProgramStatusStore::default();
    for id in ["", "a", "a/b", "z"] {
        store.report(report(id, State::Done));
    }
    store.clear("");
    assert!(store.is_empty());
}

#[test]
fn a_clear_that_matches_nothing_leaves_the_generation() {
    let mut store = ProgramStatusStore::default();
    store.report(report("a", State::Done));
    let generation = store.generation();
    store.clear("b");
    store.clear("a/b");
    assert_eq!(store.generation(), generation);
    assert_eq!(ids(&store), ["a"]);
}

#[test]
fn expiry_drops_working_and_blocked_at_every_depth() {
    let mut store = ProgramStatusStore::default();
    store.report(report("", State::Working));
    store.report(report("idle", State::Idle));
    store.report(report("done", State::Done));
    store.report(report("error", State::Error));
    store.report(report("idle/working", State::Working));
    store.report(Report {
        kind: Some(Kind::Auth),
        ..report("done/blocked", State::Blocked)
    });
    store.expire_transient();
    assert_eq!(
        states(&store),
        [
            ("done".to_owned(), "done".to_owned()),
            ("error".to_owned(), "error".to_owned()),
            ("idle".to_owned(), "idle".to_owned()),
        ]
    );
}

#[test]
fn an_expiry_with_nothing_transient_leaves_the_generation() {
    let mut store = ProgramStatusStore::default();
    store.report(report("", State::Idle));
    let generation = store.generation();
    store.expire_transient();
    assert_eq!(store.generation(), generation);
}

#[test]
fn a_reset_removes_every_record() {
    let mut store = ProgramStatusStore::default();
    store.report(report("", State::Done));
    store.report(report("x", State::Error));
    store.reset();
    assert!(store.is_empty());
}

fn fill_to_the_cap(store: &mut ProgramStatusStore) {
    for i in 0..RECORDS_MAX {
        store.report(report(&format!("r{i:03}"), State::Working));
    }
    assert_eq!(store.len(), RECORDS_MAX);
}

#[test]
fn a_new_id_at_the_cap_evicts_the_least_recently_updated() {
    let mut store = ProgramStatusStore::default();
    fill_to_the_cap(&mut store);
    store.report(report("new", State::Working));
    assert_eq!(store.len(), RECORDS_MAX);
    let ids = ids(&store);
    assert!(!ids.contains(&"r000".to_owned()), "the oldest went");
    assert!(ids.contains(&"r001".to_owned()));
    assert!(ids.contains(&"new".to_owned()));
}

/// Recency is by update, not creation: replacing the oldest record
/// makes the next oldest the one to go.
#[test]
fn an_update_protects_a_record_from_eviction() {
    let mut store = ProgramStatusStore::default();
    fill_to_the_cap(&mut store);
    store.report(report("r000", State::Done));
    store.report(report("new", State::Working));
    let ids = ids(&store);
    assert!(ids.contains(&"r000".to_owned()));
    assert!(!ids.contains(&"r001".to_owned()));
}

#[test]
fn replacing_a_record_at_the_cap_evicts_nothing() {
    let mut store = ProgramStatusStore::default();
    fill_to_the_cap(&mut store);
    let before = ids(&store);
    store.report(report("r010", State::Done));
    assert_eq!(ids(&store), before);
}

#[derive(Debug, Clone)]
enum Op {
    Report(String, State),
    Clear(String),
    Expire,
    Reset,
}

/// Small enough that random ops reach the cap and evict often.
const MODEL_CAP: usize = 8;
const ALPHABET: usize = MODEL_CAP + 4;

fn id_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        (0..ALPHABET).prop_map(|i| format!("p{i}")),
        (0..4usize, 0..3usize).prop_map(|(i, j)| format!("p{i}/c{j}")),
    ]
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let state = prop::sample::select(State::ALL.to_vec());
    prop_oneof![
        12 => (id_strategy(), state).prop_map(|(id, s)| Op::Report(id, s)),
        2 => id_strategy().prop_map(Op::Clear),
        1 => Just(Op::Expire),
        1 => Just(Op::Reset),
    ]
}

/// Least recently updated first.
#[derive(Default)]
struct Model(Vec<(String, State)>);

impl Model {
    fn apply(&mut self, op: &Op) {
        match op {
            Op::Report(id, state) => {
                if let Some(at) = self.0.iter().position(|(i, _)| i == id) {
                    self.0.remove(at);
                } else if self.0.len() == MODEL_CAP {
                    self.0.remove(0);
                }
                self.0.push((id.clone(), *state));
            }
            Op::Clear(id) if id.is_empty() => self.0.clear(),
            Op::Clear(id) => self
                .0
                .retain(|(i, _)| i != id && !i.starts_with(&format!("{id}/"))),
            Op::Expire => self
                .0
                .retain(|(_, s)| !matches!(s, State::Working | State::Blocked)),
            Op::Reset => self.0.clear(),
        }
    }

    fn sorted(&self) -> Vec<(String, String)> {
        let mut out: Vec<_> = self
            .0
            .iter()
            .map(|(id, s)| (id.clone(), s.as_str().to_owned()))
            .collect();
        out.sort();
        out
    }
}

proptest! {
    #[test]
    fn the_store_matches_a_move_to_back_model(ops in prop::collection::vec(op_strategy(), 0..200)) {
        let mut store = ProgramStatusStore::with_cap(MODEL_CAP);
        let mut model = Model::default();
        for op in &ops {
            let before = store.snapshot();
            let generation = store.generation();
            match op {
                Op::Report(id, state) => store.report(report(id, *state)),
                Op::Clear(id) => store.clear(id),
                Op::Expire => store.expire_transient(),
                Op::Reset => store.reset(),
            }
            model.apply(op);
            prop_assert!(store.len() <= MODEL_CAP);
            prop_assert_eq!(store.is_empty(), model.0.is_empty());
            prop_assert_eq!(states(&store), model.sorted());
            let changed = store.snapshot() != before;
            let reported = matches!(op, Op::Report(..));
            prop_assert_eq!(
                store.generation() > generation,
                changed || reported,
                "the generation only rises, and does when the set changed (a report always rewrites its record)"
            );
            prop_assert!(store.generation() >= generation);
        }
    }
}
