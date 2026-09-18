//! Taking the pointer out of the way, and giving it back.
//!
//! Some platforms can hide the pointer outright; Windows cannot, and can only
//! move it somewhere harmless. That leaves a debt -- the position it was taken
//! from -- and nothing else in the system will settle it. No message arrives
//! after a link drops, so a backend that parks the pointer and forgets where it
//! was leaves the person looking at a screen with the pointer in a corner and
//! no way to ask for it back.
//!
//! The recording backend stands in for that kind of platform, so the rule can
//! be exercised here rather than only on the machine it goes wrong on.

use smkvm_input::platform::loopback::{Event, Loopback};
use smkvm_input::{Inject as _, Parked};

/// The far corner of [`Loopback::single_screen`], which is where a parked
/// pointer goes.
const CORNER: (i32, i32) = (1919, 1079);

fn moves(backend: &Loopback) -> Vec<(i32, i32)> {
    backend
        .events()
        .iter()
        .filter_map(|e| match e {
            Event::MoveTo { x, y } => Some((*x, *y)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_parked_pointer_is_put_back_where_it_came_from() {
    let mut backend = Loopback::single_screen();
    backend.move_to(400, 300).unwrap();
    backend.clear();

    backend.hide_cursor().unwrap();
    assert_eq!(backend.pointer(), CORNER, "parking puts it out of the way");

    backend.show_cursor().unwrap();
    assert_eq!(
        backend.pointer(),
        (400, 300),
        "and the position it was taken from comes back"
    );
    assert_eq!(moves(&backend), vec![CORNER, (400, 300)]);
}

#[test]
fn parking_a_pointer_that_is_already_parked_does_not_lose_where_it_was() {
    // Two `Leave`s with no `Enter` between them is not a strange case: the
    // server sends one on every crossing away, and a machine that reconnects
    // can be told twice. Taking the corner as the position to return to would
    // turn a recoverable pointer into a permanent one.
    let mut backend = Loopback::single_screen();
    backend.move_to(400, 300).unwrap();

    backend.hide_cursor().unwrap();
    backend.hide_cursor().unwrap();
    backend.show_cursor().unwrap();

    assert_eq!(backend.pointer(), (400, 300));
}

#[test]
fn being_told_where_the_pointer_goes_settles_the_debt() {
    // Arriving places the pointer and then asks for it back, in that order.
    // Honouring the debt at that point would drag the pointer away from where
    // the cursor actually arrived.
    let mut backend = Loopback::single_screen();
    backend.move_to(400, 300).unwrap();
    backend.hide_cursor().unwrap();

    backend.move_to(50, 60).unwrap();
    backend.show_cursor().unwrap();

    assert_eq!(backend.pointer(), (50, 60));
    assert!(!backend.is_parked());
}

#[test]
fn a_pointer_that_was_never_parked_is_left_alone() {
    let mut backend = Loopback::single_screen();
    backend.move_to(400, 300).unwrap();
    backend.clear();

    backend.show_cursor().unwrap();

    assert_eq!(backend.pointer(), (400, 300));
    assert!(moves(&backend).is_empty(), "nothing needed moving");
}

#[test]
fn a_backend_with_nowhere_to_park_is_not_an_error() {
    // A machine that has reported no displays yet still gets told the cursor
    // has left. There is nowhere to put the pointer, which is not a failure.
    let mut backend = Loopback::new();
    backend.hide_cursor().unwrap();
    backend.show_cursor().unwrap();
    assert!(!backend.is_parked());
}

#[test]
fn the_parking_record_admits_one_debt_at_a_time() {
    let mut parked = Parked::default();
    assert!(
        parked.park((10, 20)),
        "the first park is the one that moves"
    );
    assert!(!parked.park((99, 99)), "the second has nothing to do");
    assert!(parked.is_parked());

    assert_eq!(parked.restore(), Some((10, 20)));
    assert_eq!(parked.restore(), None, "settled once, settled for good");
    assert!(!parked.is_parked());
}

#[test]
fn a_deliberate_placement_cancels_the_parking_record() {
    let mut parked = Parked::default();
    parked.park((10, 20));
    parked.placed();
    assert_eq!(parked.restore(), None);
}
