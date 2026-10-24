# pathwatch behavior

This document specifies the observable behavior of `pathwatch`: its API, the events it reports,
its options, the guarantees it makes, the cases it handles wrongly, and how soon events arrive.
The README gives a short overview, `design.md` states the rules behind this behavior, and
`architecture.md` describes the implementation.

## Terms

- **Watched path.** A path passed to `watch` and not yet passed to `unwatch`. It can name a file,
  a directory, or nothing.
- **Handler.** The closure passed to `Watcher::new` or `Watcher::with_config`. It receives every
  batch and every error.
- **Batch.** The events passed to one call of the handler.
- **Baseline.** What exists at a watched path when its `watch` call returns. It is not reported.
- **Known files.** The files that the handler has been told about: the baselines, plus every
  `Create`, minus every `Delete`.
- **Disk check.** The step in which the watcher reads the disk at the places that changed and
  compares the result with what it knows.
- **Quiet time.** The debounce interval (`with_debounce`).
- **Delete delay.** How long a `Delete` waits for its path to come back (`with_delete_delay`).
- **Polling.** Finding changes by reading the disk on a timer instead of through the watch API of
  the operating system (OS).

## The API

| Call | Behavior |
|---|---|
| `Watcher::new(handler)` | Starts a watcher with the default `Config`. Nothing is watched yet. It fails only when the watcher thread, or the queue that feeds it, cannot be created. When the watch API of the OS cannot start, the handler receives one error (`WatchLimit` or `Io`) and the watcher polls everything. |
| `Watcher::with_config(config, handler)` | Starts a watcher with the given `Config`, in the same way. |
| `watch(paths)` | Watches the given paths as one operation. Each path can be a file, a directory together with everything below it, or a path that does not exist yet. Returns once every path is watched. Watching a path that is already watched has no effect. |
| `unwatch(paths)` | Undoes an earlier `watch` of exactly these paths. Nothing is reported for them, with one exception. When such a path was followed through a symbolic link inside a watched directory, the link becomes an ordinary file of that directory again, and this change is reported (promise 12). |
| `close()` | Stops the watcher and waits for its thread to exit. A disk check in progress stops at its next step, so the wait lasts at most one look at the disk. Afterwards the handler is not called again, and `watch` and `unwatch` return `Closed`. Dropping the `Watcher` has the same effect. |

The handler has the type `FnMut(Result<Vec<Event>, Error>) + Send + 'static`, and `Watcher` is
`Send + Sync`. `watch`, `unwatch` and `close` can be called from any thread, including from inside
the handler. When they are called from another thread, they block until the handler has returned,
including the handler call for the batch that the call itself produces. The handler must therefore
never wait for such a caller: not for a lock that the caller holds, not for room in a bounded
channel that the caller fills, and not for a call into another watcher whose handler calls back.
Otherwise both threads wait forever.

If the handler or the ignore filter panics, the watcher thread ends. Nothing more is reported, and
`watch` and `unwatch` return `Closed`.

`watch` and `unwatch` return an error only for a path that cannot be used (`InvalidPath`), and when
the watcher is closed (`Closed`). A path that the OS cannot watch or read is not an error of the
call. It stays watched, the handler receives the error, and the path is polled.

## Events

Only files are reported, never directories. Each event is an `Event { path, kind }`, where `kind`
is an `EventKind`:

| Event | Meaning |
|---|---|
| `Create` | A file exists at the path, and the handler has not been told about it. |
| `Update` | The file may have new content. Its modification time or size changed, or another file now occupies the path. |
| `Delete` | A file that the handler was told about no longer exists. |

The handler receives either `Ok(events)`, which is one batch, or `Err(error)`. It runs on the
watcher thread, so it should do little more than pass the batch on.

- A batch is never empty and contains at most one event per path. The order of the events within
  a batch is unspecified.
- An event path is a watched path in its clean form (promise 8), either alone or followed by the
  path of a file below it. It starts with the watched path, byte for byte.
- An error means that something could not be watched or read. The watcher keeps running.
  `error.kind()` says what the caller can do about it, and is one of `InvalidPath`, `WatchLimit`,
  `PermissionDenied`, `RemoteDisk`, `Io` and `Closed`. `error.path()` is one path that shows the
  problem, or none when the problem is the watcher itself. `error.source()` is what the OS or the
  backend reported. The handler receives at most one error of each kind per batch. `RemoteDisk` is
  reported once per disk.

The text of an error states its kind, what happened to the watcher when the error concerns it, its
path, and its source, each of them once:

```text
permission denied: /p/secret: Permission denied (os error 13)
the OS has no room for one more watch, polling instead: /p/src: inotify has no watch left for this user (raise fs.inotify.max_user_watches)
input or output error: the OS watcher stopped, so every path is polled: Bad file descriptor (os error 9)
```

The source of a limit error names the exact limit that was hit and what to raise. An error without
a path concerns the watcher itself, whose backend did not start or has stopped. The text says which
of the two happened, and that every path is now polled.

## Options

| `Config` method | Default | Purpose |
|---|---|---|
| `with_ignored(f)` | nothing is ignored | Excludes paths from watching and reporting. |
| `with_debounce(d)` | none on macOS, 10 ms elsewhere | Sets the quiet time. |
| `with_delete_delay(d)` | 10 ms; none under polling | Sets how long a `Delete` waits for its path to come back. |
| `with_polling(bool)` | `false` | Uses polling instead of the watch API of the OS. |
| `with_poll_interval(d)` | 100 ms | Sets the pause between two polling rounds. |

Settings cannot be changed after the watcher is created. Every duration is capped at one year.

**Ignore filter.** The filter is `f(path, kind) -> bool`, where `kind` is `EntryKind::File` or
`EntryKind::Dir`. A path for which `f` returns `true` is never reported, and an ignored directory is
never read or watched. `f` must return the same answer for the same input, and should ignore
everything below a directory that it ignores. It also applies to the watched paths themselves
(promise 10).

**Quiet time.** The disk is checked, and a batch is delivered, once no change has been noticed for
`d`. While changes keep arriving, a batch is delivered four times `d` after the first change since
the previous batch, plus the time that the disk check takes. This wait is at most 500 ms, or `d`
if `d` is longer. Zero disables the wait. A `watch` or `unwatch` call ends the quiet time. A poll
that runs during the quiet time does not end it, but a change that the poll finds on its own is
reported. Under polling there is no quiet time, and each round reports the changes it finds.

**Delete delay.** A file that is removed and written again within `d` is reported as one `Update`.
A real deletion is reported `d` after the OS reports the path gone, or after the quiet time if that
is longer. Zero disables the wait, and such a file is then reported as a `Delete` and a `Create`.
Under polling there is no delete delay, because there is no quiet time. chokidar, notify and
watchdog behave the same way. A polling round that falls inside a save that leaves the path empty
for a moment reports a `Delete` and a `Create`. With vim on Linux, polled every 100 ms, this
happened in 4 of 100 saves.

**Polling.** `with_polling(true)` reads the disk on a timer instead of using the watch API of the
OS. It is meant for network drives and other places where the OS reports no changes. The poll
interval is the pause between two rounds. It is never shorter than the duration of the previous
round, so polling uses at most half a core. The minimum is 1 ms. Each round inspects every watched
file once, which takes about 50 ms for 20,000 files on APFS.

## Promises

Each promise is verified by stories in `tests/stories`. The first lines of `promises.rs` list the
stories for each promise. Every story runs on the simulated disk with each simulated backend, and
on the real disk with the native backend of the host and with polling. It must see the same events
everywhere. A story that needs a disk on which two spellings of a name refer to one entry
(promise 13) declares this with `Settings::loose`. The simulated disk is then set up that way, and
the real disk is probed. A story never skips a configuration silently. It prints `skipped: ...` to
stderr instead. Stories are skipped in these cases:

- On the real disk of Linux, for promise 13.
- On Windows with the native backend, for the stories that rename a directory above a watched one
  (promises 6 and 13), because Windows refuses the rename while a handle is open.
- On Windows when the user may not create symbolic links (promise 12, and one story each of
  promises 6 and 13).
- Everywhere except on the default disk of macOS, for the story that uses another Unicode form of
  a name.

The story with sockets (promise 1) runs on Unix only.

1. **A watched path is a file, a directory, or nothing.** A file is anything that is not a
   directory, including a socket and a symbolic link inside a watched directory.
2. **Events describe the difference from the disk.** The last event for a path gives its state.
   After `Create` or `Update` the file exists, and after `Delete` it does not. Once activity
   stops, the known files are exactly the files on the disk.
3. **`Update`.** `touch` produces an `Update`, and `chmod` does not. An atomic save (writing a
   temporary file and renaming it over the original) produces one `Update`, with no `Delete` and
   no `Create`. So does a file that is removed and written again within the delete delay (10 ms;
   none under polling). There can be more `Update` events than writes. Some saves free the name
   for a moment, such as `ReplaceFileW`, which .NET's `File.Replace` uses. A file that such a save
   replaces again and again, without pause, can be reported as a `Delete` and a `Create` when both
   checks of the delete delay fall into such moments. Measured on Windows with a delete delay of
   100 ms, this happened in 62 of 10,000 saves.
4. **What exists when `watch` returns is not reported.** Every later change is. There is one
   exception. When an earlier `watch` could not list a directory, for lack of permission or
   because another program held it, a later `watch` of a path at or below that directory reports a
   file in it as `Create`, because the handler was never told about that file.
5. **A watch belongs to its path.** It keeps working when a missing path becomes a file or a
   directory, when a file becomes a directory or the reverse, when a whole chain of missing
   directories appears at once, and when a component on the way to the path changes its kind.
6. **A directory is reported as its files.** A directory that appears (created or moved in)
   produces one `Create` for each file below it. A directory that disappears (removed, moved out,
   or renamed) produces one `Delete` for each known file below it.
7. **`unwatch` reports nothing.** A file that another watched path still covers remains known, and
   its changes are reported as before. A watched directory covers the names that it lists, not a
   path that is spelled differently from the disk (promise 13). There is one exception. A symbolic
   link inside a watched directory, through which the path was followed, becomes an ordinary file
   of that directory again, and this change is reported (promise 12).
8. **Paths.** A relative path is resolved against the current directory at the time of the call.
   `.`, repeated separators and a trailing separator are removed, and `..` removes the component
   before it. All of this is done on the text, and no symbolic link is resolved. The result, the
   clean form, is the watched path. A path that cannot be used is rejected: an empty path, a path
   that contains a NUL, a path that goes above the root, and a top directory (`/`, `C:\`,
   `\\server\share`). The call returns `InvalidPath` with the first such path, and the other paths
   of the same call are still watched.

   On Windows the same applies to a name that Windows does not accept as written. Such a name ends
   with a dot or a space, or contains `:` (which denotes a stream of a file), a control character,
   or one of `< > " | ? *`. A name of this kind inside a watched directory is reported like any
   other file; git, WSL and unpacked archives create such names. A path with a drive and no root
   (`C:name`) is resolved against the current directory when that is on the same drive, and is
   `InvalidPath` for another drive. A `\\?\` path is taken in its plain form, `/` is read as `\`,
   and event paths use the same form. A path that starts with two separators is a share
   (`\\server\share\...`, even with one separator too many before the share) or `InvalidPath`,
   never a path on the current drive.
9. **Calls from inside the handler.** `watch`, `unwatch` and `close` work inside the handler. They
   take effect when the handler returns.
10. **The ignore filter.** Nothing inside an ignored directory is reported, and the directory is
    neither read nor watched. The filter also applies to the watched paths. `watch` asks it about
    each directory above the path, as a directory, from the top down and whether or not the
    directory exists. It also asks about the path itself, both as a file and as a directory. A path
    below an ignored directory, or a path ignored as both kinds, is not watched at all, and it
    produces no event, no error and no cost. Every other watched path is asked about for the kind
    of entry at it, and again whenever that kind changes. While the filter ignores that kind, the
    path counts as missing. The filter alone never produces an event. A path that counts as missing
    was never reported, so it gets no `Delete`. A file that replaces an ignored directory at a
    watched path has appeared on the disk, so it is a `Create`. A filter should ignore everything
    below a directory that it ignores. A filter that inspects only the last component still gives
    the same result below an ignored directory. For example, `/p/node_modules/pkg/x.js` is never
    reported, whether `/p` or the file itself is watched, because `node_modules` lies on its path.
11. **Nothing beside a watched path.** A file next to a watched file is not reported.
12. **Symbolic links.** A watched path that is a symbolic link, or that passes through one, is
    followed, and its events use the spelling that was given. A symbolic link inside a watched
    directory is a file and is not followed. While a watched path is at such a link or passes
    through it, the directory follows the link too. The `watch` of that path reports the link as
    gone, and what lies behind the link becomes the baseline of that `watch`. The `unwatch` of that
    path reports the link as a new file, and forgets what lay behind it without reporting it.
13. **Spelling.** Some disks treat two spellings of a name as one entry, such as two letter cases
    or two Unicode normalization forms. The default disks of macOS and Windows do so. On such a
    disk, a watched path is the text that was given. It refers to whatever the OS finds at that
    text, and its events use that spelling. A name below a watched directory is spelled as on the
    disk. Each watched path is resolved on its own. Two watched paths that are two spellings of one
    file both receive its events. So does a watched path in another spelling inside a directory
    that is also watched. The directory reports the file under the name on the disk, and the
    watched path reports it under its own spelling.

## Known limitations

These are the known cases in which a change is reported late, wrongly, or not at all, or in which
the watcher gets in the way of other programs. None of them is fixed.

- **A writer that keeps a file open.** macOS reports nothing until the file is closed, and the
  same holds for a write through `mmap`. Linux never reports a write through `mmap`, not even after
  the file is closed, because inotify has no event for it. On Windows a save produces two records,
  one when the open truncates the file and one when the file is closed, and nothing for the writes
  in between (measured on NTFS). A save that takes longer than the quiet time from its open to its
  close is therefore reported twice, the first time with what was written so far. On Windows, as on
  Linux, a write through a mapped view of an existing file is never reported, not even after the
  file is closed. Neither is a write through a handle opened by the index of the file
  (`OpenFileById`), but polling finds that one.
- **vim and Neovim on Linux.** They save by renaming the file away, syncing their swap file,
  writing a new file, and syncing it. On a busy machine the name can stay free longer than the
  delete delay, and the sync can last longer than the quiet time. Such a save can then be reported
  as a `Delete` and a `Create`, or as two `Update` events. Measured in a Linux VM with 60 saves per
  run, a `Delete` and a `Create` came for 1 save when idle and for 7 to 23 when busy, and two
  `Update` events for 4 saves when idle and for 10 to 14 when busy. With
  `with_debounce(Duration::from_millis(20))` and `with_delete_delay(Duration::from_millis(50))`, no
  save was reported wrongly on an idle machine, and on a busy machine only the saves that left the
  name free for more than 50 ms. The saves of VS Code, the JetBrains IDEs, `sed -i` and
  `git checkout` were reported correctly in every run.
- **Polling opens and reads a file that was just written.** It does so for files up to 16 MiB, in
  every round for 2 s after the write, on every OS. Every backend does the same for a young file
  that only the poll inspects. On Windows a program that opens such a file meanwhile without
  sharing it is refused, and the first read after a write waits for the virus scanner. While the
  read lasts, Windows refuses with "access denied" a save by rename over the file (`MoveFileEx`
  with `REPLACE_EXISTING`, Python's `os.replace`, Node's `fs.rename`) and a rename of a directory
  above it. Measured next to a polling watcher, this affected 2% to 7% of the saves of a file that
  is saved again within one poll interval, and none for a file older than 2 s. Each directory of a
  polled tree is also opened for a moment in every round to be listed.
- **Windows: listing a directory keeps it open for a moment.** In that moment a rename of a
  directory above it fails, and so does the second step of another program's junction (`EBUSY` in
  Node). Measured under polling, this affected 2% to 10% of the renames of a directory containing
  500 subdirectories. A directory that another program holds without sharing is not an error for
  the watcher. It is inspected again until it can be listed.
- **macOS: a new socket or FIFO** is not reported on macOS 26. macOS 27 reports it. It is found when
  something else makes the watcher inspect its directory. Its removal is reported.
- **A rewrite of the same size that restores the old modification time** is not reported on macOS
  or under polling. Linux reports it.
- **A disk whose clock is behind the clock of the watcher**, such as a network disk or a shared
  folder of a VM, has no young files, because a file written just now looks old by the clock of the
  watcher. A second write of the same size within one step of the timestamps of that disk is not
  found by inspecting the file. It is reported where the OS reports writes (Linux, Windows).
- **A mount over a watched directory, or over a directory above one,** is detected on Linux and
  macOS. Windows was not tested. On Linux it is not detected where the watched path reaches the
  mount point through a symbolic link. There it is noticed when something else makes the watcher
  look.
- **A path longer than the OS allows** gets no events. `watch` accepts it, the handler receives one
  `Io` error, and the path counts as missing.
- **A directory on a network drive or shared with another machine** (NFS, SMB, a folder of the host
  inside a virtual machine or a container). Changes made by the other machine are not reported.
  The handler receives `RemoteDisk` once per such disk. On Linux this also happens for a local disk
  mounted through FUSE, which cannot be told apart from a shared folder of a VM. It never happens
  on Windows, where the SMB server reports such changes. `with_polling(true)` finds them.
- **A watched path spelled with a look-alike character** where the disk has plain letters, or the
  reverse. Examples are `ß` for `ss`, the long s for `s`, the Kelvin sign for `K`, a ligature such
  as `ﬁ` for `fi`, and a CJK compatibility ideograph for its unified ideograph. The disks of macOS
  treat the two as one name, and such a path gets no events on macOS. Polling works. NTFS treats
  them as two names (measured), so nothing is missed there.
- **Linux: a disk on which two spellings of a name refer to one entry**, such as a folder shared
  from a Mac or from Windows, a FAT or SMB disk, or a directory with the casefold attribute. Such a
  disk is treated as one that tells the spellings apart, like the native disks of Linux. A watched
  path that differs from the disk only in the case of its ASCII letters still receives its events.
  The exception is a second write of the same size within one step of the timestamps of that disk,
  which inotify reports under the spelling of the disk. Below a watched directory, a change made
  through another spelling of a name can be reported under a name that the disk does not have, or
  not at all. A rename that changes only the letter case leaves the old name known.
- **Linux and Windows: a watched file written through another hard link** whose name lies in a
  directory that is not watched gets no event, because inotify and `ReadDirectoryChangesW` report a
  write under the name that the writer used. Polling finds it, and so does macOS.
- **Windows: a young file in a directory that was moved away and back**, or removed and restored
  within the delete delay, receives an `Update` even when nothing was written to it. A file is
  young when it was written in the last 20 ms, or in the last 2 s on a disk whose timestamps count
  whole seconds. A write made while the directory was away is reported under the other name, which
  the watcher cannot connect to the file, so the watcher reports every file whose fingerprint could
  hide such a write. Linux and macOS are not affected, because an inotify watch follows the
  directory, and macOS reads such a file.
- **Under polling, a young file larger than 16 MiB, or one that cannot be read,** receives one
  `Update` when it becomes 2 s old, even when nothing changed, and even when it was written just
  before `watch`.
- **The target of a watched symbolic link changes**, so that the link points elsewhere. This is
  detected by a poll within one second, or within the poll interval when polling is on. A link that
  points elsewhere and back within that time is not detected, and nothing that happened at the
  other target in between is reported.
- **A watched path that is a symbolic link to a file.** A write to the file behind it is detected
  by the same poll, within one second, with every backend. A rewrite of the same size that restores
  the old modification time is not reported for such a path, on Linux either.
- **Windows: the directories above a watched path** are polled once per second. When one of them is
  created, recreated or removed, the events can arrive up to one second late. On macOS each of them
  is instead held open for kernel events, which arrive at once. They are polled only when the
  process has no file descriptors left for them (`ulimit -n`), and the watcher reports this
  (`architecture.md` §4.2).
- **macOS: many watched paths and a busy disk.** Events arrive late when the watcher has many
  separate paths and other programs write a lot. Measured while 50,000 other files were written,
  the delay was 0.1 s with 100 paths, 0.9 s with 500, 1.8 s with 1,000, and 3.5 s with 2,000.
  Nothing is lost. Other users of FSEvents in the same process are delayed too.
- **Windows: a directory above a watched directory cannot be renamed** while it is watched. The OS
  refuses the rename.
- **Windows: a watched directory that is renamed.** The `Delete` events of its files arrive when the
  poll of the directory above finds it gone, up to one second late. Nothing is reported under the
  old name in the meantime.
- **Windows: a junction or a directory symbolic link** is created in two steps. A plain directory
  comes first, and the second step, which makes it a link, produces no record. A watcher that looks
  between the steps sees a directory. A directory created within the last two seconds is inspected
  once more in the next disk check, within the poll interval, and the link is reported then. Until
  then, a watched path at or through the link gets no events. That next check can also fall
  between the steps, though this is unlikely: tools perform both steps within a millisecond, and a
  disk check comes 10 ms after a change at the earliest. In that case the link is seen only when
  something else makes the watcher inspect its directory.
- **Windows: more than 64 directories** share handles. A watched path whose directory is then
  spelled differently from the disk (beyond letter case) gets no events. Neither does a change that
  a program makes through the short (8.3) name of a directory on the way to it.
- **Windows: a path watched by its short (8.3) name**, such as `LONGFI~1.TXT`, gets no events,
  because Windows reports an entry under its long name. Watch the long name instead.
- **Windows: a name containing `\` or `/`**, which only a disk written by another system can hold,
  is not reported, because no path can name such an entry.
- **Where the OS watches are placed depends on earlier `watch` and `unwatch` calls.** A stream of
  macOS stays where it is until the streams are replaced, or until nothing watched remains below it.
  Nothing is missed.
- **The Windows backend is new.** Its tests pass on Windows 11 with NTFS. Nothing else was tried: no
  network share, no ReFS, and no older version of Windows.
- **Android has never been run.** It uses the same code as Linux and is only type-checked.

## Timing

- **Normally** an event arrives within the quiet time (none on macOS, 10 ms elsewhere) plus the
  time that the OS takes to report the change, about 1 to 15 ms. For a watched path that is a
  symbolic link to a file, a write to the file behind it is found by the poll of the links, within
  one second.
- **Under polling** a change is found by the next round. This takes at most the poll interval
  (100 ms) plus the duration of a round (about 50 ms for 20,000 files on APFS), or twice the
  duration of a round when a round takes longer than the interval. The files written in the last
  two seconds are read in every round. On Windows a read right after a write waits for the virus
  scanner.
- **Under a flood** of changes, a batch is delivered four quiet times after the first change since
  the previous batch, plus the duration of the disk check. That is 40 ms on Linux and Windows. On
  macOS a batch comes with each notification of FSEvents. A check of thousands of changed paths
  takes a second or more. In one measurement, with 30,000 files created by eight threads, the
  longest gap was 2 s.
- **A `Delete`** arrives one delete delay (10 ms) after the OS reports the path gone, or one quiet
  time later if that is longer, once the path has been found empty twice. Under polling it arrives
  in the round that finds the path gone. Under a flood it arrives with the next batch. It arrives at
  once when a directory is found where the file was, when a file is found where the directory was,
  and when a file below a watched directory is renamed to another spelling of its name (another
  letter case, on macOS and Windows). A disk error that does not establish whether the path exists
  is never a `Delete`. "Permission denied" counts as missing.
- **Lost notifications** are repaired by reading the disk again. Notifications are lost when the OS
  drops some, or when more than 16 MiB of them wait for the watcher. These rescans are paced. After
  a rescan, the next one waits nine times as long as that rescan took, and at most half a second.

## Comparison with notify, chokidar and @parcel/watcher

| Feature | In pathwatch | Notes |
|---|---|---|
| watch | `watch(paths)` | Supported. There is no recursive mode, because a directory is always watched together with everything below it. A path can be a file or not exist yet. No globs. |
| unwatch | `unwatch(paths)` | Supported differently. It undoes a `watch` of the same path, and cannot exclude a part of a watched directory. Use the ignore filter for that. |
| close | `close()`, or dropping the watcher | Supported. |
| ignored | `Config::with_ignored(f)` | Supported differently. It is one function, set when the watcher is created, with no glob list and no later change. It applies to the watched paths too: a path below an ignored directory, or ignored both as a file and as a directory, is not watched. |
| polling | `with_polling`, `with_poll_interval` | Supported. In addition, a directory that the OS cannot watch is polled without any option, and so is everything when the watch API cannot start or stops. The watcher then behaves as with `with_polling(true)`. |
| debounce | `with_debounce` | Supported and built in. Waiting until a file stops growing (chokidar's `awaitWriteFinish`) is not supported, so a slow writer produces more than one `Update`. |
| atomic writes | `with_delete_delay(d)` | Supported differently. An atomic save (writing a temporary file and renaming it over the original) is always one `Update`, without an option. A file that is removed and back within the delete delay is one `Update` too. The delete delay is 10 ms; chokidar's `atomic` waits 100 ms. |
| ignore initial | always on | Not configurable. What exists at the time of `watch` is never reported. Read the directory yourself if you need the list. |
| directory events | none | Not supported. A directory is reported as its files, so a moved directory needs no walk on the caller's side. |
| rename events | none | Not supported. A rename is a `Delete` and a `Create`. The watcher never relies on what the OS says happened. It compares the disk with what it has reported. |
| symlinks | no option | Supported differently. A watched path is followed, and a symbolic link inside a watched directory is a file. Following links inside a watched directory is not supported. |
| errors | `Err(error)` to the handler | Supported. The watcher keeps running after every error. |
| depth, `since` | none | Not supported. There is no depth limit, and no changes from before the call are reported. |

## Platforms

| OS | Watch API |
|---|---|
| Linux, Android | inotify |
| macOS | FSEvents, with kqueue for the directories on the way to each stream |
| Windows | `ReadDirectoryChangesW` |
| Every other target | None; the watcher always polls |

**Linux and Android.** inotify needs one kernel watch per directory, so a large tree needs a high
`fs.inotify.max_user_watches`. At the limit the handler receives `WatchLimit` once, and each
directory that gets no watch is polled once per second. Otherwise only a watched path that is a
symbolic link is polled. Without one, the watcher sleeps between notifications. When inotify cannot
start at all, the handler is told once and the watcher polls everything. Changes made on another
machine (NFS, a folder shared with a VM) are not reported. The handler receives `RemoteDisk`, and
polling finds the changes.

**macOS.** See also "Known limitations". A process can run 512 FSEvents streams. One watcher usually
keeps 8 or fewer, and at most about 70. A `watch` call that needs new streams adds up to 8. From 64
streams on, the streams are placed again. Those whose paths remain keep running, and the others are
stopped, with the directories below them read again. No stream is placed on `/`, `/Users` or the
home directory. What could only be watched from there is polled once per second, as on Windows.
Each directory on the way to a stream holds an open file descriptor for the kernel events that
report its rename, its removal, or a disk mounted on it. A project of 20,000 watched files uses
about 50 descriptors. All watchers of a process together use at most half of its descriptor limit.

**Windows.** Tested on Windows 11 with NTFS only. A watcher holds at most 64 directory handles.
Further directories share a handle on a directory above them, and a directory behind a junction
keeps a handle of its own. No handle is placed on `C:\`, `C:\Users` or `C:\Users\<name>`. What could
only be watched from there is polled.

**Other targets.** The watcher always polls, so the cost grows with the number of watched files. On
WASI (`wasm32-wasip1-threads`), the directories that the program may use (its preopened
directories) lie below `/`, which is not one of them. `/` is treated as a directory anyway, so a
path below a preopened directory can be watched.
