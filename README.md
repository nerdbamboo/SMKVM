# SMKVM

**English** · [한국어](README.ko.md) · [日本語](README.ja.md)

One keyboard and mouse across several computers. Move the pointer off the edge
of one screen and it appears on the next, whichever machine that screen belongs
to; what you copy on one machine pastes on another.

It is a replacement for Barrier and Synergy, written because those had stopped
being maintained and because the way they were built was the cause of most of
the trouble with them: an optional TLS layer, fingerprints accepted on first
sight, a clipboard that only travelled when the cursor did, and a size limit
past which sharing silently stopped.

## What it does

- **Any arrangement of screens.** Every monitor of every machine has a place on
  one big desktop, arranged in a window by dragging. A machine with two screens
  is two screens, not one rectangle.
- **Paired, then encrypted, always.** Two machines are introduced once, by
  comparing a six-digit code shown on both. After that every connection is a
  Noise handshake between the two keys; a machine that is not paired never
  gets a reply, let alone a session.
- **Nothing sticks.** Every key and button held is tracked, and let go of the
  moment a machine stops being in charge -- on crossing, on a dropped link, on
  a UAC prompt taking the screen.
- **The clipboard follows the copy, not the cursor.** A copy announces what is
  available; contents are fetched only when something pastes, one chunk at a
  time, so a screenshot that is never pasted costs one small message and
  there is no size at which sharing quietly fails.
- **Files go the same way, and by dragging.** Copy files in Explorer or
  Nautilus and paste them on another machine, or drag them off the edge of one
  screen and let go on another: either way they land in `~/Downloads/SMKVM`
  there, fetched through the same link, with a limit on how much one transfer
  may pull. On Windows a drop also lands where the pointer is; everywhere, the
  landed files are on the clipboard, so a paste places them.
- **It says what it is doing.** A status file, a `smkvm status` command and
  the window all show which machines are connected, where their screens are,
  and which one has the cursor. The configuration is picked up when it changes,
  so arranging screens is felt at once.

Windows and Linux (X11) today. The machine that owns the keyboard and mouse
has to be Windows for now; Linux machines receive.

## Getting started

Build once (Rust 1.82 or later):

```
cargo build --release -p smkvm-cli -p smkvm-gui
```

For a Windows binary from Linux, `cargo xwin build --release --target
x86_64-pc-windows-msvc -p smkvm-cli -p smkvm-gui`.

On the machine that owns the keyboard and mouse:

```
smkvm init --role server --listen 192.168.1.10
smkvm pair              # wait for the first client
smkvm run
```

On each other machine:

```
smkvm init --role client --server 192.168.1.10
smkvm pair 192.168.1.10
smkvm run
```

`pair` shows the same six digits on both machines; accept only if they match.
Then open `smkvm-gui` on any machine and drag the screens into the arrangement
on your desk. The daemon picks the change up within a couple of seconds.

`smkvm status` says what is connected and where things are. `smkvm --help`
lists the rest.

To have it start whenever you log in, on each machine:

```
smkvm service install
```

On Windows run that from an administrator prompt: the task it registers runs
in your desktop session with highest privileges, which is what lets it type
into windows that run as administrator, and it starts without a console
window. `--user` names the account that sits at the desk when it is not the
one registering. `smkvm service start|stop|status` do what they say.

The log's first lines say whether input from this machine will reach every
window. On Windows, one that is not elevated works everywhere except a
window running as administrator, where the system refuses silently, so it is
worth reading once after installing.

## Where things live

| | Linux | Windows |
|---|---|---|
| configuration | `~/.config/smkvm/smkvm.toml` | `%APPDATA%\smkvm\smkvm.toml` |
| identity, paired machines, status, log | `~/.local/share/smkvm/` | `%LOCALAPPDATA%\smkvm\` |

The configuration is TOML and meant to be edited; the window rewrites only
the keys it owns and leaves your comments alone.

## The pieces

| crate | what it is |
|---|---|
| `smkvm-layout` | the global desktop: where every monitor sits and what is beyond each edge |
| `smkvm-proto` | the wire format |
| `smkvm-net` | identity, pairing, the encrypted link |
| `smkvm-input` | capturing and injecting input, per platform |
| `smkvm-clipboard` | watching, reading and offering the clipboard, per platform |
| `smkvm-core` | the state machines: where the cursor is, what is held, what the clipboard is offering |
| `smkvm-config` | the configuration file, the status file, and migration from Barrier |
| `smkvm-cli` | the `smkvm` daemon and command |
| `smkvm-gui` | the window |

The state machines in `smkvm-core` know nothing of sockets, displays or
clocks: events go in with a time, actions come out. That is what lets a
modifier held across a crossing, a machine vanishing mid-paste, or a screen
edge with nothing usable beyond it be tested exactly, on any machine, with no
hardware.

## Working on it

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo test --workspace
```

Both targets, always: most of the platform code exists only on one of them.
The X11 tests start a private `Xvfb` when one is installed and skip otherwise.

The design reasoning is in the commit messages; `git log` reads as the record
of why things are the way they are. `docs/NOTES.md` holds what is not in the
code.

GPL-2.0-or-later.
