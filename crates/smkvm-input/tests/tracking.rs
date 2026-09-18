//! Keeping track of what is held down.
//!
//! A key that stays pressed on a machine you have walked away from is one of
//! the most irritating things a KVM can do, and it happens whenever the switch
//! lands between a press and its release. The fix is not to be clever about
//! timing but to know, exactly, what has been pressed and to let go of it.

use smkvm_input::platform::loopback::{Event, Loopback};
use smkvm_input::Tracked;
use smkvm_proto::{Key, MouseButton, Scroll};

const A: Key = Key(0x04);
const C: Key = Key(0x06);
const TAB: Key = Key(0x2B);

fn tracked() -> Tracked<Loopback> {
    Tracked::new(Loopback::new())
}

#[test]
fn what_is_held_is_remembered() {
    let mut t = tracked();
    t.key(Key::LEFT_CTRL, true).unwrap();
    t.key(C, true).unwrap();
    t.button(MouseButton::Left, true).unwrap();

    assert_eq!(t.held_keys(), vec![C, Key::LEFT_CTRL]);
    assert_eq!(t.held_buttons(), vec![MouseButton::Left]);
    assert!(t.is_holding_anything());

    t.key(C, false).unwrap();
    assert_eq!(t.held_keys(), vec![Key::LEFT_CTRL]);
}

#[test]
fn releasing_everything_lets_go_of_keys_before_modifiers() {
    let mut t = tracked();
    t.key(Key::LEFT_ALT, true).unwrap();
    t.key(TAB, true).unwrap();
    t.key(Key::LEFT_SHIFT, true).unwrap();
    t.inner_mut().clear();

    t.release_all().unwrap();
    assert!(!t.is_holding_anything());

    // Ordinary keys go first: releasing Alt before Tab would briefly present
    // an unmodified Tab, which some applications act on. Within each group the
    // order follows the usage id, which is simply what the held set iterates.
    let keys: Vec<Key> = t
        .inner()
        .actions()
        .iter()
        .filter_map(|e| match e {
            Event::Key { key, down: false } => Some(*key),
            _ => None,
        })
        .collect();
    assert_eq!(keys, vec![TAB, Key::LEFT_SHIFT, Key::LEFT_ALT]);
}

#[test]
fn a_failing_key_does_not_strand_the_rest() {
    // The whole point of releasing everything is that nothing is left held.
    // Giving up on the first error would do exactly what it exists to prevent.
    let mut t = tracked();
    t.key(Key::LEFT_ALT, true).unwrap();
    t.key(Key::LEFT_CTRL, true).unwrap();
    t.key(TAB, true).unwrap();
    t.button(MouseButton::Right, true).unwrap();
    t.inner_mut().clear();

    // The display server starts refusing this key only now, after it is held:
    // a release that fails is the case that strands a key.
    t.inner_mut().fail_key(TAB);

    let err = t.release_all();
    assert!(err.is_err(), "the failure is reported, not swallowed");
    assert!(
        !t.is_holding_anything(),
        "nothing may still be considered held after releasing everything"
    );

    let released: Vec<Key> = t
        .inner()
        .actions()
        .iter()
        .filter_map(|e| match e {
            Event::Key { key, down: false } => Some(*key),
            _ => None,
        })
        .collect();
    assert!(
        released.contains(&Key::LEFT_ALT) && released.contains(&Key::LEFT_CTRL),
        "the keys after the failing one were left held: {released:?}"
    );
    assert!(
        t.inner()
            .actions()
            .iter()
            .any(|e| matches!(e, Event::Button { down: false, .. })),
        "the button was left held"
    );
}

#[test]
fn arriving_adopts_the_state_from_the_machine_being_left() {
    let mut t = tracked();
    // Left over from an earlier visit.
    t.key(Key::LEFT_SHIFT, true).unwrap();
    t.key(A, true).unwrap();
    t.inner_mut().clear();

    // The cursor arrives mid-chord: Ctrl and C are down elsewhere.
    t.sync(&[Key::LEFT_CTRL, C], &[]).unwrap();

    assert_eq!(t.held_keys(), vec![C, Key::LEFT_CTRL]);
    let actions = t.inner().actions();

    // Everything stale is released before anything new is pressed, so no key
    // is ever momentarily held twice.
    let first_press = actions
        .iter()
        .position(|e| matches!(e, Event::Key { down: true, .. }))
        .expect("something is pressed");
    let last_release = actions
        .iter()
        .rposition(|e| matches!(e, Event::Key { down: false, .. }))
        .expect("something is released");
    assert!(last_release < first_press, "{actions:?}");

    // And modifiers lead the presses, so the chord arrives already modified.
    let presses: Vec<Key> = actions
        .iter()
        .filter_map(|e| match e {
            Event::Key { key, down: true } => Some(*key),
            _ => None,
        })
        .collect();
    assert_eq!(presses, vec![Key::LEFT_CTRL, C]);
}

#[test]
fn syncing_to_what_is_already_held_changes_nothing() {
    let mut t = tracked();
    t.key(Key::LEFT_CTRL, true).unwrap();
    t.key(C, true).unwrap();
    t.inner_mut().clear();

    t.sync(&[C, Key::LEFT_CTRL], &[]).unwrap();
    assert!(
        t.inner()
            .actions()
            .iter()
            .all(|e| !matches!(e, Event::Key { .. })),
        "a key was pressed or released needlessly: {:?}",
        t.inner().actions()
    );
}

#[test]
fn buttons_are_tracked_and_released_too() {
    let mut t = tracked();
    t.button(MouseButton::Left, true).unwrap();
    t.button(MouseButton::Other(9), true).unwrap();
    t.release_all().unwrap();

    assert!(t.held_buttons().is_empty());
    let ups: Vec<MouseButton> = t
        .inner()
        .actions()
        .iter()
        .filter_map(|e| match e {
            Event::Button {
                button,
                down: false,
            } => Some(*button),
            _ => None,
        })
        .collect();
    assert_eq!(ups.len(), 2);
}

#[test]
fn pointer_motion_and_scrolling_pass_straight_through() {
    let mut t = tracked();
    t.move_to(1234, 567).unwrap();
    t.wheel(Scroll::new(0, -Scroll::NOTCH)).unwrap();

    assert_eq!(
        t.inner().actions(),
        vec![
            Event::MoveTo { x: 1234, y: 567 },
            Event::Wheel {
                dx: 0,
                dy: -Scroll::NOTCH
            },
        ]
    );
}
