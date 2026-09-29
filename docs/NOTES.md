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

**Whose token the worker wears, which is the whole thing.** The worker
runs as LocalSystem, and it gets there by wearing the *service's own*
token moved into the console session: `OpenProcessToken` on ourselves,
`DuplicateTokenEx` to a primary token, `SetTokenInformation` with
`TokenSessionId`, then `CreateProcessAsUser`. It does **not** wear the
logged-in person's token, and the difference is not a nicety. A security
review caught the first draft calling `WTSQueryUserToken`, which hands
back the person's token, while three comments and this file said SYSTEM.
Three separate things followed from that one line: the `Winlogon`
desktop's access list admits LocalSystem and nobody else, so the process
could not have been created there at all; on `Default` the worker would
have run at medium integrity, which is *lower* than the scheduled task
runs today, so `--system` would have been a regression traded for
nothing; and the pipe's own list admits LocalSystem, so the worker could
not have opened it either. `SetTokenInformation(TokenSessionId)` is what
still needs `SeTcbPrivilege`, so `enable_tcb_privilege` stays.

Nothing widens the access list on `WinSta0` or on any desktop, and
nothing should. LocalSystem already has full access to both, which is
exactly why this arrangement reaches the secure desktop; granting the
interactive user access to the `Winlogon` desktop would take apart the
boundary that makes a consent prompt mean anything. If anything in here
ever reaches for `SetSecurityInfo` on a window station, something else
has gone wrong.

**The other half of the pipe's security.** The access list settles who may
connect to the pipe the service made. It says nothing about whether the
pipe the *worker* opened is that one, and any authenticated user may
create a name in the pipe namespace. The first draft used a counter --
`smkvm-worker-0000000000000001` from every boot -- and started the worker
without waiting to see whether its own pipe had been created, so anything
on the machine could have made that name first and had a process running
as the system account connect to it, drive keystrokes onto whatever
desktop it was attached to, and by default impersonate it outright. Three
things now rule that out, written down as `acl::Guard` so that dropping
one is a deletion somebody has to make on purpose: the name comes from
`BCryptGenRandom`; the pipe is created and proved to be ours *before* the
worker is started; the worker opens with `SECURITY_SQOS_PRESENT |
SECURITY_IDENTIFICATION` and checks that the process serving the pipe is
the system account before it says a word.

**Every wait has a deadline.** The service's side of the pipe is
overlapped for one reason: the first draft waited in `ConnectNamedPipe`
with no timeout, on the thread that also minds the worker, so a worker
that never arrived deadlocked that thread on its first poll -- no
relaunch, no giving up, no stopping the service, and no error, because a
hang is not an error. Writes are bounded too (`pipe::WRITE_WITHIN`): a
worker wedged on the Winlogon desktop stops reading, the buffer fills,
and an injection that blocks for ever holds the lock every other
injection wants. A missed deadline means the worker is dead, which is a
thing the rest of the program already knows what to do with.

**What has not been done.** None of it has run on Windows. `cargo xwin`
links it and clippy is clean for both targets; the workspace tests pass
but they compile **none** of `secure/windows/`, so what they prove is the
five pure modules (`acl`, `plan`, `reach`, `watch`, `wire`) and nothing
else. That is worth restating whenever the gate is quoted: a green run
says the decisions are right and says nothing at all about the calls. In particular, unverified: whether a worker wearing a
session-shifted SYSTEM token is in fact created on `WinSta0\Winlogon`;
whether it can set hooks there, or only inject; whether
`SetTokenInformation(TokenSessionId)` succeeds as written; whether the
pipe's `O:SYD:P(A;;GA;;;SY)` is accepted by `CreateNamedPipeW`; whether
the overlapped waits behave as intended; whether the input-desktop poll
sees the switch promptly enough to be useful; whether the service now
stops when asked; whether a failed start does produce the restart actions; whether `icacls`
applies the lists as written, whether `/setowner` succeeds on a directory
somebody else made, and whether `icacls /save` produces SDDL this build
parses -- the read-back refuses the install when it cannot, and the
service warns and carries on, so a parsing mistake here is loud in one
place and survivable in the other, but it is still a thing to see once; whether `%ProgramData%\smkvm\public` ends up
readable by `smkvm status` while `device.toml` is refused to an ordinary
account; whether carrying the three files across leaves an already-paired
machine paired; whether a worker
ever in fact fails to read its own desktop name, which is now a refusal
to start rather than a loop; whether the clipboard works through the
pipe at all, in either direction, and what a paste feels like with the
extra hop; whether a copy made while a consent prompt is up behaves as
described; whether `WTSQueryUserToken` plus `CreateEnvironmentBlock`
answers with the person's profile;
whether the quarter-second desktop comparison is in fact quick enough that
nothing is typed into the wrong desktop, as opposed to merely narrower
than it was; and what the clipboard and the drag catcher do in a process
that is not the one with the person's desktop.

**Capture on the Winlogon desktop is the thing to test rather than reason
about.** `worker.rs` already degrades when `Capture::start()` fails:
injection without capture, which is the half that matters at a consent
prompt, because what is typed there is typed from the other machine. The
design survives the answer being "inject only" and it should stay that
way.

**How to tell whether it is working**, in the log on the client: `running
as a service` and then `a worker is on the input desktop` with a desktop
name.
Raise a consent prompt and look for a second `a worker is on the input
desktop`, this time saying `Winlogon`. If instead the log says `could not
put a worker on WinSta0\Winlogon`, the message after it is the Win32 error
and is the thing to chase. `a worker cannot be started on this desktop`
means it gave up after five tries, and the cursor is being handed back
exactly as it was before any of this -- which is the fallback working, not
a regression.

**The third attempt worked, and it is measured.** Input moved to
`Winlogon` at 01:52:44.162, the cursor was handed back at .166, a worker
was on `Winlogon` at .190, the cursor was taken back at .239 --
seventy-seven milliseconds end to end. The prompt closing puts it back
on `Default` and it does not stick there, which was the thing flagged as
unproven when the watching was inverted. The injection refusals just
before each switch are in the log doing their job as the signal that the
desktop moved.

So the feature this whole branch exists for works on a real machine.
What follows is what the person hit the moment after.

**Copy and paste was broken in service mode, and it is the same fault
one layer along.** `client.rs` opens the clipboard through
`platform::clipboard()`; in service mode that runs in session 0. Session
0 has a clipboard of its own and it is not the one the person copies
into -- so the daemon watched a clipboard nobody writes to and offered
onto one nobody reads, without a single error anywhere. The drag catcher
was in the same place for the same reason. Exactly the shape of the
desktop bug: a thing that is per-session being done from the wrong
session.

The cure is the seam that already works for input. The exchange -- what
has been copied, what is announced, what is being fetched -- stays in the
service with the link and the state machine. The worker owns the actual
clipboard, because it is the only process of ours in the session. They
talk over the pipe that already exists; no second channel, and the
exchange did not move.

**The one thing that runs backwards.** Offering is lazy on purpose: a
copy announces what is available and the contents are fetched only if
something pastes, which is what stops a screenshot nobody pastes costing
anything. But the thing that pastes is on the *worker's* desktop and the
contents are on another machine, reachable only through the service. So
the worker asks the service mid-paste and waits -- `WantsPaste` out,
`Pasted` back. That is why the pipe now needs a question-and-answer in
both directions, and why there is a numbered waiting table on each side:
two pastes of different formats in flight at once would otherwise each
take whichever answer came first, which is a clipboard that hands over
the wrong thing rather than one that fails.

The wait itself is not new. On Windows the answer is rendered inside
`WM_RENDERFORMAT` with the pasting application stopped until it returns,
which was already a network round trip and is recorded further down this
file as a known shortcoming. The pipe adds one hop to it. What is new is
that it is bounded: a paste that is not answered in thirty seconds gives
up, so an application stalls rather than wedging.

**The clipboard belongs to the `Default` worker only.** The Windows
clipboard is per *window station*, not per desktop, so a worker on
`Winlogon` is on `WinSta0` and could probably reach it. It should not,
for a better reason than that: owning a clipboard means owning a
*window* that answers when something pastes, and that window is on the
desktop the worker is attached to and dies when the worker is replaced.
A worker that took the clipboard onto `Winlogon` would throw the
standing offer away every time a consent prompt appeared. So a switch
to `Winlogon` *suspends* the clipboard rather than losing it: the offer
lives in the service, which announces it again as soon as a worker is
back on the ordinary desktop.

**What a person should expect.** A copy on another machine can be pasted
here as usual. If a consent prompt is up, the clipboard is not being
served for as long as it is up -- a paste during that moment does
nothing, and works again when the prompt goes. Nothing is lost by
waiting. A file sent to this machine lands under the person's own
`transfer.directory`, which by default is `~/Downloads/SMKVM` in *their*
profile, not the system one; an explicit path in `smkvm.toml` is used as
written and is the way to put it anywhere else.

**Where a received file lands, and why the service answers that
question.** It used to resolve `~` in the system profile, which is
recorded below as a gap. It is not the same question as "whose
configuration should a service read", and that is the good part: there
*is* a person at the screen and the system knows who. The service asks
`WTSQueryUserToken` for their token and `CreateEnvironmentBlock` for
their environment, and reads `USERPROFILE` out of it -- no guess at
`C:\Users\<name>`, no registry, no assumption that profiles are in the
usual place. Walking that block is pure and tested.

The worker could have been asked instead, being in the session. It was
not, and the reason is worth stating because it looks like the obvious
choice: the worker is the system account too, so it would have to ask
the same way, and the service already holds the privilege it needs. The
worker has exactly one thing the service lacks -- being in the session
-- and for this question the service can get the same answer without a
round trip.

**What the second attempt on real hardware found, and the thing it
settled.** Three things worked that had never been run before: the
service starts, runs as the system account in session 0, and reads
`C:\ProgramData\smkvm\smkvm.toml` rather than a profile; the key's
access list read back as `KEY PRIVATE`, and `type` on it from a
de-elevated token was refused, with the inherited list on the file being
`(A;ID;FA;;;BA)(A;ID;FA;;;SY)` and nothing else; and the service
connected to the server. The access-list work is done and proven, and
the parser that had never seen real `icacls /save` output parsed it.

**And no worker was ever started.** Not a worker that failed to start --
no attempt, no line, no process. What was wrong is one assumption that
both the author and the review accepted: that a service running as the
system account could ask which desktop has the input.

It cannot. `OpenInputDesktop` is per *window station*, and a service
lives in session 0 on `Service-0x0-3e7$`, which is not the station the
screens are on. Asked from there it answers about session 0 or not at
all -- never about session 1. The reply was `OutOfReach`, which the loop
read as `Unreadable`, which it treated as "no news, leave the worker
where it is"; there being no worker at all, that meant leaving none.
`Step::Stay`, four times a second, in silence, for ever.

The elimination is worth keeping because it is how the log was read:
every other branch of that poll ends in `Step::Move`, which ends in
`start_worker`, which logs whether it succeeds or fails. No such line
existed, so no other branch had been taken.

**So the watching is inverted.** The service cannot see the input
desktop, and no amount of privilege changes that -- it is the wrong
station, not insufficient rights. Something inside the session has to
look. So a worker goes in first, on `Default`, which an interactive
session always has; the worker polls `desktop::input_name()` from
`WinSta0` where the question has an answer, and reports changes over the
pipe; and the service acts on what it is told. `watch::Seen::Unreadable`
now means two different things depending on whether a worker exists --
with one it is still no news, and without one it is the reason to start
the first, which is the bootstrap that was missing.

This is the shape Synergy uses, and now for the reason rather than by
imitation: the only process that can answer the question is one that is
already in the session.

**What the first attempt on real hardware found.** It reached
`service_main`, logged that it was running as a service, and exited one
second later with `ERROR_SERVICE_SPECIFIC_ERROR` -- the daemon, running as
LocalSystem, had looked for its configuration in
`C:\WINDOWS\system32\config\systemprofile\AppData\Roaming\smkvm` and
found none. Nothing to do with any Win32 call; the service simply had no
idea whose files it was meant to read. Worth recording for two reasons.
The diagnosis took one `sc query` and one log line, which is the exit-code
fix from the round before earning itself back. And it is the shape of
defect none of the reasoning rounds could have caught: everything about it
was correct except an assumption about the environment, which only a
machine has.

The fix is `Scope` in `smkvm-config::paths`. Service mode reads and writes
under `%ProgramData%\smkvm`; everything else is unchanged. The choosing is
a pure function of the scope and the environment roots, so the case that
failed -- LocalSystem's real `APPDATA`, which resolves perfectly well and
is simply the wrong place -- is now a test on any machine.

**Why the service does not read a person's profile instead.** It would be
the smaller change and it is the wrong one. Which person? At boot, before
anyone has logged in, there is nobody -- and that is precisely the case
`--system` exists to cover. A service running as the system account
reaching into `C:\Users\<somebody>\AppData` is also a thing to be uneasy
about on its own account. So the files are carried across once, at install
time, by the person installing, who is the one whose machine this is
paired as.

**What is carried, and what is deliberately not.** `smkvm.toml`,
`device.toml` and `peers.toml` -- `paths::CARRIED_OVER`, named in one place
so the copying cannot drift from the list. Not the status report, which is
written by whichever daemon is running and would announce a daemon that is
not there. A file already in the machine directory is left alone rather
than overwritten, and the reason is `device.toml`: replacing it is not an
update, it is becoming a different machine, because every other machine's
paired list still names the old key. So a second `install --system` is
safe, which is the thing somebody is most likely to try.

Left alone, but not silently. The bytes are compared, because "already
there" covers two very different situations. A second install of the same
machine is one calm line. A machine that was paired again under the login
task after an earlier `--system` install has a new key in the profile and
the old one machine-wide, and the old one is what the service would
introduce itself with -- so it connects and is turned away in the
handshake, with nothing anywhere to explain it. That case now prints a
warning naming both files and saying what to do. `uninstall`
leaves the copies and says where they are, for the same reason -- deleting
an identity is the one irreversible act here.

**Who may read what: closed by construction.** `%ProgramData%` grants
every authenticated account read by inheritance, so a directory made there
and left alone puts this machine's private key where any local account can
read it. That key is what the server trusts this machine by: a copy of it
is a machine that can connect as this one and be handed the cursor, and
therefore the keystrokes that follow it, including the ones typed into a
password field on what the person believes is their own client.

The first version of this granted ordinary accounts read on the directory
and then took it away again from each secret, and a review held the
deployment over it. That shape was wrong in three ways at once, all of them
the same mistake -- the default was open and protection was bolted on:

- a key the *service* generated later, on the documented path where there
  was nothing to carry over, was never tightened at all, because the only
  code that tightened anything ran at install time. It inherited the
  directory's grant and stayed readable for ever, while the install printed
  a line saying the opposite;
- a key that *was* carried over was readable in the milliseconds between
  being written and being tightened -- a race a local process wins easily
  against a one-shot, user-initiated event;
- and anything added later would have been open unless somebody remembered.

So the directory grants the system account and administrators, inheritably,
and nobody else. Anything created in it afterwards is closed without anyone
having to think of it. Ordinary accounts get read on the directory *object*
with no inheritance, enough to walk into the one readable subdirectory,
`public`, which is where the daemon writes its report and where nothing else
goes. A subdirectory rather than a permission on the report, because the
report does not exist when the lists are set, and a file created later
inherits what the directory says -- which is the whole lesson above.

`peers.toml` needs the other half of the argument: its contents are public
keys and are not secret, but a peer written into it is a machine this one
will accept a session from, so ordinary accounts get no write anywhere in
the directory.

**Ownership, which is the part a grant cannot fix.** `%ProgramData%` lets
ordinary accounts create folders and grants `CREATOR OWNER` full control of
what they create, so any local account can make `C:\ProgramData\smkvm`
before the installer does and be its owner. `/inheritance:r` removes only
inherited entries and `/grant:r` rewrites only the ones it names, so a
stranger's explicit entry survives both -- and an owner holds `WRITE_DAC`
implicitly, so stripping it would only postpone them putting it back. This
is the pipe-name squatting from the first review against a different
object, and `acl.rs` had already written the sentence for it: *the owner of
an object can always rewrite its access control list.* The owner is now set
to the system account before any grant, and a directory that was already
there is a decision in the code rather than something `create_dir_all`
steps over.

**And the list is read back.** Setting one and believing the exit code is
the same shape as the silent refused injection this project began with.
`icacls` prints "Successfully processed 0 files; Failed processing 1 files"
and has been known to exit zero doing it. So every list is read back with
`icacls /save`, which emits SDDL -- machine-readable, and unlike the names
in its ordinary printing not translated -- and checked by
`acl::granted_to_anyone_but`.

**The parser had to be rewritten, and how it was found is the point.** A
reviewer extracted it and fed it adversarial input rather than reading it,
and found four lists it blessed while the key was readable. Each is now a
test, kept verbatim:

| what was fed to it | what it said | what was true |
|---|---|---|
| `O:SYG:SYD:NO_ACCESS_CONTROL` | safe | no list at all: everyone, everything |
| `D:P(A;;FA;;;SY)(XA;;FA;;;WD;(1==1))` | safe | conditional allow grants Everyone |
| `D:P(XA;;FA;;;BA;(@a=="S:"))(A;;FA;;;WD)` | safe | the condition truncated the scan |
| `D:P(XA;;FA;;;BU;(Member_of{SID(BA)}))` | safe | splitting on `(` shredded the entry |

Four bugs, one attacker, all reachable in the scenario the check exists
for -- somebody who owned the directory before the install and left an
explicit entry that no `icacls` verb removes. Three rules came out of it,
and they are the ones to keep if this is ever touched again:

- **Entries are tokenised by balanced brackets before anything else.** A
  condition may contain brackets, quotes and the text `S:`, and it is
  attacker-written text sitting inside the thing being parsed. Nothing may
  be located by searching through it -- not the end of the list, not the
  start of one. Tokenising first is also what makes refusing a short entry
  safe: before it, the scan produced short fragments of its own.
- **The type test is inverted.** The harmless types are listed --
  denials, audits, alarms, labels -- and everything else grants, including
  a type this build has never heard of. Listing the granting types is what
  hid `XA` and `ZA`, and would hide whatever Windows adds next.
- **Every uncertainty resolves towards "somebody can read it."** An entry
  that cannot be read is an offender, a list that says it does not exist
  is the worst answer rather than an empty one, and a descriptor
  carrying two discretionary lists is refused rather than resolved to
  the first. That last one is not reachable -- the system function that
  writes this text serialises exactly one list -- but taking the first
  and discarding the rest is the shape of every bypass above, so it is
  refused on principle rather than reasoned about.

`NO_ACCESS_CONTROL` deserves its own sentence, because there was already a
test for a null list and it did not catch it: the test covered a
descriptor with no `D:` component, and this is the spelling the system
actually emits. A test written for exactly the bug, missing it by guessing
the wording.

**The directory is checked before anything is written into it.** The
earlier version checked only the key and only after copying it, so a
hostile inherited entry was found by writing the key into a file that
entry could read -- an honest report of an exposure that had already
happened. What a directory *hands down* is a narrower question than who
is on its list (ordinary accounts may reach the directory and must
inherit nothing from it), so it is a second function,
`acl::inheritably_granted_to_anyone_but`, with its own tests.

**Install and service answer an unreadable list differently, on purpose.**
Three verdicts, not two: private, readable, and could-not-tell. The
install refuses all three but the first, because a person is standing
there and can act. The service refuses a readable key -- carrying on
means handshaking with a secret that is not one -- but only warns when
the list could not be parsed, because failing to parse is not evidence of
a fault, this parser has been wrong four times, and taking the machine
down over the parser's own ignorance is the same shape as the outage this
hardware round began with. The log says `KEY PRIVATE`, `KEY READABLE` or
`KEY UNPROVEN`, so which of the three it was is one glance.

And `smkvm status` says so too, every time it is run. A startup line is
one line, weeks ago, in a file nobody re-reads, so a key that could not
be proven private would quietly stay that way. The status command does
not echo the service's verdict, it measures again and by the most direct
means there is: it opens the file. That is `type
%ProgramData%\smkvm\device.toml` made into code -- no access list
parsed, no agreement with `icacls` about anything, and whether the open
succeeds is not an opinion. What it cannot know is whether the account
asking is an administrator, who may legitimately read it, so it reports
what it found and who that would be a fault for rather than pronouncing.

**The three shapes of "this did not work", which must not read alike.**
Standing at the machine, these are three different situations and only
one of them is an access list that is wrong:

- `icacls itself would not answer` -- the tool did not run, or what it
  wrote could not be read back. Nothing was examined and nothing is
  claimed.
- `icacls answered, but this build found no entry for …` -- a
  disagreement about the format of `icacls /save`, not a fault in any
  list. It names what it did find, because the difference between "I
  cannot see it" and "it is wrong" is the difference between a minute
  and an afternoon.
- `the access list is wrong: … can be reached by …` -- the real thing,
  naming the trustee.

The `/save` carries `/T`, so one call covers the directory and
everything beneath it. Without it, `/save` is documented as storing the
lists of a directory's contents and is often observed to store the
directory itself -- and this one call has to answer both questions, so
whichever shape a given Windows returns, the other lookup would find
nothing and refuse the install. Fail-closed and loud, but an entirely
avoidable way to spend a deployment.

The check somebody will actually run is `type
%ProgramData%\smkvm\device.toml` from an ordinary account, which must be
refused. That one command would have caught all three of the faults above,
and it is worth running first on any machine this is installed on.

The groups are named to `icacls` by their well-known identifiers rather
than as "Administrators" and "Users", because those names are translated --
on the machines this is meant for they are something else entirely, and a
rule that silently fails to apply is how a private key ends up readable
while the install prints success. That lesson is in this file once already,
about a service status parsed out of translated text.

**Installing over ssh is not installing as the person at the desk.**
These machines are administered from elsewhere, by an administrator
account that is not the one the machine is paired as. `install --system`
carried the three files out of the *installing* account's profile, found
nothing, and said so correctly -- which meant `--system` could not be
installed remotely at all, and the files had to be copied by hand. It
now takes them from whoever is logged in at the screen
(`Win32_ComputerSystem.UserName`), or from `--user <name>` when told,
resolving the profile through the same profile list the login task's
principal is looked up in. It says whose files it carried.

**Check the key before anything else.** From an *ordinary* account, not
an administrator one:

    type %ProgramData%\smkvm\device.toml

It must be refused. `smkvm status` run from that same account says the
same thing in one line, and is the way to keep checking afterwards. That single command would have caught all three of the
access-list faults, and it is worth running before the service is started
for the first time. The install refuses by itself if the list is wrong, and
so does the service at startup, but neither is a substitute for looking.

**What the first session on real hardware should look like.** In order:
the service starts and logs `running as a service`; if nobody is logged
in yet it says so once and waits, which is not a failure; it says once
that nothing has told it where the input is, and then a worker appears
on `Default` and `a worker is reporting where the input is` follows; raising a consent prompt produces a second `a worker is on the
input desktop` saying `Winlogon`, and between the two the client should
log that it handed the cursor back because the worker had not moved yet.
Then `smkvm service stop` and confirm it stops rather than timing out.

**The two desktop names must agree, and that is the one check no test on
a Linux machine can make.** Each worker logs
`the worker is on the desktop and connected` with a `desktop=` field, and
the service logs `a worker is on the input desktop` with its own
`desktop=`. Those two come from different places -- the worker's
`desktop::ours()` and the name the service polled -- so when they agree,
the worker really is where the service thinks it is. Read them as a pair
at each switch:

    desktop=Default   when the ordinary desktop has the input
    desktop=Winlogon  while a consent prompt or the lock screen is up

A worker whose line says `Default` while a prompt is up is the bug
described in the traps below, back again: input would be going to a
worker bound to `Winlogon` and landing nowhere. There is no way to
exercise this without Windows -- the call it turns on exists only there,
and the fix removed the decision rather than making it testable -- so
this log pair is the whole of the evidence for it.

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

**`Environment` under a service's registry key must be `REG_MULTI_SZ`.**
That is the documented way to give a service an environment variable,
and it is real -- but written as `REG_SZ`, which is what
`New-ItemProperty` gives by default, the service control manager ignores
it and says nothing. An attempt to raise the log level that way on a
real machine produced no debug lines and no error, which is an hour lost
at the worst moment. There is now a file as well: one line in
`%ProgramData%\smkvm\log-level` saying `debug`, read at startup, with
nothing to get right but the contents. The order is environment, then
file, then `--verbose`, then `info`, and it is a pure function with
tests.

**`sc.exe`'s printing is translated, so parsing it fails exactly where it
is deployed.** `describe()` used to look for lines beginning `STATE` and
`SERVICE_START_NAME`; on a Korean or Japanese Windows neither is there
and both fell through to a question mark -- in the status line somebody
reads when they are already trying to work out why nothing works. It asks
the service control manager directly now
(`QueryServiceConfigW` / `QueryServiceStatusEx`). `sc.exe` is still used
to *change* things, where a person is watching and the exit code is the
answer.

**Windows makes its own firewall rules**, and when the prompt goes unanswered it
makes *Block* ones, which beat any Allow. The port looked open from inside and
was unreachable from outside. Remove the automatic rules and add an explicit
allow.

**A service that exits zero was not a service that failed, as far as the
manager is concerned.** The restart actions registered at install apply
only to a service that terminates with a code, so a `service_main` that
reports `NO_ERROR` and calls `exit(0)` whatever happened makes them inert
-- and the case they exist for, stopped at three in the morning and
staying stopped, is exactly the case not covered. It also makes `sc start`
report success while the status says "stopped", with the reason only in
the log. Failure now reports `ERROR_SERVICE_SPECIFIC_ERROR` and exits
non-zero.

**"Nobody is logged in yet" is a wait, not a failure of the desktop.** At
boot the input desktop is `Winlogon` and there may be no console session
for a moment. Counting that as a failed start exhausted the give-up
counter within about a second and latched give-up on `Winlogon`, so the
lock screen was unreachable until somebody had logged in once -- and the
log said "a worker cannot be started on this desktop", which reads like a
permission problem. The session is checked before anything else in the
loop now, and says so once rather than four times a second.

**A refusal believed for a flat interval turns a stuck worker into a
flapping cursor, and the obvious way to reset the backoff cancels it.**
The interval doubles per consecutive refusal to a ceiling. What took two
attempts to get right is when a run of refusals is allowed to end. The
first version reset the streak when an injection was *sent* on the
reasoning that an injection which goes out and is not refused means the
trouble is over -- which is sound, and worthless at that moment: the
client only injects once the wait has expired, so the first injection
after every resume satisfied the test by construction, before the
refusal it was about could possibly have arrived. The doubling never took
effect once. A simulation of a minute of continuous refusal measured 29
resumes with that reset and 4 without it, so the reset was worse than
having none, and the unit test asserted it as the intended behaviour.
Both are fixed, and the test now asserts the *rate* over a simulated
minute, because the rate is the thing that was wrong; the old test
passed throughout because it never ran two cycles.

The second attempt had the same shape in a different place: suspending
is itself several injections -- the held keys are released and the
pointer shown -- and those go out microseconds after the refusal that
caused the suspension. Counting them made every refusal look survived one
grace period later. A send is only evidence if it was made while input
was believed to be landing, and only once the time a refusal would have
taken to arrive has passed with nothing.

**A service that does not answer a stop is killed, and on a server that
means the keyboard is not given back.** The daemon has always stopped on
Ctrl+C, which a service never gets: the manager delivers a stop to a
handler on a thread of its own. The first draft set a flag the daemon
never read, so the service ran until the manager's patience ran out.
There are now two ways to ask -- Ctrl+C and `ask_to_stop` -- arriving at
one `stopping()` that replaced `ctrl_c()` everywhere the daemon waited on
it. A `STOP_PENDING` report also has to carry `dwWaitHint` and a rising
`dwCheckPoint`, or the manager has no reason to keep waiting and treats
the service as hung.

**A report is chosen by when it was written, not by where it is.** The
first version preferred the machine-wide file because it was first in the
list. Nothing deletes that file when a service is uninstalled, so a dead
report sat in `%ProgramData%` permanently and shadowed the live one a
hand-started daemon was writing in its own profile: every reader picked
the stale file, the freshness filter then threw it away, and `smkvm
status` said nothing was running while the daemon ran perfectly, for ever,
with no error anywhere. The report carries the time it was written, so the
question has an answer that does not depend on which directory it is in.
`uninstall` also deletes the machine-wide report now -- it is not an
identity, and losing it costs nothing.

**The system account has a profile, and it is not a useful one.**
LocalSystem's `APPDATA` and `LOCALAPPDATA` resolve to real directories
under `C:\WINDOWS\system32\config\systemprofile`. Nothing fails, nothing
is empty-stringed, nothing warns: paths built from them are perfectly
well-formed and point at a place none of this machine's files are. A
service written on the assumption that "the environment says where my
files are" is therefore wrong in the one way that produces no error until
the file is opened. Which files belong to a person and which to the
machine is a decision, and it is `paths::Scope`.

**"Which desktop has the input" and "which desktop am I on" are different
questions, and the call that answers the first will hand you a name that
looks like an answer to the second.** `desktop::current()` answers the
first; its `Elsewhere(name)` carries the *input* desktop's name. The
worker used it to report its own, which was harmless until
`worker_started` began comparing where a worker landed against where it
was sent. Then: a prompt appears, the service starts a worker for
`Winlogon`, the person dismisses the prompt before the worker connects,
and the worker -- genuinely bound to `Winlogon` -- looks at the input,
sees `Default`, and reports `"Default"`. The service now believes the
worker is exactly where the input is, so every look is `Step::Stay` and
the reach agrees, while every keystroke goes to a worker that can only
inject onto a desktop nobody is looking at. They vanish, silently and
confidently, until the next desktop switch happens to replace it.
`desktop::ours()` is the call for this, and the worker no longer looks at
the input desktop at all -- not even in a match arm, so the wrong arm
cannot be picked again.

**A worker that cannot name its desktop is a relaunch loop with the guard
missing.** It used to report `"?"`, which was accepted. `"?"` can never
equal the name the service's poll reads, so the service replaced the
worker on every look -- and a deliberate replacement counts no failure,
so the give-up counter never reached its limit: a process running as the
system account started and killed four times a second, indefinitely. That
is the same bad outcome as the relaunch loop fixed two rounds ago,
through a door `watch` did not guard. Shut from both sides now: the
worker fails rather than reporting a name it could not read,
`wire::welcome` refuses a name that is not a desktop name, and
`watch::worker_started` counts a worker that lands somewhere other than
where it was sent as a failure of the desktop it was sent to, so any
future name that can never match is given up on rather than looped on.

**A `continue` skips everything below it, including the things that keep
the truth current.** The "nobody is logged in yet" wait was put at the
top of the minding loop, above both the worker-liveness check and the
input-desktop update. Log off while a worker is running and neither ran:
`Reach` went on holding a worker and a matching desktop for a screen with
no session on it, claiming input was landing, until the next injection
timed out a second later. The wait now goes *after* the observing, and
detaches before it waits.

**A checker is only as good as the inputs it has met, and reading it is
not meeting them.** The access-list parser was read carefully by its
author, reviewed, and had six tests -- and still blessed four different
lists that left the key readable. Every one was found by running inputs
through it. Two lessons worth keeping: a test written for a bug can miss
that bug by guessing how the system spells it, and a parser whose input
includes attacker-written text (an SDDL condition) must be tokenised
structurally before anything is searched for inside it.

**A structure read back out of a `Vec<u8>` is unaligned, and that is
undefined whatever the hardware tolerates.** `GetTokenInformation` and
`QueryServiceConfigW` fill a caller's buffer with a structure full of
pointers and expect it read back as that structure. A `Vec<u8>` is aligned
to one byte; taking a reference to the structure inside one is undefined
behaviour in Rust even where the processor would not care, and it will
work in testing every time. `secure::windows::Aligned` backs the bytes
with `u64` instead.

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

**A worker that is attached is not the same as a worker that is in the
right place.** The service polls the input desktop four times a second, so
when a consent prompt appears the worker is still on `Default` for up to a
quarter of a second after the input has moved to `Winlogon`. A reach that
knows only "a worker is attached" says input is landing throughout that
window -- and it is landing, on the wrong desktop. If somebody is typing an
administrator's password toward the prompt, the opening characters go into
whatever window is focused behind it. `reach` therefore holds both the
worker's desktop and the last polled input desktop and reports no reach
when they differ, and `mind_workers` tells it what it saw on every look
rather than only when the worker moves. The comparison is free because the
service is the system account, so its poll already succeeds.

**Anything that belongs to a session is wrong in a service, and the
compiler cannot tell you which things those are.** The desktop was the
first. The clipboard was the second, found the same way -- on a machine,
by somebody trying to use it -- and the drag catcher came with it. Both
ran without error against a real object that was simply the wrong one,
because session 0 has a desktop and a clipboard of its own. The list of
things to suspect is anything the system keeps one of *per session or
per window station*: the clipboard, the desktop, window stations,
interactive windows, the shell, the pointer. If the service is doing any
of them itself rather than through the worker, it is doing them in a
place nobody can see.

**A window station is not a privilege, and the system account does not
get you across one.** Every desktop and window-station call is scoped to
the station the calling process is on. A service is on
`Service-0x0-3e7$` in session 0; the screens are on `WinSta0` in session
1. `OpenInputDesktop` from the service is not refused for want of rights
-- it answers a different question, about a station with no screen on
it. Running as the system account is what lets a process *start*
something in the other session; it is not what lets it see into one. The
two got conflated, by the author and by a reviewer, and cost a hardware
round. If a call takes no session or station argument, ask which station
it is implicitly about before assuming SYSTEM covers it.

**A loop that decides to do nothing must say so, once.** The minding
thread took the same silent branch four times a second for the life of
the service. Everything in it was at debug, the service ran at info, and
the result was a log that looked like a healthy service and a machine
that did nothing. Both the no-session wait and the nobody-is-watching
case now say so once, latched, at info. This is the silent refused
injection again, in a third costume.

**A panicking thread is silent in a service.** A panic prints to stderr
and unwinds that thread; a service has no stderr anybody reads, so the
thread simply stops and nothing mentions it -- and "the minding thread
died" looks exactly like "the minding thread decided to do nothing",
which is precisely the pair that could not be told apart on the machine.
There is a panic hook now that logs before the default one runs.

**Asking "can input reach the screen" from a service asks about the wrong
screen.** `platform::injection_blocked` and `injection_possible` answer by
looking around the process that asks -- which desktop has the input, what
is in front, does a probe land. In the daemon that is right, because the
process that asks is the process that injects. In the service it is wrong
in the silent direction: it asks from session 0 on `Service-0x0-3e7$`,
which has no screen and never will, so the answer is "blocked" for ever
however well the worker is doing, and the client suspends at the first
refusal and never resumes. Both now take the worker's word in service
mode (`secure::reach`), and the accounting is pure so it is tested on
whatever machine is building.

**A service is not in the person's session, and a null `lpDesktop` does not
say so.** A service lives in session 0 on the window station
`Service-0x0-3e7$`, which has no screen. `CreateProcessAsUser` with
`lpDesktop` left null gives the child *that* station, so it starts, sees
nothing, hooks nothing and reports no error -- the same silent shape as
everything else on this page. The name has to be the full `WinSta0\Default`
or `WinSta0\Winlogon`; a bare desktop name means "on my station", which is
the wrong one. `watch::on_station` is where that is built, and it is tested.

**A privilege that is held is not a privilege that is on.**
`SetTokenInformation(TokenSessionId)` needs SeTcbPrivilege, which
LocalSystem's token holds and which is *disabled* until
`AdjustTokenPrivileges` switches it on. Worse,
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
