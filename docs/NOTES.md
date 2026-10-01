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
- **Under the service, nothing copied on the machine reaches another
  one.** Inbound works -- what another machine copies can be pasted here,
  text, images and files. Outbound does not, because a process running as
  the system account cannot see what the logged-on user copied. This is
  measured and understood, and fixing it is a choice with real costs
  rather than a defect to be patched; the next section lays the choice
  out. Under the scheduled task, outbound works as it always has.

## The clipboard, the account, and a choice nobody has made yet

This section exists to be read before anybody writes code for it. It
is a decision, not a design, and the measurement behind it is
finished.

### What was measured, and what it settles

Three observers, the same session, the same desktop, the same copy of
the same file.

| observer | sees | can fetch |
| --- | --- | --- |
| the copying process, as the user | five formats | yes |
| another process, as the user, plain Win32 only | `49161=DataObject`, `15` (`CF_HDROP`), `49159=FileNameW`, `49158=FileName`, `49171=Ole Private Data` | `GetClipboardData(15)` returns a real handle |
| the worker, as the system account | `49161=DataObject`, and nothing else | no |

The worker looked eight times across 1750 ms and the list never
changed, so it is not a copy still being assembled. It then looked
again from a thread with a COM apartment, both the plain way and by
asking the data object: plain Win32 saw the same single entry, and
`IDataObject::EnumFormatEtc` returned an empty list. So an apartment
is not the difference either, and `available_now` was never at fault
-- `CF_HDROP` has been in it throughout.

**What this settles: a process running as the system account cannot
see what the logged-on user copied, however it asks.**

**What it does not settle: why.** Two mechanisms fit everything
observed -- that the formats past `CF_DATAOBJECT` are produced by OLE
inside the asking process and cannot be marshalled across a user
boundary, or that the format list itself is filtered by token. They
have not been told apart, and for the choice below it does not matter:
every shape that fixes it fixes it by having a process of the user's
own identity do the looking. Nobody should spend a round on the
mechanism before the shape is chosen.

One thing the measurement is silent on, and it is the cheapest open
question here: **nobody has tested a system process that
*impersonates* the user.** That single test decides whether the first
shape below is viable at all, and it is an afternoon.

### Why this is a choice and not a bug

Input must be the system account or it cannot reach a consent prompt.
The clipboard must be the logged-on user or it cannot see what they
copied. Those two requirements are not in tension -- they only
conflict because one process is being asked to satisfy both.

It is also, right now, a trade the person at the desk is living with.
The client is on the service: it reaches the consent desktop and has
no outbound clipboard. The server is on the scheduled task: it has
the clipboard in both directions and cannot reach the consent
desktop. Both arrangements work; neither does everything. **Doing
nothing is a real option, and for a machine that rarely sees a
consent prompt the scheduled task is the better arrangement today.**
That is worth saying first, because the engineering costs below are
not small and this one is zero.

### Shape A -- impersonate the user on the clipboard's threads

`WTSQueryUserToken`, then `ImpersonateLoggedOnUser` on the threads
that touch the clipboard.

*What it costs.* Impersonation is per-thread, and the clipboard
already spans three: the window thread, the COM thread, and whichever
thread reads. Every one of them has to impersonate, and any future
thread handed clipboard work has to as well -- a rule that is
invisible when broken, which is the category of mistake that has cost
this file most of its rounds. The clipboard window's identity is
likely fixed when the window is created, so the window probably has
to be created while impersonating rather than impersonated
afterwards. COM and impersonation together need cloaking set
deliberately (`CoSetProxyBlanket`, `EOAC_STATIC_CLOAKING`), and
getting that wrong fails silently in exactly the way this has already
failed eleven times.

*What is unknown.* Whether it works at all. Untested, and testable in
an afternoon as noted above. Until that test exists this shape should
not be scoped further.

*What it does not cost.* No second process, no new lifecycle, no new
pipe, and no window-station or desktop DACL is widened -- which was
the line drawn at the start of this work and still holds.

### Shape B -- a second helper, running as the logged-on user

The service already knows how to start a process in the interactive
session as a chosen identity; that is `CreateProcessAsUserW`, and a
`WTSQueryUserToken` path existed here before it was replaced with the
system token to make input work. So: two helpers rather than one. The
desktop worker stays exactly as it is -- the system account, input,
following the desktop. A second helper runs as the user, on `Default`
only, and owns the clipboard.

*What it costs.* A second process to supervise: started, noticed when
it dies, restarted. A second lifecycle to get right around logon,
logoff, lock, unlock and fast user switching; between logoff and
logon there is no user, so there is no clipboard helper, which is
arguably the correct answer rather than a gap. Two things to install
and uninstall cleanly.

*And one cost that is larger than it looks.* The worker's pipe is
`O:SYD:P(A;;GA;;;SY)` -- the system account and nobody else, which is
one of the three guards named in `secure::acl`. A helper running as
the user cannot connect to it. It would need a pipe of its own whose
DACL admits the interactive user, and **that endpoint is then
reachable by anything running as that person.** It must therefore
carry clipboard messages only and must not be able to drive
injection, which means splitting `ToWorker` and `FromWorker` rather
than reusing them whole. Treating the user-side pipe as untrusted
input is not optional and is the main new piece of security thinking
this shape requires. It does not weaken the consent-prompt boundary
-- a pipe is not a desktop -- but it is a new surface and should be
designed as one.

*What it buys.* The clipboard half becomes ordinary code running as
an ordinary user, which is the arrangement that demonstrably works on
three machines today. No impersonation, no cloaking, no per-thread
rule that is invisible when broken, and each half has exactly the
identity it needs.

### Shape B-minimal -- a helper that only reads

Worth separating out, because it may be most of shape B's benefit for
a fraction of its cost.

Note the asymmetry the measurement actually shows. **Inbound works
today as the system account**: the worker takes the clipboard,
promises formats and serves renders, and all of that is about a
clipboard we own. Only *reading what another process put there* needs
the user's identity. So the helper does not need to own the
clipboard, offer anything, or handle a single render. It answers two
questions -- what is on the clipboard now, and give me this format --
and watches for changes, which is part of reading.

That matters for the cost above in two ways. A read-only endpoint
cannot drive injection by construction, so the security question
mostly answers itself. And nothing that works today moves: the
inbound path, the renders, the promise, the renewal and the whole
secure-desktop arrangement stay in the process they are in now.
Moving a working path into a new process to fix a broken one is the
mistake this file has already made in a smaller form, and this avoids
it.

The drag catcher reads the shell's data and almost certainly has the
same problem; it belongs with reading rather than with input.

*What is unknown.* Whether a process the *service* starts as the user
sees the five formats. A process started by hand does -- that is the
second row of the table -- but the token path differs, and that is
the one thing to confirm before building.

### What the measurements said, and what was built

Three observers, one page, after a file copy in Explorer:

| observer | sees |
| --- | --- |
| the service's own process, as the system account | one format |
| a thread of it impersonating the logged-on user | the same one format |
| a child started as that user with `WTSQueryUserToken` and `CreateProcessAsUserW` | all five, and reads real bytes from every one -- 114 of `CF_HDROP`, the file list |

**Impersonation is not the answer.** Shape A is closed, and the
reason is worth keeping: the identity has to belong to a *process*,
not to a thread of one. Anything built on `ImpersonateLoggedOnUser`
here would have failed in the same silent way, after the work.

So B-minimal was built. `secure::reading` is a protocol with three
messages, all of them reads; `secure::windows::reader` is the half
that runs as the person; `secure::windows::readers` is the service's
end of it. Offering, rendering and injection did not move and run as
the system account exactly as before.

Six things about it that are decisions rather than details:

- **The reader gets the person's environment; the worker gets the
  service's.** The two calls look identical and want opposite
  answers, which is why `token::Environment` names the choice rather
  than taking a `bool`. Removing `CreateEnvironmentBlock` for the
  worker was right -- with a *system* token it builds LocalSystem's
  environment, which is what the worker already has. Passing `None`
  for the reader is not: it runs *as the person*, so it inherited
  `APPDATA` and `LOCALAPPDATA` pointing into the system profile, a
  place that account cannot write, and its first act is to open a log
  there. It died before a line reached disk anywhere and the service
  saw only a broken pipe. `CREATE_UNICODE_ENVIRONMENT` goes with the
  block, or the wide strings are read as ANSI and the child gets an
  environment of mojibake.

- **Nothing chooses a source once.** The backends are built at the
  top of the daemon, outside the reconnect loop, because the
  clipboard outlives any one session -- and the service starts at
  boot, before anybody has logged in. So asking "is there a reader?"
  at that moment answers no for the life of the process. The first
  version did exactly that: it latched the worker, logged that a
  reader was running, and consulted nothing. Copies now arrive on one
  channel that both halves feed, and a read asks who can answer at
  the time it is made. **The line that proves it is working is
  `the reader says something was copied`**, because that one comes
  from the consuming end and appears only if something is consuming.

- **The reader's pipe admits interactive users, and the worker's
  still admits nobody but the system account.** Two pipes, because
  two identities. `GRGW` and not `GA` on the reader's: generic all
  includes `WRITE_DAC`, so the person could rewrite who else gets in.
  A test asserts both.
- **The protocol cannot express an injection**, so nobody has to
  reason about whether one can be smuggled through an endpoint every
  process the person owns can reach. The test that enforces this is a
  match with no wildcard: adding a fourth message breaks it, and the
  comment there says what question to answer before changing it.
- **The reader checks that the service is the system account; the
  service checks that the reader is the process it started.** The
  first matters more than its mirror does for the worker: the reader
  runs as the person, so without it anything else running as them
  could put up a pipe of that name and be told what they copy.
- **The reader follows the session; the worker follows the desktop.**
  They are minded by separate loops because they change on different
  occasions -- a worker several times a minute while consent prompts
  come and go, a reader at logon and logoff. Starting one is tried
  five times and then given up on until the next logon, because the
  failure that matters is somebody else on the desktop winning the
  race to the pipe, and retrying that for ever costs a process
  started as the system account every five seconds.
- **`FromReader::Said` is the one message in either direction that is
  not a read**, so it is capped, stripped of control characters and
  rate-limited. The security argument was written over `ToReader` and
  has to cover both directions: newlines in a line from the reader
  would otherwise reproduce whole log lines with no `reader:` prefix
  on them, in the one artefact every diagnosis here has turned on.

### Taking the two measurements

`smkvm clipboard-probe` is the instrument, hidden from `--help`
because it exists to answer one question and should be deleted when
that is answered. Run as the person it shows one observer. Run as the
system account it shows three, which is the point: the same code
looking the same way, three identities, one page.

Getting it to run as the system account, with somewhere for the
output to go, since a service account has no console:

```
schtasks /create /tn smkvm-probe /ru SYSTEM /sc once /st 00:00 /f \
  /tr "cmd /c \"C:\smkvm\smkvm.exe\" clipboard-probe > C:\smkvm\probe.txt 2>&1"
schtasks /run /tn smkvm-probe
schtasks /delete /tn smkvm-probe /f
```

Copy a **file** in Explorer first; a text copy proves nothing here,
because text was never the thing that went missing.

What the three rows mean, written down before the run rather than
after it:

| child as the user | impersonating thread | what it means |
| --- | --- | --- |
| sees the formats | either | B-minimal is viable; build it |
| sees the formats | sees them too | shape A is alive and cheaper; the person should get that choice first |
| bare | bare | both roads are shut and the finding is bigger than the plan -- stop |

### What absence looks like, decided before presence is written

Between logoff and logon there is no user to be, so there is no
clipboard helper. That is not an error path and must not be written
as one. The service keeps running, keeps the link and keeps the
consent-desktop reach; what it loses is the ability to say what this
machine has copied, because this machine has nobody to copy.

Three things follow, and they are design decisions rather than
details:

- **A read with no helper fails immediately and says why**, rather
  than waiting out a deadline. "Nobody is logged in here, so this
  machine has no clipboard to read" is an answer; three seconds of
  silence followed by a timeout is a bug report.
- **Nothing is announced.** The far machine is told nothing at all,
  which is different from being told this machine's clipboard is
  empty. Announcing emptiness would let a logoff wipe what another
  machine is offering.
- **On logon, the helper reports what is already on the clipboard
  before it reports any change.** Otherwise a copy made during the
  gap -- or before the service started -- is invisible until the
  person copies again, which reads as the feature not working.

The same three apply to a helper that has died and not yet been
restarted, which is why they are worth settling once rather than
twice.

### What to do next, in order

1. Confirm the `WTSQueryUserToken` + `CreateProcessAsUserW` path
   produces a process that sees the five formats. Cheap, and it
   de-risks both B shapes.
2. Test whether an impersonating system thread sees them. Cheap, and
   it either keeps shape A alive or closes it.
3. Only then choose, and only then scope.

Until one of those is done, the honest position is the one at the top
of this section: two arrangements exist, each does something the
other cannot, and a machine can be put on whichever matters more to
the person using it.

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
to start rather than a loop; whether the message
filter is what was stopping the render, which the next run settles
either way; then whether a paste lands inside the budget, and if not
whether the promise is renewed and the second paste is instant as
designed -- the offer, the ownership and the fetch are all proven on
the machine, and what remains untested is everything after the ask; whether a copy made while a consent prompt is up behaves as
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

**The paste that never asked, and the oldest rule in this program.**
Three builds in a row failed to paste in service mode, each for a
different reason, and the third produced a log with no render line of
any kind for any attempt -- while the clipboard listed the right
formats and named the worker as its owner. Two possibilities: the
handler is not being reached, or it is being reached and its words do
not survive. They had to be separated before anything else was
changed, and separating them is most of this entry.

The control is what settles it. **The server runs the same clipboard
code, with the same message-only window and the same delayed
rendering, under the scheduled task -- and it pastes correctly.** So
nothing about the window, the message loop or the render logic is
wrong in itself. What differs is only who the worker is: the system
account, at system integrity, rather than the person at medium.

The leading suspect is UIPI, met from the other side. It refuses
*input* injected into a window of higher integrity -- the fault this
whole branch began with, `SendInput` returning zero and Windows saying
nothing. It also refuses *messages sent to* a window of higher
integrity, and delayed rendering works by the system sending
`WM_RENDERFORMAT` to the clipboard's owner on behalf of whoever is
pasting. So the shape fits: the clipboard lists the formats, names us
as owner, and yields nothing, with no handler ever entered.

`ChangeWindowMessageFilterEx` is the documented way for a window to
accept a particular message from less privileged senders, and the
window now asks for the three a clipboard owner must receive:
`WM_RENDERFORMAT`, `WM_RENDERALLFORMATS`, `WM_DESTROYCLIPBOARD`. By
name rather than wholesale, because nothing else needs to reach a
window running as the system account.

**But the control cuts against the simple version of that story, and
it should be read as an open question rather than a settled
mechanism.** The server's daemon is not a medium-integrity process
either: its task runs at highest privileges and says so at every
start, `running high enough for every window ... level="high (run as
administrator)"`. So on the server a medium application pastes into a
**high** window and the render arrives; on the client a medium
application pastes into a **system** window and it does not. A plain
"lower may not send to higher" rule would have broken the server too,
and the server has been working throughout.

So one of these is true and it is not yet known which:

- the rule has a step in it that treats high and system differently,
  and the message filter is the right cure for the reason given;
- the message filter is the right cure for a different reason;
- or the block is not UIPI at all and something else correlates with
  running as the system account.

There is one thing a positive result *would* settle, and it is worth
being precise about it. `ChangeWindowMessageFilterEx` does nothing
except adjust UIPI filtering. If adding it makes the paste work, then
UIPI was blocking the message -- that much is confirmed. What stays
unconfirmed is the account above of *why* high needs no such call and
system does. Whoever reads this next will find the server sitting
there working and should not be puzzled by it: that difference is not
explained.

Two cheap observations were added rather than reasoned about, being
the lesson of the round before:

- the worker now reports the integrity level it actually runs at,
  measured with the call this program already had, instead of it being
  inferred from how the process was started;
- the window reports whether each message filter was applied, because
  a call that quietly succeeded and a call that was never made look
  identical afterwards.

And one lesson about the instrumentation itself, found by using it:
a prefix added by whoever *relays* a line is a prefix that is missing
on the path with no relay. Somebody reading a log at a machine with a
consent prompt in the way should not have to know which arrangement
produced it in order to search for the thing they need.

**This is a hypothesis with a control, not a certainty, so the run that
tests it also proves which half was wrong.** Independently of the fix,
the clipboard crate now says its handful of decisive lines through a
second channel: `witness_through`, which the worker points at the
pipe. The pipe is the one thing known to work in that process -- it
carries every keystroke -- so a line sent that way arrives whatever is
wrong with the worker's own logging. `WM_RENDERFORMAT entered` is
written as the first statement of the handler, before it looks at
anything.

**The worker held the clipboard open, and that takes it away from
everybody.** Found by measuring from inside the person's session
rather than by reading anything:

    OPEN BY window=5179950 pid=16280 name=smkvm
    OWNER   window=5179950 pid=16280 name=smkvm
    attempt 1: FAILED, error 5   (and again, and again)

Only one process in a session may have the clipboard open at a time.
While ours held it, *nothing* on that machine could copy or paste --
not the person's own applications, not `Get-Clipboard`, and not the
system on somebody's behalf, which is why no `WM_RENDERFORMAT` was
ever triggered and why a paste returned empty in about a second
instead of waiting. There is no timeout on this and no recovery but
the holder letting go or dying.

The cause was a log line. `take_clipboard` opens the clipboard,
promises the formats, and -- as of the round that added the owner
check -- said what it had done *before* closing. Saying something
reaches `witness`, which in the worker relays down the pipe, and that
write is blocking with no deadline on a pipe that also carries every
captured pointer movement. One write that did not return left the
clipboard open for ever.

That rule was written and obeyed, and the same blocking write came
back one round later in a form the rule did not cover. Closing the
clipboard first stops the damage spreading to the whole session, but
the relay could still stop the thread that was emitting the line --
and with it every statement after that line in the same function.
`witness` logs first and relays second, so a wedged relay leaves a
distinctive fingerprint: the line appears once in the local log,
never with the `worker:` prefix, and nothing that should have
followed it appears at all. That was exactly the state of the one
instrument built to diagnose this bug.

**A diagnostic that can stop the code it is diagnosing is not a
diagnostic.** The clipboard's witness now hands each line to a
bounded queue and returns; one thread drains it in order, and when
the queue is full the line is dropped and counted rather than made to
wait. Losing a line is a nuisance. Losing the rest of the function
reads like a different bug entirely, and cost a round on that basis.
The render path got the same treatment one round later, and for a
sharper reason. `WM_RENDERFORMAT` runs on the clipboard window's
thread with the pasting application stopped and a promise
outstanding, and it asked the service for the contents through the
same shared writer. A handler that does not come back has not merely
been slow: returning without setting data answers the question with
nothing, permanently, and Windows never asks again. So one contended
write left the person's clipboard empty until something restarted --
and it presented as "the render is never entered", because after the
first failure there were no more renders to enter.

**`secure::outbox`.** One place where a thread that must not wait
puts something for the service: `post` uses `try_send`, so it either
takes the message or refuses and counts the refusal, and a named
thread does the waiting where waiting is allowed. Refusals are
admitted in the log rather than silently swallowed, and if the
admission itself cannot be posted it is owed again. The tests are the
invariant: a full outbox refuses promptly, an undrained one says so
rather than hanging, order is preserved, and the count is handed over
once.

A caller that is refused takes the path that renews the promise, not
the path that answers nothing -- which is the difference between one
paste that does nothing and a clipboard that is empty for ever.

`AskTheService` deliberately holds no `Speak` any more. The surest
way to say that nothing reachable from a render may wait on another
thread is to not hand it the thing that can, and the compiler keeps
it that way: putting the field back makes it dead code, which is how
this was confirmed rather than assumed.

The worker's remaining writes -- the capture pump and the main loop
-- are still direct. Those threads may wait without answering a
promise on the way out.

So the rule, which is short and has no exceptions: **between an open
and a close, do nothing that can wait on anything.** Not a log line,
not a lock, not a channel, not a pipe. Gather what is needed, close,
then speak. `take_clipboard` now drops the guard by name before it
says a word, and then asks `GetOpenClipboardWindow` whether the
clipboard really is closed, saying so loudly if it is not -- because
that state costs everybody on the machine, not only us.

`smkvm status` reports it too, and for the same reason it re-measures
the key rather than trusting a log line: somebody whose copy and paste
has stopped should find out in one command whether we are the reason,
without reading a log or knowing this program has a worker.

**And a guard in an `if` condition is dropped before the block.**
Found by reading, while looking for the above. `if Opened::take(w).is_ok()
{ ... }` reads as "open it, and if that worked, do this" and does not
mean that: the condition of an `if` is a terminating scope, so the
guard is dropped -- closing the clipboard -- before the block runs.
Both places that did this ran their bodies with the clipboard closed,
which is where an earlier round's `ERROR_CLIPBOARD_NOT_OPEN` came
from. `if let Ok(_open) = ...` keeps it alive for the block, which is
what was meant. Worth knowing generally: this is the same footgun as
`if mutex.lock().unwrap().is_empty() { ... }`, which releases the lock
before the block it is guarding.

**A hang with nowhere to look, because only one end of the handler
said anything.** The data arrives instantly and correctly -- both hops
measured at zero milliseconds, seventeen bytes for a seventeen-byte
marker -- and then the log stops. No hand-over, no error, no renewal,
while the process goes on logging other things and the pasting
application never returns.

What made that hard to place is a fault in the instrumentation rather
than in the code. `WM_RENDERFORMAT entered` was said; nothing said
whether it came back. The absence of a further line was doing the work
of evidence, and it only worked at all because somebody could see the
log carry on with unrelated traffic. Both ends are said now, with the
outcome between them and how long it took:

    clipboard: WM_RENDERFORMAT entered for format 13
    clipboard: WM_RENDERFORMAT for format 13 returned after 2 ms: served it, 17 bytes

Reading the code in that gap ruled out the obvious candidates rather
than confirming one: `write_raw` has no loop, no `OpenClipboard` and
no blocking call -- which is correct for a render, since the requester
already holds the clipboard open -- so the hand-over itself cannot
hang. That leaves either something after the handler returns, or a
path not yet visible. The next run says which, and that is the point.

**And the renewal could run on the success path.** Whether to promise
again was a `bool` threaded through the body, set in three places, and
nothing prevented the path that *succeeded* from setting it. Renewing
a promise that was just kept is not a small mistake: it re-takes the
clipboard, which empties it, which throws away the data handed over a
moment earlier. It is now `Rendered`, returned by the render and acted
on by the caller, with `needs_renewing` beside the outcomes and tested
-- including the case that matters, that a kept promise is never made
again.

**UIPI was it, and the filter is the cure.** Under the service, with
the filter asked for, the handler is entered:

    clipboard: window is up: window=0x... thread=...
      messages-from-less-privileged[773=allowed 774=allowed 775=allowed]
    clipboard: WM_RENDERFORMAT entered for format 13

`ready: ... integrity=system` confirmed the premise at its root and
the owner comparison confirmed the promise is on our own window. Since
`ChangeWindowMessageFilterEx` adjusts nothing but UIPI filtering, the
message having arrived settles that UIPI was stopping it. Why a
high-integrity owner needs no such call and a system one does remains
unexplained; see the open question above, which stays open.

**Then a self-collision the X11 side had already solved.** The worker
owns the clipboard for the far machine's offer *and* watches the
clipboard for local copies, and it was reporting its own offer as
somebody else copying. What followed was a loop: the change is
announced, the service asks the worker to read what was "copied", the
worker asks the clipboard, the owner it is asking is itself mid-
promise, the read fails, the render fails, the promise is renewed --
and round again, for a whole run.

The mechanism to prevent it existed and could not work. A flag was set
when the clipboard was taken and cleared by the first notice that came
back. The comment immediately beneath it said, and had always said,
that *one copy arrives as several of these*: the first notice consumed
the flag and every one after it was taken for a foreign copy. Each of
those threw away `state.offer` **and `state.cache`** -- which is also
why the second paste was never instant, and why the cache promised in
the round before never helped once.

The fix is what X11 has always done: ask who owns the clipboard rather
than remember that it should be us. `GetClipboardOwner` against our own
window does not care how many notices one copy produces.

**The control was run, and it narrows this to one variable.** Same
machine, same binary, still under the scheduled task, the user's
clipboard never interrupted:

    PASTE UNDER THE TASK: [MARKER-121830-CTRL]
    INFO clipboard: WM_RENDERFORMAT entered for format 13

Three things stop being assumptions. The witness channel arrives. The
render handler runs and serves correctly, so nothing in the window,
the message loop, the promise or the render logic is wrong. And the
only variable left between working and not working is the arrangement
itself: the same code, on the same machine, in the same session, with
the same window -- as the person at high integrity under a task it
pastes, as the system account under a service the message never
arrives.

That is worth more than any of the three fixes that preceded it, and
it was available for two rounds before anybody ran it.

So the next run has two readings and not three:

Both of those have now happened: the control showed the handler works
when reached, and the service run showed the filter is what lets it be
reached. What the next run has to answer instead is where a paste's
time goes, and it is instrumented to say so rather than to be guessed
at:

- `clipboard: WM_RENDERFORMAT entered`, then `a paste waited on the
  service waited_ms=...`, then `the far machine answered a paste
  took_ms=...`, and now `WM_RENDERFORMAT for format 13 returned after
  N ms: ...`. Those bracket the whole cost. All four have been seen
  except the last, which is the one that says whether the handler
  comes back at all -- and if it does not appear, the hang is inside
  the handler and after the hand-over; if it appears and the paste
  still does not finish, the hang is somewhere after the handler
  returns and nothing in this file is the cause.
- `pasting what arrived too late for the last attempt` on a second
  paste is the cache doing what was promised. Its absence means the
  cache is still being cleared underneath, which the self-collision
  was doing.

Do not raise a budget without those numbers. A number raised to
accommodate a loop is a number that hides it.

Grep for `clipboard:` and it finds these lines on either arrangement.
The prefix is put on at the source for that reason; under the task
they arrive straight from `tracing`, and under the service they come
again through the pipe as `worker: clipboard: ...`, so the presence of
`worker:` additionally says which path carried it.

**Returning from a render without supplying data does not defer the
question -- it answers it, with nothing.** This is the single most
important thing in this file about the clipboard, and it was learnt by
making the previous fault worse.

Delayed rendering works by promising: `SetClipboardData(format, NULL)`
says "ask me when somebody pastes". When the ask comes as
`WM_RENDERFORMAT`, the handler must call `SetClipboardData` with real
data *before it returns*. A handler that returns without doing so has
not postponed anything. Windows records empty data for that format,
and **never asks again**. The clipboard then sits there listing the
right formats, owned by the right process, serving emptiness for ever.

So bounding the render -- which was the right fix for the sixty-second
hang -- turned a slow paste into a permanently broken one, because the
slow version at least got asked a second time. The signature on the
machine was unmistakable once looked for: `EnumClipboardFormats` showed
`CF_UNICODETEXT` and friends, `GetClipboardOwner` was the worker's own
process id, and `Get-Clipboard` returned nothing at all, instantly,
with no render line in the log for any attempt.

The cure is that a render which cannot supply the data **renews the
promise** rather than abandoning it: it posts itself a message and
re-takes the clipboard once the handler has returned and the clipboard
is closed again. The next paste is then asked for afresh, and by then
the contents that arrived too late are in hand, so it is instant. After
three renewals with nothing ever served, the clipboard is released
altogether -- because holding a promise that cannot be kept is worse
for the person than holding nothing: their own last copy is gone *and*
every paste yields emptiness. Giving it back at least makes the machine
work on its own again.

**How to tell any of this from outside.** Two checks, neither
replaceable by a log line, both run inside the person's session through
a throwaway scheduled task as their account:

- `Get-Clipboard` -- the verdict on whether a paste *works*.
- `EnumClipboardFormats` plus `GetClipboardOwner` -- the verdict on
  whether the offer was *made*, and comparing that owner's process id
  against the worker's is what proves the delegation itself is sound.

The difference between them is the difference between the three
clipboard faults in this file, and the log could not distinguish them
in any of the three rounds.

**The paste that took sixty seconds and then produced nothing.** With
the worker's voice in place this was one deployment to isolate, with a
control: a marker string held on another machine's clipboard, read
inside the logged-in session on each Windows machine through a
throwaway scheduled task running as that account. The server, on the
scheduled task, returned the marker. The client, on the service,
returned nothing. So the announcement, the network fetch and the far
machine's supply were all fine and the break was inside the worker's
paste path alone.

The log then said it exactly: `something on this desktop is pasting
Text` at 02:41:17, and at 02:42:17 `could not put the data on the
clipboard: ... <clipboard not open>`. Sixty seconds, then a *successful*
fetch handed to a clipboard that had been closed for most of a minute.

It was not a deadlock, which was the obvious guess and was checked
first: the clipboard's message loop and the pipe reader are genuinely
separate threads in the worker, and the lock around the served
clipboard is not held across a render. It was arithmetic. The worker
waited thirty seconds and the service's own patience was also thirty,
so when a fetch was slow the worker gave up at the exact moment the
answer might arrive; the answer was discarded as belonging to nobody;
`WM_RENDERALLFORMATS` asked again; that second attempt queued behind
the first fetch, which still held the fetch lock, for another thirty
seconds; and what it finally got was written into a clipboard that had
long since closed.

Three rules came out of it, and they are in `secure::budget` with
tests that run on any machine, which matters because the code they
govern runs on exactly one:

- **Every inner wait expires before the wait outside it**, with
  headroom. The moment an inner deadline outlives an outer one, an
  answer arrives for a question nobody is waiting on and the next
  attempt queues behind work already abandoned. The ladder is now
  1.8 s for the far machine, 2.5 s for the worker, 4 s for the render.
- **A deadline that cannot be met is not made longer.** A fetch too
  slow for a paste is given up on, and what it returns is *kept* for
  the next paste. The worst case is one paste that does nothing and a
  second that is instant, which a person can understand and work with.
- **A late answer is never written.** The render checks the clock
  before handing anything over, so data that arrived after the
  clipboard closed is cached rather than pushed into
  `ERROR_CLIPBOARD_NOT_OPEN`.

And nothing queues: a second paste arriving while one is in flight is
refused at once rather than waiting out the first.

**System error messages must carry their number, and be decoded as
wide text.** The error above reached the log as
`?ㅻ젅?쒖뿉 ?대젮 ?덈뒗 ?대┰蹂대뱶媛 ?놁뒿?덈떎` -- the machine's ANSI code page
written into a UTF-8 file. These machines are Korean, so *every* system
message this program surfaces has this problem, and mojibake is both
unreadable and unsearchable. `FormatMessageW` is now called explicitly
rather than left to anything that might reach for the ANSI form, and
the numeric code is always appended, because a number can be looked up
whatever happened to the text.

**A file with a line break in its name was refused for a week, and that
was most of "copy and paste does not work".** A journal PDF downloaded
with its title wrapped across two lines --

    Oracle-Guided Reinforcement Learning for\nDegradation-Aware ... .pdf

-- was abandoned on every attempt, with the only trace a warning in the
log of the machine doing the *sending*, which is the one place nobody
looks. It predates the service by a week and has nothing to do with it.

The rule was right about danger and wrong about what to do with it, and
the fix is to separate the two kinds by what is at stake:

- **Refused**: names that could land somewhere other than the directory
  they were meant for -- `..`, a leading `/`, a backslash, a NUL, a
  colon, anything `Path::components` does not make exactly one ordinary
  component of. Nothing about such a name can be trusted, so it is not
  tidied up and used anyway.
- **Cleaned**: names that are merely not writable as they stand --
  control characters, the characters Windows reserves, trailing dots
  and spaces, reserved device names, too long. The file arrives under
  the closest name that can be written, and the change is said once in
  the log. Losing a newline out of a filename is not a loss worth
  refusing a file for.

The colon is the interesting one, and it moved *into* the refusal list
while writing this. It looks like an awkward character and is not: on
Windows it means a drive or an alternate data stream. Cleaning it to
`_` would have turned `C:` into the perfectly ordinary-looking `C_` --
a dangerous name made to look safe, which is precisely how this change
could have made things worse. The existing escape test caught it, which
is the second time that test has earned its keep.

A refusal is now also loud on the machine *receiving* the files, not
only in the reply to the sender. A paste where nothing happens and
nothing anywhere says why is the shape of fault this project keeps
paying for.

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

**Holding a system-wide singleton is not like holding a lock of your
own.** The Windows clipboard can be open by one process at a time, for
everybody. A window left in there is not a bug this program suffers,
it is a bug this program *inflicts* -- the machine's copy and paste
stops and stays stopped, with no error anybody sees and no way out but
killing the holder. Anything with that shape deserves the strictest
possible rule about what may run while it is held, a guard whose drop
releases it, and a check afterwards that it really was released.

**Logging is not free, and it is not safe everywhere.** Three rounds
were spent adding places where this program says what it is doing,
which was right; this round one of those lines took the machine's
clipboard away. A log line that relays over a pipe is a blocking
write, and a blocking write inside a critical region is a deadlock
waiting for a slow reader. Say things where waiting is allowed.

**A success path that shares code with a failure path needs a test
that it does not do the failure path's work.** The renewal was written
for the case where the contents do not arrive, and lived in the same
function as the case where they do, joined by a mutable flag. Nothing
in the type system, the tests or the review stopped the good path
taking the bad path's action -- and that action was the exact opposite
of what had just been achieved. Where two paths share a function and
only one should act, return what happened and let the caller decide;
then the decision is one function with a name, and it can be tested
without a machine.

**Log the thing that worked, not only the things that did not.** A
handler that says "entered" and nothing else leaves a hang
indistinguishable from a silence, and reading it requires noticing
that *other* traffic continued. One line saying `served it, 17 bytes`
would have placed this in a second. Every exit says so now, success
included, and the entry line is paired with a return line carrying the
elapsed time.

**A guard that remembers cannot count, and one copy is several
notices.** The self-change guard was a boolean set on taking the
clipboard and cleared by the next notice. Windows sends several
notices for one copy -- the code's own comment said so, two lines
below the guard -- so every notice after the first was read as a
foreign copy. Where the question is "did I cause this", ask the system
who did it rather than remembering that you meant to; the answer does
not depend on how many times you are asked.

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

**UIPI is a rule about messages, not only about input.** It was met
first as injected keystrokes being refused; it also governs what a
window will accept from a less privileged sender, which is why a
clipboard owner may need `ChangeWindowMessageFilterEx` to be pasted
from at all. Note that the threshold is *not* simply "any higher
integrity level": a high-integrity owner on the server is pasted into
happily by medium-integrity applications without asking for anything.
Where exactly the step falls is not known here. Anything the system does *on behalf
of* another process by sending a window a message is subject to it:
rendering a clipboard format, drag and drop, and more. Running as the
system account buys reach in one direction and costs it in the other,
and the cost is as silent as the original was. Where a window must
hear from ordinary applications, say so with
`ChangeWindowMessageFilterEx` and name the messages.

**Two processes, one log, and the question of who said what.** The
worker writes to the same file as the service and also relays through
the pipe with a `worker:` prefix. A line without that prefix is the
worker's own logging, a line with it came down the pipe, and which of
the two appears is itself evidence -- in one round the relayed lines
arrived and the local ones did not. When a component that is not the
worker needs to be heard from, give it the pipe too rather than
trusting the file; `smkvm_clipboard::witness_through` is that, and it
exists because the file could not be trusted.

**Giving up faster is not automatically safer.** Bounding the render
was correct and made things worse, because the thing being bounded had
a side effect on being abandoned: the promise was consumed. Before
shortening any wait, ask what the caller does with the abandonment --
if it records the absence as an answer, a fast failure is a permanent
one and the slow version was doing something useful by still being
there to be asked again.

**Nested deadlines must shrink inwards, and equal is as bad as
inverted.** Two hops each given thirty seconds is not "thirty seconds
of patience", it is sixty and a discarded answer in the middle. Where
one wait contains another, the inner must expire first with room to
spare, and the arithmetic deserves a test of its own -- especially
where, as here, the code cannot be run on the machine that writes it.

**A slow answer is worth keeping even when it is too late to use.**
Throwing away a fetch that missed its render means the next paste pays
the same cost and misses again. Keeping it turns a permanent failure
into a single retry, and that is the difference between a feature that
looks broken and one that looks slow the first time.

**Refusing is not the safe default; it is a different risk.** A check
that refuses everything doubtful looks conservative and reads as
careful, and for a week it silently stopped a person transferring their
own files. Danger and awkwardness are different questions and deserve
different answers: ask what is actually at stake if the input is
hostile, and if the answer is "the name would look odd", clean it and
carry on. If the answer is "it might not land where it was meant to",
refuse. When separating the two, check that nothing in the cleaning can
produce something the refusal would have caught -- that is the way the
change goes wrong.

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

**The worker is a whole process, and it had no voice.** It injected
every keystroke perfectly -- so the pipe demonstrably worked -- and
wrote not one line to the log file across three restarts, which made it
impossible to tell a clipboard that failed to start from a clipboard
that was never asked for. Why its own logging was silent is still not
known. What is fixed is the dependence on it: anything worth knowing
now goes *both* into the worker's log and down the pipe as
`FromWorker::Said`, which the service writes with a `worker:` prefix.
Which of the two arrives is itself a diagnosis. The worker also says an
inventory at startup -- desktop, whether it has capture, where it
thinks its log is -- and is started with `--unattended` so it never
decides where to log by asking whether stderr looks like a terminal.

**How to check the clipboard, which is not by reading a log.** Put a
marker on another machine's clipboard, hold it, and read the clipboard
*inside the person's session*: a throwaway scheduled task running as
that account at `LeastPrivilege`, doing `Get-Clipboard`. Anything else
-- including everything in the service's log -- tells you only that an
offer was made, not that it reached the person.

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

**Switching the arrangement does not stop the daemon the old one
started.** `install --system` removed the login task and the task's
daemon carried on running, so a machine briefly had the old daemon,
the new service and its worker -- two daemons over one clipboard,
which is the thing the trap below is about and which `secure::plan`
was written to make impossible.

It was defeated by looking in the wrong profile. `running_pid` reads
the status report, a daemon writes that report into the profile of
whoever runs it, and an install happens over ssh as an administrator
who is not that person. The report was not in any place the installer
could look, so it found nothing and stopped nothing. Same root as the
configuration and the transfer directory: *the installing account is
not the account the machine belongs to*, and anything that reads a
per-user place during an install is asking the wrong person.

The process list needs nobody's profile, so the switch now asks it.
What counts as a daemon is `plan::is_a_daemon`, which is pure and
tested -- and it has to be, because the installer is itself an
`smkvm.exe` and stopping everything by name would have it stop itself
halfway through.

**Two daemons on one machine.** Before the guard in `smkvm serve`/`connect`,
starting a second `smkvm connect` left the first attached and forgotten,
holding whatever it did last. The guard reads `status.toml`; if a daemon was
killed hard, its report goes stale after 15 s and the next start is allowed.

**BLAKE3 and MASM.** Its assembly needs an assembler that does not exist when
cross-compiling from Linux, hence the `pure` feature. Anything else pulling in
assembly will need the same treatment.

**`SetClipboardData` returns null on success, and Rust cannot say so.**
The call returns the handle it was given. A delayed-render promise
gives it null, so a successful promise returns null -- and
`windows-rs` maps a null return to `Err(Error::from_win32())`, which
carries whatever `GetLastError` happened to be holding from some
earlier, unrelated call. The first version of the read-back below
printed that as `REFUSED (error 6)` on every promise ever made, next
to `listed` in the same line, and the contradiction was read as a real
refusal rather than as a broken diagnostic. The error is cleared with
`SetLastError(0)` before the call now, so a null return with the error
still clear is read as what it is. `promise_said` is a pure function
with tests because getting this wrong once already cost a round.

The general form is worth keeping: **a diagnostic that can only say
"failed" will say it, and be believed.** Two independent readings of
the same fact -- the call's result and `IsClipboardFormatAvailable` --
are what caught this, and when they disagree the line now says so
instead of picking one.

**Ask the system what it is holding, and never with `GetClipboardData`
from the owner.** The promise at the start of the delayed-rendering
chain is `SetClipboardData(format, NULL)`, and its result was thrown
away for the whole life of this code. Everything downstream -- the
format missing, the format listed but empty, a paste answered with
nothing -- looks like a fault somewhere else when the first link is
unexamined. It is checked and logged now, along with
`IsClipboardFormatAvailable` for the same format, so the log says what
the system believes rather than what the code intended.

The read-back stops there deliberately. `GetClipboardData` on a format
this process has promised makes Windows send *this thread*
`WM_RENDERFORMAT`, synchronously, from inside the handler still making
the promise -- and a render that returns without calling
`SetClipboardData` has answered the question with nothing, for good.
Reading the promise that way destroys it. What `GetClipboardData`
returns is the most valuable of the four values, and it can only be
asked by a process that is pasting.

**A single sample is not a measurement.** `smkvm status` caught
Windows Defender's session helper holding the clipboard for an instant
-- perfectly normal, every copy and paste does it -- and announced
that nothing in the session could copy or paste. True of that instant,
false of the situation, and alarming. It samples five times over a
second now, and only a holder present in every sample gets the
verdict; a holder seen in some of them is named and called normal. A
diagnostic exists to be trusted, so an alarming sentence about an
ordinary event is worse than no sentence at all.

**`smkvm status` prints the four values.** Who owns the clipboard,
which formats it lists, whether anything is holding it open, and what
a paste returns for each format with how long it took. Those four
settle every argument this bug has produced, and before this they came
from four different places and a guess. Running it performs a real
paste, which the output says.

**Everything that can write over a format announces itself.** There
are exactly three: the promise in `take_clipboard`, the renewal that
re-takes the clipboard through `WM_RENEW`, and `WM_RENDERALLFORMATS`,
which turns every promise into real data at once. The last of these
collected its outcomes into a list and then never said them -- the one
message able to quietly replace a promise was also the only one that
reported nothing.

**The offer travels in two halves.** `WindowsHandle::offer` puts the
formats and the place to fetch them on a channel, and posts a thread
message telling the window thread to look. Neither half is any use
without the other, they are sent from a different thread than the one
that reads them, and the state they land in is a thread-local
reachable only from the window's own thread. That is the only place
in the Windows clipboard where one act is split across two
mechanisms, and it is the first thing to suspect when the clipboard
promises something nothing can supply.

So both ends say what they saw, with contents: handing over, taking
it up (naming what it replaced), taking the clipboard to promise it,
and being asked to produce a format with nothing recorded to fetch.
The two places that ever let an offer go -- somebody else copying,
and giving the clipboard back -- each name what they let go of.
`announced` and `nothing is on offer` were two statements about the
same value that never once appeared together with that value in
them, which is why three rounds of this were guesswork.

**A count that can outlive what it counts will be applied to
something else.** The renewal count sat in `State` beside the offer,
and was reset only by a render that succeeded. Once a promise had
been given up on, the count stayed above its limit for the life of
the window, so the *next* offer would be released after its first
unsuccessful render -- one clipboard that could not be served
poisoning every clipboard after it. It lives inside `Offer` now.
Letting the offer go takes the count with it, and a new offer starts
from zero because it is a new value; there is no longer a way to
write the bug. (This was found while adding the lines above, and is
not the fault those lines were added to find: `NothingOffered` does
not renew, so this path was not the one firing.)

**One thread in this process has a COM apartment, and nothing else
runs on it.** A file copy arrives as an OLE data object. A process in
the session sees `CF_HDROP` beside it and can fetch the file list;
the worker, as the system account, on the same desktop, at the same
instant, enumerated the data object alone -- eight times across two
seconds. Whether that is the account or the missing apartment could
not be settled from outside, because everything available to run in
that session initialises COM whether asked to or not.

So `ole_thread` is both the experiment and, if the evidence holds,
the fix. It calls `OleInitialize` and looks in the two ways that can
disagree: plain `EnumClipboardFormats`, which is what the window
thread already does and differs only in having an apartment behind
it, and `IDataObject::EnumFormatEtc`, which asks the data object what
it can supply rather than what is registered. Both answers are
logged. First list full: an apartment was all that was wanted. Only
the second full: the formats live in the object and never on the
clipboard. Both bare: it is the account, and no amount of COM will
help.

It is a thread of its own because everything it does can wait.
`OleGetClipboard` talks to the process that did the copying and
`GetData` may make it produce the data, and three separate faults in
this file came from giving the clipboard window's thread something
that can wait. The window thread posts a question and never looks
back; the apartment thread reports a copy down the same channel the
settle timer would have used.

Reading has the same cause and the same cure, which is the strongest
single argument for the apartment: a process that cannot *see*
`CF_HDROP` cannot *fetch* it either, and `GetClipboardData` returning
nothing for an image is the same symptom from the other end. But the
read tries the calling thread **first** and only then the apartment.
The scheduled task reads correctly on three machines today, and a
change that routes a working read through a new thread to fix a
different arrangement can only lose. A second chance after a failure
is never a detour before one.

**A file copy does not arrive on the clipboard all at once.** The
shell puts its OLE data object there first, and the standard formats
that object stands for -- `CF_HDROP` among them -- appear around it
afterwards. The settle timer looked once, sixty milliseconds after
the first notice, and found a clipboard holding nothing but
`49161=DataObject`; a probe in the same session, two seconds later,
found five formats including `15`. One look cannot tell a copy still
being assembled from a copy of something we do not share.

So it looks again: up to eight times, a quarter-second apart, giving
up after two seconds. The first look ends it whenever the copy is
already complete, so the common case costs nothing. Giving up reports
*both* enumerations -- what was there at the first look and at the
last -- because which of them is bare is the diagnosis. A full second
one means the first was early. Two bare ones mean this process cannot
see what a process in the session can, which would be about OLE and
not about timing, and the enumeration itself would have to change.
Finding the formats after more than one look says so too, since
whether this re-looking is load-bearing is worth knowing.

**There is one `available_now`, not two.** It is tempting, when the
scheduled task works and the service does not, to look for the list
the service got wrong. For clipboard formats there is no such list:
`available_now` is the only place that decides which formats are
worth telling another machine about, and both arrangements call it --
the settle timer calls it in whichever process holds the clipboard
window, and `WindowsClipboard::available` calls it for a poll. A
difference in behaviour between the two arrangements therefore cannot
be a difference in that list, and reasoning that starts "the worker's
set must be a subset" starts from something that does not exist.

What the list *was* missing, independently: an image offered only as
`CF_DIBV5` or only as `CF_BITMAP` was not noticed at all. Windows
synthesises the others from `CF_BITMAP` only once something asks, so
`IsClipboardFormatAvailable(CF_DIB)` is false until then. Both are
checked now.

**A diagnostic that reports a mismatch must name both sides of it.**
"None of the formats we share were on it" is true, and it cost a
round, because settling what was actually there took somebody sitting
in the session with a Win32 enumerator. It now lists every format on
the clipboard with the name Windows has for it, and the identifiers
it was looking for. The same class of slip in any other list is a
single line from now on.

**Line continuations written by a script.** Several messages shipped
with long runs of spaces inside them, because a `\` at the end of a
line inside a *Python* string is Python's own continuation: it eats
the newline and bakes the Rust indentation into the literal. Thirteen
of them. Rust needs the backslash to survive into the source, which
means a raw string or a doubled backslash on the way. `cargo fmt`
cannot see it, clippy cannot see it, and the only symptom is an ugly
log line -- so it is worth a grep for `"[^"]*[a-z,.)]   +[a-z]` after
any scripted edit, allowing for the handful of deliberately aligned
columns in `smkvm status`.

**The outbound half had no instrumentation at all.** Four rounds
were spent making the inbound path -- another machine's copy arriving
here -- say what it was doing, and in all that time the path by which
*this* machine's copies reach anywhere else said nothing whatsoever.
Not one line between `WM_CLIPBOARDUPDATE` noticing a copy and the
service being told about it, and three places in that span could fail
silently: the settle timer finding no state, the clipboard refusing
to open so the formats could not be read, and the send going into a
channel whose receiver had gone. The last of those means no copy made
on the machine will ever reach another one again, and it was a
discarded `Result`.

It now says what it found at each step, and the worker's "something
was copied on this desktop" moved from debug to info -- a person
copying something happens at human pace, and that line was the one
that would have shown this half was not running. The service warns
rather than silently discarding when the worker reports a copy and
nothing on this side is listening.

The general shape is worth naming: **instrumenting one direction of a
two-way path makes the other direction look like a missing feature.**
Inbound worked and was legible; outbound was broken and silent; and
the conclusion drawn was that the outbound half had never been
written.

**Diagnostics became the problem they were added to solve.** A 62 MB
log on the client, eighty-five identical render lines inside one
second, and bursts where the worker's outbox reported discarding
229,897 messages at once. Three separate costs: real work on a thread
that must not be slowed, an outbox spending its whole budget on
repetition, and -- worst -- a reader who cannot find anything, which
produced a wrong conclusion that cost a round.

So `witness` splits in two. `witness` is for things that happen once
per human action: an offer arriving, an offer being let go with its
reason, a render that could not be served, a copy noticed here. Those
stay at info. `step` is for things that happen once per machine
action -- entering a render, promising a format, taking the clipboard
-- and goes to debug. The level crosses the pipe with the line, so a
step does not arrive at the service as something a person is meant to
wade through.

Both go through `Throttle`, keyed on the text: the first couple of
identical lines in each second get through and the rest are counted,
and the count is handed to the next one that does, exactly as the
outbox does with messages it had to refuse. A spin now reads as
`[and 83 more like it in the last second]` instead of as a megabyte,
which also makes the rate of a spin a measurement rather than
something to be counted by hand.

**Both ends of both directions say what they saw.** A paste crosses
four boundaries: the worker posts `WantsPaste`, the service receives
it, the service asks the far machine, and the answer comes back to
the worker as `Pasted`. For two rounds the only evidence about that
whole span was silence, and silence has been wrong three times in
this file -- once it was a filtered message, once a wedged relay,
once a blocking write. Each of the four now says so, with the paste's
id, and the worker says whether anything was still waiting when the
answer arrived, because an answer with no asker is a fault this code
has had before.

The service's line goes *before* its `fetch`, not only after. That
`fetch` has no deadline of its own -- `SERVICE_FETCH_WITHIN` is
measured against it, not imposed on it -- so a far machine that never
answers leaves a thread waiting for ever and the only record of the
attempt is a line that never comes. That is the exact shape that cost
three rounds on the render.

**The outbox drain does not exit on a failed write.** It used to
return, which drops the receiver -- and since the outbox is installed
once per process, every later message including a paste request would
be refused for the life of the worker on the strength of one failed
write. It stays and says so instead, once when it starts failing and
once when it recovers, through `tracing` rather than through the
outbox, which would be the line trying to report its own failure to
be reported.

**A fix that lives as a type is only applied by somebody remembering
the type exists.** `Aligned` was added after a review found an
unaligned `TOKEN_USER` read, applied at the two call sites that
existed -- and then the same mistake was written from scratch in a
third, `token::whoami`, while the type sat a few hundred lines away
being used correctly. `vec![0u8; n]` compiles, passes every test and
is undefined behaviour. The note on `Aligned` now says in the
imperative that every `TOKEN_*` and `QueryServiceConfigW` read goes
through it, and shows both the right shape and the wrong one, which
is the most that can be done short of making the wrong form
impossible to write.

**A limit should count only the thing it is named for, and say what
that thing was.** `renewed 4 of 3 times` is a limit working
perfectly, and it tells nobody what it was working against. The count
is `failures` now -- renders that could not be served, nothing else
-- and it carries `last_failure` beside it, so giving up says which
failure kept happening rather than only that a number was reached.
The count on its own could not distinguish "the far machine never
answered" from "the offer was withdrawn"; with the reason attached,
one line ends the question.

**Every release says why.** Giving the clipboard back is the act that
ends a person's ability to paste what the other machine copied, and
there are three quite different reasons for it: the offer was
withdrawn, renders kept failing, or the offer was gone before a
render finished. Two of those mean something is wrong here and one is
ordinary housekeeping, and a single line saying "giving the clipboard
back" cannot be read as either. The reason travels in `WM_RELEASE`'s
`wparam` -- which the message loop has to carry across by hand,
because a thread message has no window to be dispatched to, and
dropping it made every release look ordinary.

**Every take says who asked for it.** There are two callers and they
mean opposite things: a fresh announcement is the far machine
offering something new, and a renewal is this machine failing to
supply something old. Four takes with one announcement is a diagnosis
on its own, and only if the log distinguishes them.

**An absence must never be asked to carry evidence, and that
includes the instruments.** The round that was meant to separate
"posted and not delivered" from "never posted" added tracing only to
the outbox's *failure* paths -- so a healthy drain and an absent one
both produced a file with no outbox line in it, and the question
stayed exactly where it was. Every post is now traced at the moment
of posting with what became of it, and the drain marks its first few
sends and every fiftieth after. A working path has to leave a mark,
or its silence cannot be told from its absence. This is the third
time that sentence has been the finding.

Two places in the reader could also end a thread in silence: the
watch and the sequence poll each did `if !post { return; }`, and the
`return` jumped over the line that would have said so. And when one
of them won the race to announce a copy, the other `continue`d
without a word, so "this path works and deferred to the other" was
indistinguishable from "this path never ran". Both say so now.

**A zero that means two things will be read as the wrong one.**
`EnumClipboardFormats` answers zero both for "that was the last one"
and for "that did not work", and the two are told apart only by the
error left behind -- clear for the end of the list. Read naively it
reports every failure as an empty clipboard, which is how `the
clipboard now holds []` came to be printed one line above a list of
formats something else had just found on the same clipboard.

`ERROR_CLIPBOARD_NOT_OPEN` is what it was hiding: the enumeration
runs with the clipboard not open on that thread, every time, while
`IsClipboardFormatAvailable` on the line above needs no open
clipboard and answers correctly. That is a real fault, found only
because the diagnostic stopped lying about it.

That is the second call in this file with that shape. The first was
`SetClipboardData`, whose null return meant both "promised" and
"refused", and which reported every promise it ever made as a
refusal until the error was cleared before the call. **Any Win32
call whose failure value is also a legitimate answer needs
`SetLastError(0)` before it.** There will be a third.

**Every way out of a process has to be written down.** The reader's
ordinary end logged through `tracing`, to a file in the person's
profile; an error returned to `main` and was printed to a console
that does not exist; a panic unwound past both. So it could vanish a
second after saying it had something to report and leave no account
of why -- which it did. Its outcome, including a panic, now goes to
the trace file, and so does the end of its outbox and of its watch.
The service, for its part, says when a reader stops talking, with
its exit code and with whatever it managed to write for itself: the
half that supervises should notice, and should say what it noticed.

**The same wedge, in the newest process, put there by the fix for
the last one.** The reader's writer was a shared `Mutex<Pipe>` and
an unbounded `write_all` -- the pattern `secure::outbox` exists to
end, and which had already cost the worker three rounds. It was
harmless in the reader until its clipboard lines were relayed down
the pipe, and then the *window thread* began calling it from inside
`WM_CLIPBOARDUPDATE`, which is the one thread in that process that
may never wait.

The symptom was exact and worth keeping, because it will recur in
some other shape: the first notification arrived and the handler
said so; the write did not come back; after that there was no
settle, no second notification -- the message was posted and never
collected, so the counter stayed at one -- and nothing from the
sequence watch either, because its first line queued behind the same
lock. Two independent paths stopping together is a shared
dependency, and the dependency was the act of speaking.

The rule the outbox encodes is worth saying without reference to any
particular thread: **a process that has one writer and more than one
thread needs the writing to be somebody's whole job.** Adding a new
line to an existing logger is enough to break it, which is what
happened here -- the change that caused this was one call added to a
function that had been fine for a week.

**One copy, two ways of noticing it, each saying which it was.** The
reader's window registers as a format listener, the registration is
accepted, the window pumps -- and `WM_CLIPBOARDUPDATE` never arrives
there, while the worker's window in the same session on the same
clipboard is told about every copy. The two processes differ in the
identity they run as and in nothing else anybody has found: the same
axis as the finding this arrangement exists for, moved from the
enumeration to the notification.

So the reader also watches `GetClipboardSequenceNumber`, which needs
no window, no message queue and nobody's permission, and moves on
every change. Running both is the measurement -- whichever notices
says so, in the line -- and it is equally the way out, because a
copy noticed by polling is as good as one announced. Whichever gets
there first claims the sequence number, so one copy is one notice
however many ways it was spotted.

The window also counts what it is sent and says the count. "No
message arrived" and "a message arrived and the handler did nothing"
shared a symptom for a round, and an absence cannot be told from a
dead instrument by looking at the absence.

**`witness_through` is one slot per process, and only the worker
ever filled it.** Every line the clipboard crate produced inside the
*reader* -- the settle timer noticing a copy, the clipboard refusing
to open, which formats it saw -- went to that process's own log and
nowhere the service could read. The half whose entire purpose is
noticing copies was the half whose noticing could not be read from
the side that cares, which is most of why "the reader said nothing"
was true of the only place anybody was looking.

**An ordinary account cannot open a service's token.** The reader
connects by `pipe::connect`, which checks that the process serving
the pipe runs as the system account -- by opening that process and
reading its token. The worker may do that; it is the system account
itself. The reader is the person, and `OpenProcessToken` on a
service's process is refused for an ordinary account. So the reader
failed the check it made *before saying a word*, returned an error
out of `main`, and exited 1 with no log, no pipe traffic and nothing
written anywhere -- while the service, whose pipe it had already
opened, saw only that the pipe had broken.

The check it needs is `Pipe::owned_by_the_system`, which asks the
*object* rather than the process. The pipe's descriptor says `O:SY`,
reading an owner needs only `READ_CONTROL`, and `GENERIC_READ` --
which the reader's own access control entry grants -- includes it.
The guarantee is if anything stronger: a pipe owned by the system
account was created by it, and nothing running as the person can
make one that is. `connect` and `connect_as_the_person` differ in
exactly this and in nothing else.

The general shape is one to watch for: **a guard written for one
caller, inherited by another with less privilege, fails closed and
silently at the earliest possible moment.** It was the right guard
in the right place, and it was the first thing the new process did.

**`STILL_ACTIVE` is 259, and a process that exits with 259 cannot be
told from a running one through `GetExitCodeProcess`.** Nor can a
*failure* of that call, if its error is folded into the same answer.
Both were, and the service reported "the reader was still running"
about a process that was not in the process list at all -- a
diagnostic asserting something it had not established, which is the
most expensive kind of line this codebase produces. `Started::how_it_ended`
decides by waiting on the handle, which is authoritative, and reads
the code only once the wait says it has gone. Not yet, a code, and
could-not-ask are three answers, not one.

**Once a reader exists, the worker's answer about the clipboard is
noise that looks like data.** The worker sees one format where the
person sees five; that is the finding the whole arrangement exists
for. So the service uses the reader's answer and drops the
worker's -- at debug, not in silence, because the two disagreeing is
worth being able to look at deliberately.

What has *not* been done, and is the standing hypothesis rather than
a fact: the worker's clipboard window is still a format listener, so
two settle timers still run against one clipboard in one session,
and each opens it -- the worker up to eight times over 1750 ms per
copy. That may be what stops the reader's open. Removing it means a
way to stop watching without stopping offering, which is a protocol
message and a change in the clipboard crate, and it is not worth
doing on a hypothesis when the relay above will say in one run
whether the reader's timer fires and fails or never fires at all.

**A process that cannot be trusted to speak gets a file handed to
it.** The reader's log depends on profile paths and a subscriber;
its voice down the pipe depends on the pipe; and for three rounds
every one of its words was lost. So the service works out an
absolute path inside the person's own profile -- proven writable by
that account, since the probe's child wrote there -- deletes
whatever is at it, and passes it on the command line. The reader
appends a line before the log, before the connect, before anything
that has ever failed, opening and closing per line so a death in the
next instruction still leaves what it had said. The service reads it
back into its own log when a start fails, because a file nobody is
told about is a file nobody reads.

It is temporary and it is meant to be. When the reader is reliable
it goes.

**Say the pid at creation.** The service knew it and never printed
it, so telling "died instantly" from "never existed" took fourteen
samples of a process list taken by hand. One line, at the only
moment the number is certainly true.

**A process whose first act can kill it silently cannot be
diagnosed.** The reader has no console, no terminal and a parent that
can only see a pipe, and `start_logging` was the first thing it did.
When that failed it returned from `main` with nowhere to say so. For
the reader alone, a log that cannot be opened is now carried rather
than raised: the complaint goes down the pipe in `FromReader::Ready`,
alongside who it is running as, its session, and the path it is
logging to. Saying where you are has to come before anything that
could stop you saying it.

The service reports the child's **exit code** when a handshake fails,
taken before the kill because afterwards the code is ours. A broken
pipe says only that a process is not there.

**A line that must exist is said by a drop, not by a statement.**
`WM_RENDERFORMAT for format N returned after ... ms` is the most
important line in the Windows clipboard, and across several rounds it
never once appeared. The reason was different every time -- a
filtered message, a wedged relay, a blocking write -- and identical
in shape: the statement that would have said it was never reached. It
is emitted from `Returning`'s `Drop` now, which also covers unwinding
and says plainly that the handler did not come back when no outcome
was recorded. A statement can be skipped; a drop cannot.

**Measure from inside the session.** Nine faults in the clipboard
path so far. All but one were found by asking the running system a
question from the session it was in, rather than by reading the code
or the log. The exception is instructive in the same direction: it
was a diagnostic line that could only ever say "failed", believed
because it was the only thing speaking. The pattern is consistent
enough to be a rule: when a theory and a measurement are both
available, take the measurement -- and give the measurement two ways
to disagree with itself.

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
