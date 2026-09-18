#!/usr/bin/env python3
"""Generate the HID-usage to Linux-keycode table.

HID usage IDs are stable and defined by the USB HID Usage Tables; Linux
keycodes are defined by <linux/input-event-codes.h>. Rather than transcribe a
table of numbers -- where a single wrong digit produces a key that silently
types the wrong thing -- this maps usages to KEY_ *names* and resolves the
numbers from the header, failing loudly on any name it cannot find.

    python3 tools/gen_keymap.py > src/keymap/table.rs
"""
import re
import sys

HEADER = "/usr/include/linux/input-event-codes.h"

# HID Usage Page 0x07 (Keyboard/Keypad) -> Linux KEY_ name.
USAGES = {}


def span(start, names):
    for i, name in enumerate(names):
        USAGES[start + i] = name


span(0x04, [f"KEY_{c}" for c in "ABCDEFGHIJKLMNOPQRSTUVWXYZ"])
span(0x1E, ["KEY_1", "KEY_2", "KEY_3", "KEY_4", "KEY_5",
            "KEY_6", "KEY_7", "KEY_8", "KEY_9", "KEY_0"])
USAGES.update({
    0x28: "KEY_ENTER",      0x29: "KEY_ESC",        0x2A: "KEY_BACKSPACE",
    0x2B: "KEY_TAB",        0x2C: "KEY_SPACE",      0x2D: "KEY_MINUS",
    0x2E: "KEY_EQUAL",      0x2F: "KEY_LEFTBRACE",  0x30: "KEY_RIGHTBRACE",
    0x31: "KEY_BACKSLASH",
    # "Non-US # and ~" sits where backslash does on an ANSI board.
    0x32: "KEY_BACKSLASH",
    0x33: "KEY_SEMICOLON",  0x34: "KEY_APOSTROPHE", 0x35: "KEY_GRAVE",
    0x36: "KEY_COMMA",      0x37: "KEY_DOT",        0x38: "KEY_SLASH",
    0x39: "KEY_CAPSLOCK",
})
span(0x3A, [f"KEY_F{n}" for n in range(1, 13)])
USAGES.update({
    0x46: "KEY_SYSRQ",      0x47: "KEY_SCROLLLOCK", 0x48: "KEY_PAUSE",
    0x49: "KEY_INSERT",     0x4A: "KEY_HOME",       0x4B: "KEY_PAGEUP",
    0x4C: "KEY_DELETE",     0x4D: "KEY_END",        0x4E: "KEY_PAGEDOWN",
    0x4F: "KEY_RIGHT",      0x50: "KEY_LEFT",       0x51: "KEY_DOWN",
    0x52: "KEY_UP",         0x53: "KEY_NUMLOCK",    0x54: "KEY_KPSLASH",
    0x55: "KEY_KPASTERISK", 0x56: "KEY_KPMINUS",    0x57: "KEY_KPPLUS",
    0x58: "KEY_KPENTER",
})
span(0x59, [f"KEY_KP{n}" for n in range(1, 10)])
USAGES.update({
    0x62: "KEY_KP0",        0x63: "KEY_KPDOT",      0x64: "KEY_102ND",
    0x65: "KEY_COMPOSE",    0x66: "KEY_POWER",      0x67: "KEY_KPEQUAL",
})
span(0x68, [f"KEY_F{n}" for n in range(13, 25)])
USAGES.update({
    0x74: "KEY_OPEN",       0x75: "KEY_HELP",       0x76: "KEY_PROPS",
    0x77: "KEY_FRONT",      0x78: "KEY_STOP",       0x79: "KEY_AGAIN",
    0x7A: "KEY_UNDO",       0x7B: "KEY_CUT",        0x7C: "KEY_COPY",
    0x7D: "KEY_PASTE",      0x7E: "KEY_FIND",       0x7F: "KEY_MUTE",
    0x80: "KEY_VOLUMEUP",   0x81: "KEY_VOLUMEDOWN", 0x85: "KEY_KPCOMMA",
    # International and language keys. Present because they are part of the
    # standard table, not because anything here interprets them.
    0x87: "KEY_RO",         0x88: "KEY_KATAKANAHIRAGANA",
    0x89: "KEY_YEN",        0x8A: "KEY_HENKAN",     0x8B: "KEY_MUHENKAN",
    0x8C: "KEY_KPJPCOMMA",  0x90: "KEY_HANGEUL",    0x91: "KEY_HANJA",
    0x92: "KEY_KATAKANA",   0x93: "KEY_HIRAGANA",   0x94: "KEY_ZENKAKUHANKAKU",
    0xE0: "KEY_LEFTCTRL",   0xE1: "KEY_LEFTSHIFT",  0xE2: "KEY_LEFTALT",
    0xE3: "KEY_LEFTMETA",   0xE4: "KEY_RIGHTCTRL",  0xE5: "KEY_RIGHTSHIFT",
    0xE6: "KEY_RIGHTALT",   0xE7: "KEY_RIGHTMETA",
})


def load_header(path):
    codes = {}
    pattern = re.compile(r"^#define\s+(KEY_\w+)\s+(\S+)")
    for line in open(path, encoding="utf-8"):
        m = pattern.match(line)
        if not m:
            continue
        name, value = m.group(1), m.group(2)
        try:
            codes[name] = int(value, 0)
        except ValueError:
            # Some entries alias another name.
            if value in codes:
                codes[name] = codes[value]
    return codes


# Linux keycode -> PS/2 scan code set 1, for keys whose value differs.
#
# For the main block the two numbering schemes agree: the kernel's AT keyboard
# driver maps set-1 codes 0x01..0x58 straight through, so KEY_A is 30 and the
# scan code is 0x1E. Only the keys that arrived later, and are sent with an
# 0xE0 prefix, need stating.
#
# A key that is not listed and not in that range gets no mapping at all rather
# than a guess. An unmapped key refuses to be pressed, which is a visible
# failure; a wrong scan code silently types something else.
EXTENDED = {
    "KEY_KPENTER":    0xE01C,
    "KEY_RIGHTCTRL":  0xE01D,
    "KEY_KPSLASH":    0xE035,
    "KEY_SYSRQ":      0xE037,
    "KEY_RIGHTALT":   0xE038,
    "KEY_HOME":       0xE047,
    "KEY_UP":         0xE048,
    "KEY_PAGEUP":     0xE049,
    "KEY_LEFT":       0xE04B,
    "KEY_RIGHT":      0xE04D,
    "KEY_END":        0xE04F,
    "KEY_DOWN":       0xE050,
    "KEY_PAGEDOWN":   0xE051,
    "KEY_INSERT":     0xE052,
    "KEY_DELETE":     0xE053,
    "KEY_MUTE":       0xE020,
    "KEY_VOLUMEDOWN": 0xE02E,
    "KEY_VOLUMEUP":   0xE030,
    "KEY_POWER":      0xE05E,
    "KEY_LEFTMETA":   0xE05B,
    "KEY_RIGHTMETA":  0xE05C,
    "KEY_COMPOSE":    0xE05D,
    "KEY_KPEQUAL":    0x0059,
}

# The top of the range the two schemes share.
SHARED_MAX = 0x58


def scan_code(name, code):
    """The set-1 scan code for a Linux keycode, or None if not established."""
    if name in EXTENDED:
        return EXTENDED[name]
    if 1 <= code <= SHARED_MAX:
        return code
    return None


def main():
    codes = load_header(HEADER)
    missing = sorted({n for n in USAGES.values() if n not in codes})
    if missing:
        sys.exit(f"{HEADER} defines no: {', '.join(missing)}")

    rows = sorted((u, USAGES[u], codes[USAGES[u]]) for u in USAGES)
    print("//! HID usage to Linux keycode, generated by tools/gen_keymap.py.")
    print("//!")
    print("//! Do not edit by hand: regenerate from <linux/input-event-codes.h>")
    print("//! so the numbers always come from the kernel's own definitions.")
    print()
    print("/// `(HID usage, Linux keycode)`, sorted by usage.")
    print(f"pub const HID_TO_EVDEV: [(u16, u16); {len(rows)}] = [")
    for usage, name, code in rows:
        print(f"    (0x{usage:04X}, {code}), // {name}")
    print("];")
    print()

    scans = [(u, n, scan_code(n, c)) for u, n, c in rows]
    known = [(u, n, s) for u, n, s in scans if s is not None]
    print("/// `(HID usage, PS/2 set 1 scan code)`, sorted by usage.")
    print("///")
    print("/// A code of `0xE0xx` is sent with the extended prefix. Usages")
    print("/// absent from this table have no established scan code and are")
    print("/// refused rather than guessed at.")
    print(f"pub const HID_TO_SCANCODE: [(u16, u16); {len(known)}] = [")
    for usage, name, code in known:
        print(f"    (0x{usage:04X}, 0x{code:04X}), // {name}")
    print("];")

    missing = [n for _, n, s in scans if s is None]
    if missing:
        print()
        print("// Deliberately unmapped, for want of an established scan code:")
        for name in missing:
            print(f"//   {name}")


if __name__ == "__main__":
    main()
