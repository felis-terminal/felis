use super::*;

#[test]
fn osc_22_sets_pointer_shape_and_flags_dirty() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]22;pointer\x07");
    assert_eq!(g.pointer_shape(), Some("pointer"));
    assert_eq!(
        g.take_pointer_shape_dirty(),
        Some(Some("pointer".to_owned()))
    );
    assert_eq!(g.take_pointer_shape_dirty(), None, "dirty clears on take");
}

#[test]
fn osc_22_empty_body_resets_to_default_arrow() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]22;text\x07");
    assert_eq!(g.pointer_shape(), Some("text"));
    assert_eq!(g.take_pointer_shape_dirty(), Some(Some("text".to_owned())));
    drive(&mut p, &mut g, b"\x1b]22;\x07");
    assert_eq!(g.pointer_shape(), None, "empty body resets to default");
    assert_eq!(g.take_pointer_shape_dirty(), Some(None), "reset is shipped");
}

#[test]
fn osc_22_rejects_invalid_keyword_keeping_current_shape() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]22;crosshair\x07");
    assert_eq!(
        g.take_pointer_shape_dirty(),
        Some(Some("crosshair".to_owned()))
    );
    // Junk must not clobber the valid earlier request.
    drive(&mut p, &mut g, b"\x1b]22;Bogus Shape!\x07");
    assert_eq!(g.pointer_shape(), Some("crosshair"));
    assert_eq!(g.take_pointer_shape_dirty(), None, "invalid → no change");
}

#[test]
fn osc_22_push_and_pop_restore_the_prior_shape() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]22;text\x07");
    drive(&mut p, &mut g, b"\x1b]22;>wait\x07");
    assert_eq!(
        g.pointer_shape(),
        Some("wait"),
        "push makes the new shape live"
    );
    drive(&mut p, &mut g, b"\x1b]22;<\x07");
    assert_eq!(
        g.pointer_shape(),
        Some("text"),
        "pop restores the pushed shape"
    );
}

#[test]
fn osc_22_pop_on_empty_stack_resets_to_default() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]22;text\x07");
    // kitty's behavior: pop on an empty stack resets to the default.
    drive(&mut p, &mut g, b"\x1b]22;<\x07");
    assert_eq!(g.pointer_shape(), None, "pop with an empty stack resets");
}

#[test]
fn osc_22_takes_the_first_valid_shape_of_a_fallback_list() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    // A comma list is kitty's fallback form; the client resolves
    // recognition.
    drive(&mut p, &mut g, b"\x1b]22;grab,progress\x07");
    assert_eq!(g.pointer_shape(), Some("grab"));
    drive(&mut p, &mut g, b"\x1b]22;BAD!,pointer\x07");
    assert_eq!(g.pointer_shape(), Some("pointer"));
}

#[test]
fn ris_ships_the_pointer_shape_reset_and_empties_the_stack() {
    let mut p = Parser::new();
    let mut g = Grid::new(1, 4);
    drive(&mut p, &mut g, b"\x1b]22;text\x07\x1b]22;>wait\x07");
    assert_eq!(g.take_pointer_shape_dirty(), Some(Some("wait".to_owned())));
    drive(&mut p, &mut g, b"\x1bc");
    assert_eq!(g.pointer_shape(), None);
    assert_eq!(g.take_pointer_shape_dirty(), Some(None), "reset is shipped");
    drive(&mut p, &mut g, b"\x1b]22;<\x07");
    assert_eq!(g.pointer_shape(), None, "the pushed shape is gone");
}
