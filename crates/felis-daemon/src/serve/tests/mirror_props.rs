use super::*;

/// One step of the mirror property below: the parse, the drain that
/// parks its scroll effects, a fan-out cycle composing for some
/// subscribers, a resize, and a subscriber browsing scrollback.
#[derive(Debug, Clone)]
enum MirrorStep {
    Feed(Vec<u8>),
    Drain,
    Cycle([bool; 2]),
    Resize(u16, u16),
    Browse(usize, u32),
}

fn mirror_token() -> impl proptest::strategy::Strategy<Value = Vec<u8>> {
    use proptest::prelude::*;
    prop_oneof![
        4 => proptest::collection::vec(b'a'..=b'z', 1..6),
        3 => Just(b"\r\n".to_vec()),
        1 => Just(b"\n".to_vec()),
        1 => Just("\u{3042}".as_bytes().to_vec()),
        1 => Just("e\u{301}".as_bytes().to_vec()),
        1 => Just("\u{202E}".as_bytes().to_vec()),
        1 => Just(b"\x1bM".to_vec()),
        1 => Just(b"\x1bD".to_vec()),
        1 => (1u8..=4, prop_oneof![Just('S'), Just('T'), Just('L'), Just('M'), Just('@'), Just('P')])
            .prop_map(|(n, op)| format!("\x1b[{n}{op}").into_bytes()),
        1 => (1u8..=8, 1u8..=8).prop_map(|(t, b)| format!("\x1b[{t};{b}r").into_bytes()),
        1 => Just(b"\x1b[r".to_vec()),
        1 => (1u8..=8, 1u8..=10).prop_map(|(r, c)| format!("\x1b[{r};{c}H").into_bytes()),
        1 => (0u8..=2).prop_map(|n| format!("\x1b[{n}J").into_bytes()),
        1 => (0u8..=2).prop_map(|n| format!("\x1b[{n}K").into_bytes()),
        1 => prop_oneof![Just(0u8), Just(1), Just(7), 41u8..=43]
            .prop_map(|n| format!("\x1b[{n}m").into_bytes()),
        1 => Just(b"\x1b[?1049h".to_vec()),
        1 => Just(b"\x1b[?1049l".to_vec()),
        1 => (1u8..=5, 2u8..=10)
            .prop_map(|(l, r)| format!("\x1b[?69h\x1b[{l};{r}s").into_bytes()),
        1 => Just(b"\x1b[?69l".to_vec()),
        1 => Just(b"\x1b[?6h".to_vec()),
        1 => Just(b"\x1b[?6l".to_vec()),
        1 => Just(b"\x1b[3J".to_vec()),
        1 => Just(b"\x1b]8;;https://x\x1b\\ln\x1b]8;;\x1b\\".to_vec()),
        2 => prop_oneof![
            Just(b"\r".to_vec()),
            Just(b"\x08".to_vec()),
            Just(b"\t".to_vec()),
            Just(b"\x1b7".to_vec()),
            Just(b"\x1b8".to_vec()),
        ],
        2 => (1u8..=9, prop_oneof![Just('A'), Just('B'), Just('C'), Just('D'), Just('G'), Just('d')])
            .prop_map(|(n, op)| format!("\x1b[{n}{op}").into_bytes()),
        1 => prop_oneof![Just(41u16), Just(45), Just(1045)]
            .prop_map(|m| format!("\x1b[?{m}h").into_bytes()),
        1 => Just(b"\x1b[?25l".to_vec()),
        1 => Just(b"\x1b[?25h".to_vec()),
    ]
}

fn mirror_step() -> impl proptest::strategy::Strategy<Value = MirrorStep> {
    use proptest::prelude::*;
    prop_oneof![
        6 => proptest::collection::vec(mirror_token(), 1..12)
            .prop_map(|tokens| MirrorStep::Feed(tokens.concat())),
        3 => Just(MirrorStep::Drain),
        3 => any::<[bool; 2]>().prop_map(MirrorStep::Cycle),
        1 => (2u16..=8, 2u16..=10).prop_map(|(rows, cols)| MirrorStep::Resize(rows, cols)),
        1 => (0usize..2, prop_oneof![Just(0u32), 1u32..=6])
            .prop_map(|(sub, lines)| MirrorStep::Browse(sub, lines)),
    ]
}

/// The daemon's half of the stream (grid, parked scrolls, one
/// `SubscriberStream` per mirror) wired to real client shadows the way
/// `session_task` wires them, minus the sockets.
struct MirrorRig {
    grid: Grid,
    parser: felis_vt::Parser,
    parked: Vec<streaming::QueuedScroll>,
    subs: Vec<(SubscriberStream, felis_client_core::ShadowScreen)>,
    rows: RowEncodeCache,
}

impl MirrorRig {
    fn new(rows: u16, cols: u16) -> Self {
        let grid = Grid::new(rows, cols);
        let subs = (0..2)
            .map(|_| {
                let mut out = Vec::new();
                let stream = seeded_stream_into(&grid, &mut out);
                let mut shadow = felis_client_core::ShadowScreen::new(rows, cols);
                Self::apply(&mut shadow, &out);
                (stream, shadow)
            })
            .collect();
        Self {
            grid,
            parser: felis_vt::Parser::new(),
            parked: Vec::new(),
            subs,
            rows: RowEncodeCache::default(),
        }
    }

    /// Entries apply one at a time so each can first be checked against
    /// the released client, whose row write compared the incoming cells
    /// with the row's storage past its watermark and skipped the write
    /// on a match, leaving the row reading blank.
    fn apply(shadow: &mut felis_client_core::ShadowScreen, out: &[OutEvent]) {
        for ev in out {
            let OutEvent::Grid(msg) = ev else {
                continue;
            };
            let GridMsg::RowDelta { rows } = msg else {
                shadow.apply(msg).expect("the mirror admits every frame");
                continue;
            };
            for (row, payload) in rows {
                let screen = shadow.screen();
                let mut styles = screen.style_table().clone();
                let decoded = felis_grid::decode_row(&payload.0, &mut styles).expect("decodes");
                if screen.cols() as usize == decoded.cells.len() {
                    let storage = screen.row_cells(*row).unwrap_or(&[]);
                    let read: Vec<felis_grid::Cell> = (0..screen.cols())
                        .map(|c| screen.cell(*row, c).copied().unwrap_or_default())
                        .collect();
                    assert!(
                        storage != decoded.cells.as_slice() || read == decoded.cells,
                        "a released client would skip row {row} and read it blank",
                    );
                }
                shadow
                    .apply(&GridMsg::RowDelta {
                        rows: vec![(*row, payload.clone())],
                    })
                    .expect("the mirror admits every frame");
            }
        }
    }

    fn drain(&mut self) {
        for effect in self.grid.take_pty_effects() {
            if let PtyEffect::Scrolled {
                op,
                geometry_gen,
                first_seq,
                last_seq,
            } = effect
            {
                self.parked.push(streaming::QueuedScroll {
                    geometry_gen,
                    first_seq,
                    last_seq,
                    op,
                });
            }
        }
    }

    fn fan_out(&mut self) {
        let (current_gen, rows) = (self.grid.geometry_gen(), self.grid.rows());
        for queued in std::mem::take(&mut self.parked) {
            for (stream, _) in &mut self.subs {
                stream.offer_scroll(&queued, current_gen, rows);
            }
        }
        for (stream, _) in &mut self.subs {
            stream.catch_up_to(&self.grid);
            stream.damage.merge(self.grid.damage());
        }
        self.grid.damage_mut().clear();
    }

    fn compose(&mut self, i: usize) -> Vec<OutEvent> {
        let mut out = Vec::new();
        compose_diffs(
            &mut self.grid,
            &mut self.subs[i].0,
            &mut self.rows,
            &mut out,
        )
        .expect("compose");
        out
    }

    fn step(&mut self, step: &MirrorStep) {
        match step {
            MirrorStep::Feed(bytes) => self.parser.advance(&mut self.grid, bytes),
            MirrorStep::Drain => self.drain(),
            MirrorStep::Cycle(which) => {
                self.fan_out();
                self.rows.begin_cycle(self.subs.len());
                for (i, &compose) in which.iter().enumerate() {
                    if compose {
                        let out = self.compose(i);
                        Self::apply(&mut self.subs[i].1, &out);
                    }
                }
            }
            MirrorStep::Resize(rows, cols) => {
                if self.grid.on_alternate_screen() {
                    self.grid.resize(*rows, *cols);
                } else {
                    drop(self.grid.reflow(*rows, *cols));
                }
                for (stream, _) in &mut self.subs {
                    stream.retire_scrolls(*rows);
                }
                self.fan_out();
                self.rows.begin_cycle(self.subs.len());
                for i in 0..self.subs.len() {
                    let out = self.compose(i);
                    let shadow = &mut self.subs[i].1;
                    shadow
                        .apply(&GridMsg::Size {
                            dims: GridDims {
                                rows: *rows,
                                cols: *cols,
                                pixel_w: 0,
                                pixel_h: 0,
                            },
                        })
                        .expect("the mirror admits the resize");
                    Self::apply(shadow, &out);
                }
            }
            MirrorStep::Browse(i, lines) => {
                let viewport = self.grid.clamp_viewport(*lines);
                let stream = &mut self.subs[*i].0;
                if viewport != stream.diff.viewport {
                    stream.diff.viewport = viewport;
                    stream.damage.mark_all();
                }
            }
        }
    }

    /// What a mirror shows besides its cells: the cursor and each row's
    /// soft-wrap bit.
    fn overlay_of(screen: &felis_grid::ScreenBuffer) -> ((u16, u16, bool), Vec<bool>) {
        let cursor = screen.cursor();
        let wraps = (0..screen.rows())
            .map(|r| screen.row_soft_wrap_continued(r))
            .collect();
        ((cursor.row, cursor.col, cursor.visible), wraps)
    }

    /// Every visible cell as a mirror can compare it: the grapheme's
    /// text, the resolved attributes (style ids are per-side), the link.
    fn cells_of(
        screen: &felis_grid::ScreenBuffer,
    ) -> Vec<Vec<(String, felis_grid::Attributes, Option<std::num::NonZeroU16>)>> {
        (0..screen.rows())
            .map(|r| {
                (0..screen.cols())
                    .map(|c| {
                        let cell = screen.cell(r, c).copied().unwrap_or_default();
                        let mut text = String::new();
                        felis_grid::push_cell_text(&cell, screen.cluster_table(), &mut text);
                        (text, *screen.style(cell.style), cell.link)
                    })
                    .collect()
            })
            .collect()
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

    /// Whatever the parse, the drain cadence, the resizes and each
    /// subscriber's pull cadence, a mirror that composes at the live
    /// view holds exactly the grid's cells afterwards.
    #[test]
    fn every_mirror_ends_each_compose_holding_the_grids_cells(
        rows in 2u16..=8,
        cols in 2u16..=10,
        steps in proptest::collection::vec(mirror_step(), 1..40),
    ) {
        let mut rig = MirrorRig::new(rows, cols);
        for (n, step) in steps.iter().enumerate() {
            rig.step(step);
            if matches!(step, MirrorStep::Cycle(_) | MirrorStep::Resize(..)) {
                let composed: Vec<bool> = match step {
                    MirrorStep::Cycle(which) => which.to_vec(),
                    _ => vec![true; rig.subs.len()],
                };
                let want = MirrorRig::cells_of(rig.grid.screen());
                let want_overlay = MirrorRig::overlay_of(rig.grid.screen());
                for (i, (stream, shadow)) in rig.subs.iter().enumerate() {
                    if composed[i] && stream.diff.viewport == 0 {
                        proptest::prop_assert_eq!(
                            MirrorRig::cells_of(shadow.screen()),
                            want.clone(),
                            "mirror {} after step {} ({:?})", i, n, step
                        );
                        proptest::prop_assert_eq!(
                            MirrorRig::overlay_of(shadow.screen()),
                            want_overlay.clone(),
                            "mirror {} cursor or soft wraps after step {} ({:?})", i, n, step
                        );
                    }
                }
            }
        }
        rig.step(&MirrorStep::Browse(0, 0));
        rig.step(&MirrorStep::Browse(1, 0));
        rig.step(&MirrorStep::Drain);
        rig.step(&MirrorStep::Cycle([true, true]));
        let want = MirrorRig::cells_of(rig.grid.screen());
        let want_overlay = MirrorRig::overlay_of(rig.grid.screen());
        for (i, (_, shadow)) in rig.subs.iter().enumerate() {
            proptest::prop_assert_eq!(
                MirrorRig::cells_of(shadow.screen()),
                want.clone(),
                "mirror {} after the final cycle", i
            );
            proptest::prop_assert_eq!(
                MirrorRig::overlay_of(shadow.screen()),
                want_overlay.clone(),
                "mirror {} cursor or soft wraps after the final cycle", i
            );
        }
    }
}
