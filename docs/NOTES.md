# Notes for whoever works on this next

What a new session needs that is not already written down. The design reasoning
is in the commit messages — `git log` reads as the record of why things are the
way they are, and is worth reading before changing any of it. This file covers
only the things that live outside the code: what is still broken, and the traps
that have already cost time.

The particulars of one installation — which machines, which accounts, which
addresses — are deliberately not here. Keep them in `docs/local/`, which is
ignored by git.

## Where it stands

Working, on real hardware: pairing, the encrypted link, input capture and
injection, a four-monitor arrangement across three machines, reconnection. The
cursor crosses between all four screens.

Written and tested without hardware, **not yet run on real machines**
(everything from `feat/clipboard-status` onwards):

- The clipboard travels over the link. The exchange is a state machine in
  `smkvm-core::exchange` with the server as hub; the daemon glue is
  `smkvm-cli/src/clipboard.rs`. Text, HTML, PNG and file lists, fetched one
  128 KiB chunk at a time only when something pastes. The X11 side has an
  Xvfb test end to end; the Windows side compiles and its pieces are the
  ones that were already tested, but the owner-and-render path has not been
  exercised on a Windows machine.
- The daemon writes `status.toml` (both roles), with every machine's screens
  where the daemon has them and which machine has the cursor. The GUI draws
  machines the configuration does not place, dashed, and dragging one pins
  it. `smkvm status` prints the report.
- The server re-reads `smkvm.toml` when it changes, so arranging screens in
  the GUI is felt within two seconds. Network and name changes still need a
  restart, and the log says so.
- Heartbeats both ways (`network.heartbeat_ms`, three misses), the same
  machine connecting again replaces its old session, and each side states
  its protocol version first (`PROTO_VERSION` is now 2). **Every machine
  must be updated together**: an old build is refused with a message naming
  the version.
- `smkvm init`, `smkvm run`, `smkvm status`; a second daemon refuses to start
  beside a live report; the log rolls over at 4 MiB.

Written and tested without hardware since the last run on the real machines
(everything from `feat/drag-drop` onwards):

- **Dragging files between screens.** The cursor leaving with the button
  held is looked into: a window of ours is stood under the pointer, the
  pointer nudged so the dragging application notices it, the file list read
  from what it offers, and the button released on its behalf so its drag
  ends there with nothing moved. On Windows that is an OLE drop target
  (`smkvm-clipboard::platform::windows_drag`); on X11 an XDND target
  (`DndCatcher` in `platform/x11.rs`, with an Xvfb test that plays the
  dragging application). The files are then offered exactly as a copy would
  be, with the manifest kept by the offerer since nothing put it on a
  clipboard, and a `Bulk::Dragging` notice goes through the server to the
  machine the cursor arrived on, which pulls the files at once into
  `transfer.directory`, puts their list on its clipboard, and on Windows
  starts a native drag (`DoDragDrop` with a shell data object) so letting
  go drops them where the pointer is. `PROTO_VERSION` is 3.
- **A refused injection is reported and acted on.** When Windows will not
  take injected input -- a window running as administrator in front, or the
  secure desktop -- the client now notices (every refusal, and a look twice
  a second), says which program is in the way and what to do, and suspends
  so the server takes the cursor home; it resumes the moment a probe
  injection lands again. `SuspendReason::Elevated` names the case.
- **`smkvm service install|uninstall|start|stop|status`.** Windows: a
  scheduled task named `SMKVM` in the desktop session, "run with highest
  privileges", `smkvm run` at logon, restarted on failure; registering it
  needs an administrator prompt, and `--user` names the account sitting at
  the desk when it is not the one registering. The principal is registered
  by SID, looked up in the profile list when the name cannot be resolved (a
  cloud or domain account with no line to its directory fails the name
  lookup with 0x80070534 although it logs in fine). The action runs through
  `conhost.exe --headless`, so nothing appears at logon: the console window
  the first version put up got closed by the person, which killed the
  daemon (the task's last result reads 0xC000013A). The action says `run
  --unattended`: a task's process has a console, headless or not, and stderr
  looks like a terminal from inside it, so without the flag the log went to
  a console nobody could see while the file stopped at the last hand-started
  run. Linux: a login item in
  `~/.config/autostart/`, since a systemd user service does not reliably see
  the display. `start` and `stop` go by the status report's pid only when
  that process is still alive, so stopping and starting again within the
  report's 15 s does not get refused; `stop` also ends a daemon that was
  started by hand. `stop` on a server ends the hooks and gives the keyboard
  back, so it is the recovery command too.
- **A service as the system account, behind `smkvm service install
  --system`.** The one arrangement that reaches the desktop a UAC prompt or
  the lock screen is on. See the section below; **none of it has run on a
  Windows machine.**
- **The daemon says at startup how far its input reaches.** On Windows a
  daemon at an ordinary integrity level works everywhere except a window
  running as administrator, where `SendInput` returns zero and the capture
  hook stops being called, both without a word from the system. The first
  lines of the log now say which of the two it is, so the limit is readable
  on the day of installation rather than the first time a PowerShell is
  clicked. `platform::report_rank` says it; `privilege::our_level` reads it.

## Running on Windows

**Anything started over ssh lands in session 0 and cannot see or touch the real
desktop.** It will appear to run and do nothing. The daemon has to be started
in the interactive session — a scheduled task with an interactive principal, or
a shortcut the person runs. Session 0 is also why `smkvm monitors` over ssh
reports a display that is not the one on the desk.

A task has no console, so a failure reported only by returning it goes nowhere.
This is why every error is also logged, and why the log lives in
`%LOCALAPPDATA%\smkvm\smkvm.log` rather than on stderr.

Build for Windows from Linux:

```
cargo xwin build --release --target x86_64-pc-windows-msvc -p smkvm-cli -p smkvm-gui
```

**The server swallows the keyboard and mouse while the cursor is on another
machine.** A fault there can leave the person unable to use their own computer.
Stopping the server process always recovers it, because the hooks go when the
process does. Keep a way to do that from another machine within reach, and say
so before doing anything that might strand them.

**A pointer another program has hidden or caged is that program's.** The
invisible pointer that cost the most time was a leftover Barrier client, still
retrying a server that had stopped answering, holding the pointer confined
(`ClipCursor`) to one pixel — the same corner SMKVM parks at, which is why it
looked like ours. `GetClipCursor` proved it; SMKVM calls `ClipCursor` nowhere,
and `ShowCursor(TRUE)` cannot undo another process's hide (measured; see the
comment in `smkvm-input/src/platform/windows/mod.rs`). If a pointer is
invisible, look for other KVM software first.

## What is still wrong

Everything on `main` has run on the real machines: input, the clipboard in
both directions including images, and files pasted both ways. Each fault
found there was pinned with a test that failed first; the commit messages
say what each one was. What remains:

- **Dragging has not run on the real machines.** What to watch, in the log
  on the machine the drag left: `files picked up from a drag` means the
  catcher worked; nothing at all after `the cursor left` with the button
  held means the dragging application never noticed our window (on
  Windows, look at whether `DragEnter` needs the window activated; on X11,
  whether the toolkit sends `XdndEnter` on an XTEST motion). On the machine
  it arrived on: `the cursor arrived carrying files; fetching them` then
  `the files have all arrived`, then either `dropping the files where the
  pointer is` or `...are on the clipboard; paste to place them`. Two
  things known to be imperfect: the destination presses the button on
  arrival before it knows a drag is coming, so the window under the pointer
  may begin a selection of its own until `DoDragDrop` takes the capture; and
  an X11 destination makes no native drop -- the files land and are on the
  clipboard, and that is all.
- **An elevated window in front stops the client, by design of Windows.**
  UIPI refuses injected input to a process of higher integrity, so a
  PowerShell run as administrator on a client freezes the pointer there
  until the foreground changes. The client now says so and hands the cursor
  back; the cure is to run the client's scheduled task with "run with
  highest privileges" so it outranks everything it has to type into. This
  is fixed on the real machines: `reason=Elevated` refusals stopped the day
  the task was registered at highest privileges and have not recurred.
- **The secure desktop, and the service that is meant to reach it.** See
  the section below. The code is written and nothing about it has been run
  on Windows.
- **A paste of files blocks the pasting application until they arrive.**
  Explorer or Nautilus asks for the list and gets it only once every file
  is on disk, so a large paste looks hung for the duration. `transfer.max_bytes`
  bounds the wait; the fix is Windows' virtual-file form
  (`CFSTR_FILEDESCRIPTOR`), which lets Explorer show its own progress.
- **The pointer on a machine with no mouse relies on MouseKeys.** Windows
  hides the pointer outright when no mouse is attached; the client switches
  the MouseKeys accessibility setting on while the cursor is there, which is
  the only thing that makes Windows draw it (Barrier did the same). It is put
  back when the cursor leaves. While on, the number pad steers the pointer
  in whichever Num Lock state it was *not* in when the cursor arrived, so
  toggling Num Lock while there makes the digits move the pointer.
- **The X11 owner thread blocks on the network.** Serving a paste calls
  `Fetch::fetch`, which waits on the far machine, and a replacement offer is
  taken up only once it returns. Ownership now continues across the
  replacement and stale clears are recognised, so nothing is lost -- but a
  slow fetch still delays the next offer. The Windows side has the same shape
  inside `WM_RENDERFORMAT`.
- **The GUI on a client shows only that client.** A client's status report
  carries no desk -- `monitor = []` even for its own screens -- so the desk
  view is complete only on the server. Either the server sends the desk to
  its clients or the GUI reads the server's report over the link.
- **Log lines print `DeviceId` as thirty-two decimal bytes.** Hex, or the
  machine's name, would make the clipboard lines readable.

## The service, and the desktop a UAC prompt is on

This is the part of `--system` that is not in the code, and the part
somebody trying it on a real machine will need.

**What the arrangement is.** A UAC consent prompt, the lock screen and
Ctrl+Alt+Del run on a desktop winlogon makes ("Winlogon", on the WinSta0
window station). Nothing in the person's session can open, hook or inject
into it, at any integrity level -- that is the point of it, and the login
task at highest privileges does not change it. The only thing that can is a
service running as LocalSystem: it holds SeTcbPrivilege, so it may ask for
the interactive session's token and start a process *in that session,
attached to a desktop it names*. That process -- the worker -- does the
hooking and injecting for whichever desktop has the input, and runs as
SYSTEM, so nothing on the consent desktop outranks it. The service watches
which desktop has the input four times a second and relaunches the worker
when it changes. Synergy and Input Leap are built this way; it follows from
who is allowed to do what rather than from anyone's taste.

The service is the daemon. It runs the same `run` a person gets from
`smkvm run`, with the same `smkvm-core` state machines and the same Noise
link; the only difference is that `platform::injector` hands back an arm
that writes down a pipe instead of calling `SendInput`.

**Where each piece is.** `crates/smkvm-cli/src/secure/` -- `watch` decides
when to move the worker, `acl` is the pipe's access list, `wire` is what
the two say to each other, `plan` settles that a task and a service are
never both registered. All four are pure and tested on Linux. The Win32
half is under `secure/windows/`: `token` (the session's token and starting
a process on a named desktop), `pipe`, `scm` (registering the service, and
being it), `worker`, `link` (the daemon's end).

**What has not been done.** None of it has run on Windows. `cargo xwin`
links it and clippy is clean for both targets, and that is the whole of the
evidence. In particular, unverified: whether `WTSQueryUserToken` succeeds
(it wants SeTcbPrivilege *enabled*, not merely held -- see the
`AdjustTokenPrivileges` in `token.rs`); whether a worker started with
`lpDesktop = WinSta0\Winlogon` can actually set hooks there, or only
inject; whether the pipe's `O:SYD:P(A;;GA;;;SY)` is accepted by
`CreateNamedPipeW` as written; whether the input-desktop poll sees the
switch promptly enough to be useful; and what the clipboard and the drag
catcher do in a process that is not the one with the person's desktop.

**How to tell whether it is working**, in the log on the client: `running as
a service` and then `a worker is on the input desktop` with a desktop name.
Raise a consent prompt and look for a second `a worker is on the input
desktop`, this time saying `Winlogon`. If instead the log says `could not
put a worker on WinSta0\Winlogon`, the message after it is the Win32 error
and is the thing to chase. `a worker cannot be started on this desktop`
means it gave up after five tries, and the cursor is being handed back
exactly as it was before any of this -- which is the fallback working, not
a regression.

**Getting out of it.** `smkvm service uninstall` removes whichever of the
two is registered, and `smkvm service install` with no flag puts the
scheduled task back. The two are never both registered; `secure::plan` is
what makes that true and has a property test over every starting state.
`sc delete SMKVMSystem` is the hand version if the CLI cannot be run.

## Traps that have already cost time

**Check that a string replacement applied.** Editing by search-and-replace and
not verifying cost several rounds twice. The worst instance: a log line in
`cross_to` was written up in a commit message but never actually landed in the
file, so crossings appeared never to happen. Hours went into chasing a phantom
in the geometry while the real fault was elsewhere entirely. `git log -S` will
tell you whether a line was ever really there. Assert after every edit.

**Do not clear a log before reading it.** The deploy sequence cleared the log and
restarted, so every reading was of a fresh file and the evidence from the last
test was gone. This is what made the phantom above so persistent.

**`scp` fails while the binary is running.** Windows locks it. Stop the daemon
first, and check the hash on both ends afterwards — a stale binary produces
behaviour that matches no source you can read.

**Windows makes its own firewall rules**, and when the prompt goes unanswered it
makes *Block* ones, which beat any Allow. The port looked open from inside and
was unreachable from outside. Remove the automatic rules and add an explicit
allow.

**Keystrokes must never be logged.** Debug logging briefly recorded every key to
a file on disk, which is how a password would leak. The key identity is not
recorded at any level now; an unmapped key reports only its code, once, and
produces no text anywhere by definition.

**Two encodings, two rules.** The wire format is not self-describing, so
`skip_serializing_if` on anything that crosses it desynchronises the stream —
one type is shared with the config, where it looks harmless. There is a test
pinning it.

**Adding a message means classifying it.** `ServerControl::may_be_dropped`
decides which queue a message goes to and so what happens when a machine falls
behind, and `smkvm-proto/tests/dropping.rs` matches every variant exhaustively
so a new one does not compile until somebody has decided. Only pointer motion
and wheel notches may be lost.

**A refused injection is silent.** `SendInput` returns zero and Windows says
nothing more; the client used to discard the error, so a pointer stopped by
an administrator window in front looked identical to every other stopped
pointer. Every refusal is now noticed and explained in the log. If a pointer
stops on a Windows client and the log says nothing, look there first.

**Flushing an X message is not the same as the server having acted on it.**
The catcher sent `XdndFinished` and let its connection go; a server that
reads those bytes together with the end of the connection may drop them, and
about a third of the time on a loaded machine it did. The application is then
left holding a drag that never ended. A round trip after the last thing said
on a connection is what makes it stick -- `get_input_focus().reply()` is the
cheap one. The same applies anywhere a connection is closed right after a
send.

**A service is not in the person's session, and a null `lpDesktop` does not
say so.** A service lives in session 0 on the window station
`Service-0x0-3e7$`, which has no screen. `CreateProcessAsUser` with
`lpDesktop` left null gives the child *that* station, so it starts, sees
nothing, hooks nothing and reports no error -- the same silent shape as
everything else on this page. The name has to be the full `WinSta0\Default`
or `WinSta0\Winlogon`; a bare desktop name means "on my station", which is
the wrong one. `watch::on_station` is where that is built, and it is tested.

**A privilege that is held is not a privilege that is on.**
`WTSQueryUserToken` needs SeTcbPrivilege, which LocalSystem's token holds
and which is *disabled* until `AdjustTokenPrivileges` switches it on. Worse,
that call reports "you do not have it" by succeeding and setting the last
error to `ERROR_NOT_ALL_ASSIGNED`, so the obvious code says nothing and the
failure appears two calls later as an access denial that looks like the
wrong account. `token::enable_tcb_privilege` checks the last error and says
which it is.

**A scheduled task's process has a console even when nothing is on screen.**
The daemon decided where to log by asking whether stderr was a terminal, and
from inside a task it looked like one, so two deployments logged into a
console nobody could see while their log files stopped at the last
hand-started run. The task now says `--unattended`, which settles it rather
than guessing.

**The image of a program stays locked for a moment after it exits.** A deploy
that stops the daemon and immediately moves the new binary over the old one
fails, and `Move-Item` says so in a way that scrolls past; the task then
restarts the *old* binary and everything looks deployed. Wait for the process
to be gone and retry the move, and check the hash afterwards.

**Two daemons on one machine.** Before the guard in `smkvm serve`/`connect`,
starting a second `smkvm connect` left the first attached and forgotten,
holding whatever it did last. The guard reads `status.toml`; if a daemon was
killed hard, its report goes stale after 15 s and the next start is allowed.

**BLAKE3 and MASM.** Its assembly needs an assembler that does not exist when
cross-compiling from Linux, hence the `pure` feature. Anything else pulling in
assembly will need the same treatment.

## Verifying a change

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo test --workspace
```

Both targets, always: most of the platform code exists only on one of them,
and linting only the host let its problems accumulate unseen. Clippy does not
link; a release cross-build (above) is what proves the Windows binaries do.

A test written for a fix should be confirmed to fail against the old code —
revert, watch it fail, restore. Two tests have already been caught claiming to
cover something they never exercised.
