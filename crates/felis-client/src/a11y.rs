//! The accessibility tree (`docs/reference/accessibility.md`): the
//! viewport's rows as AccessKit text runs, rebuilt from the shadow after
//! each painted frame and sent as the nodes that differ from the last
//! update.

use std::collections::HashMap;
use std::sync::Arc;

use accesskit::{
    Node, NodeId, Rect, Role, TextDirection, TextPosition, TextSelection, TreeId, TreeInfo,
    TreeUpdate,
};
use felis_client_core::a11y_text::{RUN_CAP, RowText};
use felis_client_core::selection::{GridPos, Selection, SelectionMode};
use felis_grid::ScreenBuffer;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::Window;

use crate::AppEvent;

const ROOT_ID: NodeId = NodeId(0);
const TERMINAL_ID: NodeId = NodeId(1);
const FIRST_RUN_ID: u64 = 2;
/// Run ids per row: enough for every column count the shadow can hold,
/// not just the admitted maximum, because `reflow` resizes the shadow
/// optimistically before the daemon answers. The `+ 1` is the line break.
const RUNS_PER_ROW: u64 = (u16::MAX as u64 + 1).div_ceil(RUN_CAP as u64);

/// The platform side: AccessKit's winit adapter in a window build, a
/// recorder in tests.
pub(crate) trait A11yAdapter {
    fn process_event(&mut self, event: &WindowEvent);
    fn update(&mut self, update: TreeUpdate);
}

struct WinitAdapter {
    adapter: accesskit_winit::Adapter,
    window: Arc<Window>,
}

impl A11yAdapter for WinitAdapter {
    fn process_event(&mut self, event: &WindowEvent) {
        self.adapter.process_event(&self.window, event);
    }

    fn update(&mut self, update: TreeUpdate) {
        self.adapter.update_if_active(|| update);
    }
}

/// What one rebuild reads; borrowed from the App at the flush.
pub(crate) struct TreeSource<'a> {
    pub(crate) screen: &'a ScreenBuffer,
    pub(crate) viewport: u32,
    pub(crate) selection: Option<Selection>,
    pub(crate) title: &'a str,
    pub(crate) cell_px: (f32, f32),
    pub(crate) origin_px: (f32, f32),
}

#[derive(Default)]
pub(crate) struct A11yTree {
    proxy: Option<EventLoopProxy<AppEvent>>,
    adapter: Option<Box<dyn A11yAdapter>>,
    active: bool,
    changed: bool,
    sent: HashMap<NodeId, Node>,
}

impl A11yTree {
    pub(crate) fn with_proxy(proxy: EventLoopProxy<AppEvent>) -> Self {
        Self {
            proxy: Some(proxy),
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn with_adapter(adapter: Box<dyn A11yAdapter>) -> Self {
        Self {
            adapter: Some(adapter),
            ..Self::default()
        }
    }

    /// Must run while `window` is still invisible: AccessKit's adapter
    /// panics on a visible window, and an adapter attached after the
    /// window is shown misses the platform's first query.
    pub(crate) fn attach(&mut self, event_loop: &ActiveEventLoop, window: &Arc<Window>) {
        let Some(proxy) = self.proxy.take() else {
            return;
        };
        let adapter = accesskit_winit::Adapter::with_event_loop_proxy(event_loop, window, proxy);
        self.adapter = Some(Box::new(WinitAdapter {
            adapter,
            window: Arc::clone(window),
        }));
    }

    pub(crate) fn process_event(&mut self, event: &WindowEvent) {
        if let Some(adapter) = self.adapter.as_mut() {
            adapter.process_event(event);
        }
    }

    /// The next flush sends the whole tree, which the event-loop-proxy
    /// adapter requires as its first update.
    pub(crate) fn activate(&mut self) {
        self.active = true;
        self.changed = true;
        self.sent.clear();
    }

    pub(crate) fn deactivate(&mut self) {
        self.active = false;
        self.sent = HashMap::new();
    }

    /// Returns whether a flush will follow, so the caller can request
    /// the redraw that reaches it when nothing else repaints.
    pub(crate) const fn mark_changed(&mut self) -> bool {
        if self.active {
            self.changed = true;
        }
        self.active
    }

    pub(crate) fn flush(&mut self, source: &TreeSource<'_>) {
        if !self.active || !self.changed {
            return;
        }
        self.changed = false;
        let Some(adapter) = self.adapter.as_mut() else {
            return;
        };
        let nodes = build_nodes(source);
        let mut changed = Vec::new();
        let mut sent = HashMap::with_capacity(nodes.len());
        for (id, node) in nodes {
            if self.sent.get(&id) != Some(&node) {
                changed.push((id, node.clone()));
            }
            sent.insert(id, node);
        }
        self.sent = sent;
        if changed.is_empty() {
            return;
        }
        adapter.update(TreeUpdate {
            nodes: changed,
            tree: Some(TreeInfo::new(ROOT_ID)),
            tree_id: TreeId::ROOT,
            focus: TERMINAL_ID,
        });
    }
}

fn run_id(row: u16, run: usize) -> NodeId {
    NodeId(FIRST_RUN_ID + u64::from(row) * RUNS_PER_ROW + run as u64)
}

fn text_position(row: u16, text: &RowText, index: usize) -> TextPosition {
    let (run, character_index) = text.run_position(index);
    TextPosition {
        node: run_id(row, run),
        character_index,
    }
}

/// Columns on each row that must map to a character of their own.
fn pad_columns(source: &TreeSource<'_>, caret: Option<GridPos>) -> HashMap<u16, u16> {
    let mut pads: HashMap<u16, u16> = HashMap::new();
    let mut pad = |p: GridPos| {
        let entry = pads.entry(p.row).or_insert(p.col);
        *entry = (*entry).max(p.col);
    };
    if let Some(caret) = caret {
        pad(caret);
    }
    if let Some(sel) = source.selection
        && sel.mode() == SelectionMode::Linear
    {
        pad(sel.anchor());
        pad(sel.extent());
    }
    pads
}

fn caret_of(source: &TreeSource<'_>) -> Option<GridPos> {
    let cursor = source.screen.cursor();
    let screen = source.screen;
    (cursor.visible
        && source.viewport == 0
        && cursor.row < screen.rows()
        && cursor.col < screen.cols())
    .then_some(GridPos {
        row: cursor.row,
        col: cursor.col,
    })
}

fn selection_of(
    source: &TreeSource<'_>,
    rows: &[RowText],
    caret: Option<GridPos>,
) -> Option<TextSelection> {
    let in_range = |p: GridPos| usize::from(p.row) < rows.len();
    if let Some(sel) = source.selection
        && sel.mode() == SelectionMode::Linear
        && in_range(sel.anchor())
        && in_range(sel.extent())
    {
        let ((lr, lc), (hr, hc)) = sel.range();
        let lo = text_position(
            lr,
            &rows[usize::from(lr)],
            rows[usize::from(lr)].index_of_col(lc),
        );
        let hi = text_position(
            hr,
            &rows[usize::from(hr)],
            rows[usize::from(hr)].index_after_col(hc),
        );
        let forward = sel.anchor() == GridPos { row: lr, col: lc };
        let (anchor, focus) = if forward { (lo, hi) } else { (hi, lo) };
        return Some(TextSelection { anchor, focus });
    }
    let caret = caret?;
    let text = &rows[usize::from(caret.row)];
    let at = text_position(caret.row, text, text.index_of_col(caret.col));
    Some(TextSelection {
        anchor: at,
        focus: at,
    })
}

fn build_nodes(source: &TreeSource<'_>) -> Vec<(NodeId, Node)> {
    let screen = source.screen;
    let (cw, ch) = (f64::from(source.cell_px.0), f64::from(source.cell_px.1));
    let (ox, oy) = (f64::from(source.origin_px.0), f64::from(source.origin_px.1));
    let caret = caret_of(source);
    let pads = pad_columns(source, caret);

    let mut rows: Vec<RowText> = Vec::with_capacity(usize::from(screen.rows()));
    for r in 0..screen.rows() {
        let text = RowText::build(screen, r, pads.get(&r).copied(), rows.last());
        rows.push(text);
    }

    let mut nodes = Vec::new();
    let mut children = Vec::new();
    for (r, text) in rows.iter().enumerate() {
        let r = r as u16;
        let runs = text.run_count();
        for k in 0..runs {
            let first = k * RUN_CAP;
            let last = (first + RUN_CAP).min(text.len());
            let byte_start: usize = text.char_lengths[..first]
                .iter()
                .map(|&l| usize::from(l))
                .sum();
            let byte_len: usize = text.char_lengths[first..last]
                .iter()
                .map(|&l| usize::from(l))
                .sum();
            let col0 = text.char_cols[first];
            let col_end = text.char_cols[last - 1] + text.char_spans[last - 1];
            let mut node = Node::new(Role::TextRun);
            node.set_value(&text.value[byte_start..byte_start + byte_len]);
            node.set_character_lengths(&text.char_lengths[first..last]);
            node.set_character_positions(
                text.char_cols[first..last]
                    .iter()
                    .map(|&c| f32::from(c - col0) * source.cell_px.0)
                    .collect::<Vec<_>>(),
            );
            node.set_character_widths(
                text.char_spans[first..last]
                    .iter()
                    .map(|&s| f32::from(s) * source.cell_px.0)
                    .collect::<Vec<_>>(),
            );
            node.set_word_starts(
                text.word_starts
                    .iter()
                    .filter(|&&i| (first..last).contains(&i))
                    .map(|&i| (i - first) as u8)
                    .collect::<Vec<_>>(),
            );
            node.set_text_direction(TextDirection::LeftToRight);
            let y0 = f64::from(r).mul_add(ch, oy);
            node.set_bounds(Rect {
                x0: f64::from(col0).mul_add(cw, ox),
                y0,
                x1: f64::from(col_end).mul_add(cw, ox),
                y1: y0 + ch,
            });
            if k > 0 {
                node.set_previous_on_line(run_id(r, k - 1));
            }
            if k + 1 < runs {
                node.set_next_on_line(run_id(r, k + 1));
            }
            let id = run_id(r, k);
            children.push(id);
            nodes.push((id, node));
        }
    }

    let mut terminal = Node::new(Role::Terminal);
    terminal.set_label("Terminal");
    terminal.set_bounds(Rect {
        x0: ox,
        y0: oy,
        x1: f64::from(screen.cols()).mul_add(cw, ox),
        y1: f64::from(screen.rows()).mul_add(ch, oy),
    });
    if let Some(selection) = selection_of(source, &rows, caret) {
        terminal.set_text_selection(selection);
    }
    terminal.set_children(children);
    nodes.push((TERMINAL_ID, terminal));

    let mut root = Node::new(Role::Window);
    root.set_label(source.title);
    root.set_children(vec![TERMINAL_ID]);
    nodes.push((ROOT_ID, root));
    nodes
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use felis_grid::Grid;

    use super::*;

    #[derive(Clone, Default)]
    struct Recorder(Rc<RefCell<Vec<TreeUpdate>>>);

    impl A11yAdapter for Recorder {
        fn process_event(&mut self, _event: &WindowEvent) {}
        fn update(&mut self, update: TreeUpdate) {
            self.0.borrow_mut().push(update);
        }
    }

    fn grid_with(rows: u16, cols: u16, bytes: &[u8]) -> Grid {
        let mut g = Grid::new(rows, cols);
        let mut p = felis_vt::Parser::new();
        p.advance(&mut g, bytes);
        g
    }

    fn source(screen: &ScreenBuffer, selection: Option<Selection>) -> TreeSource<'_> {
        TreeSource {
            screen,
            viewport: 0,
            selection,
            title: "felis",
            cell_px: (10.0, 20.0),
            origin_px: (0.0, 0.0),
        }
    }

    fn values(update: &TreeUpdate) -> Vec<(u64, String)> {
        let mut out: Vec<(u64, String)> = update
            .nodes
            .iter()
            .filter(|(_, n)| n.role() == Role::TextRun)
            .map(|(id, n)| (id.0, n.value().unwrap_or_default().to_owned()))
            .collect();
        out.sort();
        out
    }

    fn terminal(update: &TreeUpdate) -> &Node {
        &update
            .nodes
            .iter()
            .find(|(id, _)| *id == TERMINAL_ID)
            .unwrap()
            .1
    }

    #[test]
    fn nothing_is_sent_before_an_assistive_technology_asks() {
        let rec = Recorder::default();
        let mut tree = A11yTree::with_adapter(Box::new(rec.clone()));
        let g = grid_with(2, 8, b"$ ls");
        assert!(!tree.mark_changed());
        tree.flush(&source(g.screen(), None));
        assert!(rec.0.borrow().is_empty());
    }

    #[test]
    fn activation_sends_the_whole_tree_then_only_the_rows_that_changed() {
        let rec = Recorder::default();
        let mut tree = A11yTree::with_adapter(Box::new(rec.clone()));
        let mut g = grid_with(3, 8, b"$ ls\r\na  b");
        tree.activate();
        tree.flush(&source(g.screen(), None));
        let first = rec.0.borrow()[0].clone();
        assert_eq!(
            values(&first),
            vec![
                (2, "$ ls\n".to_owned()),
                // The caret sits after "b", so the row is padded to reach it.
                (2 + RUNS_PER_ROW, "a  b \n".to_owned()),
                (2 + 2 * RUNS_PER_ROW, "\n".to_owned()),
            ]
        );
        assert!(first.nodes.iter().any(|(id, _)| *id == ROOT_ID));

        let mut p = felis_vt::Parser::new();
        p.advance(&mut g, b"\r\nc");
        assert!(tree.mark_changed());
        tree.flush(&source(g.screen(), None));
        let second = rec.0.borrow()[1].clone();
        assert_eq!(
            values(&second),
            vec![
                (2 + RUNS_PER_ROW, "a  b\n".to_owned()),
                (2 + 2 * RUNS_PER_ROW, "c \n".to_owned()),
            ]
        );
        // The caret moved onto the new row, so the terminal node rides along.
        assert!(second.nodes.iter().any(|(id, _)| *id == TERMINAL_ID));
        assert!(!second.nodes.iter().any(|(id, _)| *id == ROOT_ID));
    }

    #[test]
    fn an_unchanged_screen_sends_no_update() {
        let rec = Recorder::default();
        let mut tree = A11yTree::with_adapter(Box::new(rec.clone()));
        let g = grid_with(2, 8, b"$ ls");
        tree.activate();
        tree.flush(&source(g.screen(), None));
        tree.mark_changed();
        tree.flush(&source(g.screen(), None));
        assert_eq!(rec.0.borrow().len(), 1);
    }

    #[test]
    fn the_caret_sits_on_the_cursor_cell() {
        let rec = Recorder::default();
        let mut tree = A11yTree::with_adapter(Box::new(rec.clone()));
        let g = grid_with(2, 8, b"$ ");
        tree.activate();
        tree.flush(&source(g.screen(), None));
        let update = rec.0.borrow()[0].clone();
        let sel = terminal(&update).text_selection().unwrap();
        assert_eq!(sel.focus, sel.anchor);
        assert_eq!(sel.focus.node, NodeId(2));
        assert_eq!(sel.focus.character_index, 2);
        assert_eq!(values(&update)[0].1, "$  \n");
    }

    #[test]
    fn a_selection_keeps_the_direction_it_was_dragged_in() {
        let g = grid_with(2, 8, b"hello\r\nworld");
        for (from, to) in [
            (GridPos { row: 0, col: 1 }, GridPos { row: 1, col: 2 }),
            (GridPos { row: 1, col: 2 }, GridPos { row: 0, col: 1 }),
        ] {
            let rec = Recorder::default();
            let mut tree = A11yTree::with_adapter(Box::new(rec.clone()));
            let mut sel = Selection::new(from);
            sel.extend(to);
            tree.activate();
            tree.flush(&source(g.screen(), Some(sel)));
            let update = rec.0.borrow()[0].clone();
            let got = *terminal(&update).text_selection().unwrap();
            let start = TextPosition {
                node: NodeId(2),
                character_index: 1,
            };
            let end = TextPosition {
                node: NodeId(2 + RUNS_PER_ROW),
                character_index: 3,
            };
            if from.row == 0 {
                assert_eq!((got.anchor, got.focus), (start, end));
            } else {
                assert_eq!((got.anchor, got.focus), (end, start));
            }
        }
    }

    #[test]
    fn a_long_row_is_chained_runs_of_at_most_255_characters() {
        let g = grid_with(1, 600, &[b'x'; 600]);
        let nodes = build_nodes(&source(g.screen(), None));
        let runs: Vec<&Node> = nodes
            .iter()
            .filter(|(_, n)| n.role() == Role::TextRun)
            .map(|(_, n)| n)
            .collect();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].character_lengths().len(), 255);
        assert_eq!(runs[2].character_lengths().len(), 600 - 510 + 1);
        assert_eq!(runs[0].next_on_line(), Some(NodeId(3)));
        assert_eq!(runs[1].previous_on_line(), Some(NodeId(2)));
        // One word across all three runs: only the first starts it.
        assert_eq!(runs[0].word_starts(), &[0]);
        assert_eq!(runs[1].word_starts(), <&[u8]>::default());
        assert_eq!(runs[2].word_starts(), &[90]);
    }
}
