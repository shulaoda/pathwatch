# pathwatch

`pathwatch` watches paths and tells you which files were created, updated or deleted.

```rust
use std::sync::mpsc;

use pathwatch::{Config, EventKind, Watcher};

fn main() -> Result<(), pathwatch::Error> {
  // Everything at or below a directory named `node_modules`.
  let config = Config::default()
    .with_ignored(|path, _kind| path.components().any(|part| part.as_os_str() == "node_modules"));

  // The handler runs on the thread of the watcher: it only passes each batch on.
  let (sender, batches) = mpsc::channel();
  let watcher = Watcher::with_config(config, move |result| {
    let _ = sender.send(result);
  })?;

  // A directory, a file, and a path that does not exist yet.
  watcher.watch(&["/project/src", "/project/package.json", "/project/not-there-yet.json"])?;

  for result in batches {
    match result {
      Ok(events) => {
        for event in events {
          match event.kind {
            EventKind::Create => println!("create {}", event.path.display()),
            EventKind::Update => println!("update {}", event.path.display()),
            EventKind::Delete => println!("delete {}", event.path.display()),
          }
        }
      }
      Err(error) => eprintln!("{error}"),
    }
  }
  Ok(())
}
```

To try it on your own files:

```
cargo run -p pathwatch --example watch -- src package.json not-there-yet.json
cargo run -p pathwatch --example dev_server -- path/to/project
```

## The API

| Call | What it does |
|---|---|
| `Watcher::new(handler)` | Starts a watcher with the default `Config`. Nothing is watched yet. |
| `Watcher::with_config(config, handler)` | The same, with your `Config`. |
| `watch(paths)` | Watches paths, in one step: each a file, a directory (with everything below it), or a path that does not exist yet. Returns when every path is watched. Watching a path again does nothing. |
| `unwatch(paths)` | Takes back a `watch` of these very paths. Nothing is reported for them, with one exception: a link inside a watched directory that such a path was followed through is a plain file of that directory again, and that change is reported (promise 12). |
| `close()` | Stops the watcher and waits for its thread: a check of the disk that runs ends at its next step. Dropping the `Watcher` does the same. |

`watch`, `unwatch` and `close` can be called from any thread, and from inside the handler.
From another thread they wait until the handler has returned, the batch that the call itself
makes included: a handler must never wait for such a caller (a lock it holds, a bounded channel
it fills, a call into another watcher whose handler calls back), or both wait for ever.
A handler or an ignore filter that panics ends the thread of the watcher: nothing is reported
after that, and `watch` and `unwatch` return `Closed`.
`watch` and `unwatch` return an error only for a path that cannot be used (`InvalidPath`) and
when the watcher is closed (`Closed`). A path that the OS cannot watch or read is no error
there: the handler gets it.

## Events

Only files are reported, never a directory.

| Event | Meaning |
|---|---|
| `Create` | A file is there that you were not told about. |
| `Update` | The file may have other content: its modification time, its size, or the file itself is another one. |
| `Delete` | The file that you were told about is gone. |

The handler gets `Ok(events)`, one batch, or `Err(error)`. It runs on the thread of the
watcher, so it should only pass the batch on.

- A batch is never empty and has at most one event for a path. Its order is not promised.
- An event path is the watched path as you gave it, or that path and the rest below it.
- An error means that something could not be watched or read. The watcher keeps running.
  `error.kind()` is one of `InvalidPath`, `WatchLimit`, `PermissionDenied`, `RemoteDisk`, `Io`,
  `Closed`.
  The handler gets one error of a kind for a batch, with one path that shows the problem;
  `RemoteDisk` comes once for each disk.

## Options

| `Config` method | Default | Meaning |
|---|---|---|
| `with_ignored(f)` | nothing is ignored | `f(path, kind) -> bool`, where `kind` is `EntryKind::File` or `EntryKind::Dir`. A path for which it returns `true` is never reported, and an ignored directory is never read or watched. It must give the same answer for the same input, and should ignore everything below a directory that it ignores. It decides about the paths you watch too (promise 10). |
| `with_debounce(d)` | none on macOS, 10 ms on Windows, 20 ms on Linux | The quiet time: the disk is checked, and a batch goes out, when no change was noticed for `d`. While changes keep coming, a batch goes out `4 × d` after the first change since the last batch, and 500 ms after it at most (never before `d`), plus the time that the check of the disk takes. Zero: no waiting. The most is a year, also for the poll interval. A `watch` or `unwatch` call ends the quiet time; a poll that runs meanwhile does not, but a change it finds by itself is reported. Under polling there is no quiet time: a change is found by the next round. |
| `with_delete_delay(d)` | 10 ms on macOS and Windows, 50 ms on Linux; none under polling | How long a `Delete` waits for the path to come back. A file that is removed and written again within `d` is one `Update`; a real delete is reported `d` after the OS said that the path is gone, or after the quiet time if that is longer. Zero: no waiting, and such a file is a `Delete` and a `Create`. Under polling there is none, as there is no quiet time, and as in chokidar, notify and watchdog: a round that falls into a save that leaves the path empty for a moment reports a `Delete` and a `Create` (vim on Linux, polled every 100 ms: 4 of 100 saves). The most is a year. |
| `with_polling(bool)` | `false` | Read the disk on a timer and do not use the watch API of the OS. For network drives and other places where the OS reports no changes. |
| `with_poll_interval(d)` | 100 ms | The pause between two rounds of polling, and at least as long as the round took: polling takes half of a core at most. The least is 1 ms. A round looks at every watched file once (20,000 files: about 50 ms on APFS). |

The settings cannot be changed after the watcher is made.

## Promises

Each of these has a story (`tests/stories`; the first lines of
`promises.rs` say which). A story runs on the simulated disk with each simulated backend
and on the real disk with the backend of this OS and with polling, and the events are the
same everywhere. A story that needs a disk that takes two spellings of a name for one (13)
says so (`Settings::loose`): the simulated disk is made so, and the real disk is probed. A
world that a story skips is never silent: the story says so on stderr (`skipped: ...`).
That happens on the real disk of Linux for the stories of 13, on Windows with the native
backend for the stories that rename a directory above a watched one (6, 13: Windows refuses
the rename while a handle is open), on Windows when the user may not make a symbolic link
(12, and one each of 6 and 13), and everywhere but the usual disk of macOS for the story
with another Unicode form of a name. The story with sockets (1) is Unix only.

1. **A watched path is a file, a directory or nothing.** A file is anything that is not a
   directory: also a socket, and a symbolic link inside a watched directory.
2. **Events are the difference to the disk.** The last event for a path tells its state: after
   `Create` or `Update` the file is there, after `Delete` it is not. When things are quiet, the
   files you were told about are the files on the disk.
3. **`Update`.** `touch` is an `Update`; `chmod` is not. An atomic save (write a temporary
   file, rename it over) is an `Update`, with no `Delete` and no `Create`. So is a file that is
   removed and written again within the delete delay (10 ms on macOS and Windows, 50 ms on Linux; none
   under polling). There can be more `Update`
   events than writes. A file that is replaced again and again without a pause by a save that
   leaves the name free for a moment (`ReplaceFileW`, the save of .NET's `File.Replace`) can
   get a `Delete` and a `Create` when both looks of the delete delay fall into such moments
   (measured on Windows with a delete delay of 100 ms: 62 of 10,000 saves).
4. **What exists when `watch` returns is not reported.** Every change after that is. One
   exception: a file in a directory that an earlier `watch` could not list (no permission, or
   another program held it) is reported as `Create` by a later `watch` of a path at or below
   it, since nothing was told of it.
5. **The watch belongs to the path.** It keeps working when a missing path becomes a file or a
   directory, when a file becomes a directory or the other way round, when a whole chain of
   missing directories appears at once, and when a name on the way to the path changes its
   kind.
6. **A directory is its files.** A directory that comes (made, moved in) is one `Create` for
   each file below it. One that goes (removed, moved out, renamed) is one `Delete` for each
   file that you were told about.
7. **`unwatch` reports nothing.** A file that another watched path still covers stays known,
   and its changes are reported as before. A watched directory covers the names that it
   lists: not a path that is spelled in another way than the disk has it (13). One exception:
   a link inside a watched directory that the path was followed through is a plain file of
   that directory again, and that change is reported (12).
8. **Paths.** A relative path is joined to the current directory at the time of the call. `.`,
   `..` and repeated separators are cleaned by text; no link is resolved. A path that cannot be
   used (empty, a NUL in it, a file system root) is `InvalidPath`; the other paths of the same
   `watch` call are watched. On Windows this also holds for a name that Windows does not
   take as it is: one that ends with a dot or a space, or has a `:` (a stream of a file), a
   control character or one of `< > " | ? *` in it. A name of that kind that a watched
   directory holds (git, WSL and an unpacked archive make them) is a file like the others.
   On Windows a path with a drive and no root (`C:name`) is joined to the current directory
   when that is on the drive, and is `InvalidPath` for another drive; a `\\?\` path is taken
   in its plain form and `/` as `\`, and so are the event paths; a path that starts with two separators is a
   share (`\\server\share\...`, with a separator too many before the share still) or
   `InvalidPath`, never a path on the current drive.
9. **Calls inside the handler.** `watch`, `unwatch` and `close` work there. They take effect
   when the handler has returned.
10. **The ignore filter.** Nothing is reported from inside an ignored directory, and the
    directory is not read or watched. The filter decides about the paths you watch too.
    `watch` asks it about each directory above the path, as a directory, from the top down
    and whether the directory exists or not, and about the path itself as a file and as a
    directory: a path below an ignored directory, or ignored for both kinds, is not watched
    at all, with no event, no error and no cost. Every other watched path is asked about for
    the kind that is there, again whenever that changes, and while the filter ignores that
    kind the path counts as missing. No event comes from the filter alone: a path that counts
    as missing was never reported and gets no `Delete`; a file that takes the place of an
    ignored directory at a watched path came to the disk and is a `Create`. A filter should
    ignore everything below a directory that it ignores. One that only looks at the last name
    gives the same answer below an ignored directory all the same:
    `/p/node_modules/pkg/x.js` is never reported, whether `/p` or the file itself is watched,
    since `node_modules` is on the way.
11. **Nothing beside a watched path.** A file next to a watched file is not reported.
12. **Symbolic links.** A watched path that is a link, or that goes through one, is followed,
    and its events carry the spelling you gave. A link inside a watched directory is a file: it
    is not followed. While a watched path is at or goes through such a link, the link is
    followed for the directory too: the `watch` of that path reports the link as gone, and
    what is behind it is the baseline of that `watch`; the `unwatch` of it reports the link as
    a new file and forgets what was behind it without a word.
13. **Spelling.** On a disk that takes two spellings of a name for one (letter case, Unicode
    form: the usual disks of macOS and Windows), a watched path is the text you gave:
    what is at it is what the OS finds there, and its events carry your spelling. A name below
    a watched directory is spelled as on the disk. Every watched path is a question of its own.
    Two watched paths that are two spellings of one file both get their events. So does a
    watched path in another spelling inside a directory that you watch too: the directory
    reports the file under the name of the disk, and the path under its own.

## Holes

What is not reported, or reported wrongly. All of these are known and not fixed.

- **A writer that keeps a file open.** macOS reports nothing until the file is closed; the
  same for a write through `mmap`. Linux reports no write through `mmap` at all, also after
  the file is closed: inotify has no word for it. On Windows a save is two records, one at the
  open that truncates the file and one at the close, and nothing for the writes between
  (measured on NTFS): a save that takes longer than the quiet time from its open to its
  close is told twice, the first time with what was written so far. On Windows a write
  through a mapped view of a file that exists is not reported at all, as on Linux, also after
  the file is closed, and neither is a write through a handle that was opened by the index of
  the file (`OpenFileById`); polling finds the second.
- **With polling, a file that was just written is opened and read** (up to 16 MiB, at every
  round for 2 s after the write), on every OS, and so is a young file wherever only the poll
  looks at it. On Windows a program that opens it meanwhile and shares it with nobody is
  refused, and the first read after a write waits for the virus scanner. While the read lasts,
  Windows refuses a save by rename over the file (`MoveFileEx` with `REPLACE_EXISTING`,
  `os.replace` of Python, `fs.rename` of Node) and a rename of a directory above it with
  "access denied": measured next to a polling watcher, 2 to 7 % of the saves of a file that
  is saved again within a poll interval, none for a file older than 2 s. Each directory of a
  polled tree is opened for its listing at every round, for a moment.
- **Windows: a listing holds the directory open for a moment.** A rename of a directory above
  it, and the second step of another program's junction (`EBUSY` in Node), fail in that
  moment (measured under polling: 2 to 10 % of the renames of a directory with 500
  subdirectories in it). A directory that another program holds with no sharing is no error
  for the watcher: it is looked at again until it can be listed.
- **macOS: a socket or a FIFO that is made** is not reported (macOS 26; macOS 27 reports it).
  It is found when something else makes the watcher look at its directory. Its removal is
  reported.
- **A rewrite of the same size that puts the old modification time back** is not reported on
  macOS and with polling. Linux reports it.
- **A disk whose clock is behind the clock of the watcher** (a network disk, a shared folder
  of a VM) has no young files: a file written just now looks old by the time of the watcher.
  A second write of the same size inside one step of the time stamps of that disk is not
  found by a look there; the OS reports it where it reports writes (Linux, Windows).
- **A mount over a watched directory, or over a directory above one**, is noticed on Linux and
  macOS (Windows was not checked). On Linux not where the watched path reaches the mount point
  through a link: there it is noticed when something else makes the watcher look.
- **A path longer than the OS allows** gets no events: `watch` takes it, the handler is told
  once (`Io`), and the path counts as missing.
- **A directory on a network drive, or shared with another machine** (NFS, SMB, a folder of the
  host in a virtual machine or a container): changes that the other machine makes are not
  reported. The handler is told once for each such disk (`RemoteDisk`; on Linux also for a local
  disk through FUSE, which cannot be told apart from a shared folder of a VM; not on Windows,
  where the server of SMB reports changes). `with_polling(true)` finds them.
- **A watched path that is spelled with a look-alike character**, where the disk has plain
  letters, or the other way round: `ß` for `ss`, the long s for `s`, the Kelvin sign for `K`,
  a ligature like `ﬁ` for `fi`, a CJK compatibility ideograph for its ideograph. The disks of
  macOS take these for one name. Such a path gets no events on macOS. With polling it works.
  NTFS takes the two for two names (measured), so nothing is missed there.
- **Linux: a disk that takes two spellings of a name for one** (a folder shared from a Mac or
  from Windows, a FAT or SMB disk, a directory with the casefold flag) is taken for one that
  tells them apart, as the disks of Linux do. A watched path that differs from the disk only
  in the case of its ASCII letters still gets its events, except for a second write of the
  same size inside one step of the time stamps of that disk, which inotify names as the disk
  spells it. Below a watched directory a change made through another spelling of a name can
  be reported under a name the disk does not have, or not at all, and a rename that changes
  only the case leaves the old name known.
- **Linux and Windows: a watched file written through another hard link of it**, under a name
  in a directory that is not watched, gets no event: inotify and ReadDirectoryChangesW report
  a write under the name the writer used. Polling finds it, and so does macOS.
- **Windows: a young file in a directory that was moved away and back**, or removed and put
  back within the delete delay, gets an `Update` also when nothing was written to it (young:
  written in the last 20 ms, or 2 s on a disk that counts in seconds). A write while the
  directory was away is reported under the other name, which the watcher cannot connect, so
  it reports the files whose fingerprint could hide one. Linux and macOS are not touched: the
  watch of inotify follows the directory, and macOS reads such a file.
- **A young file above 16 MiB, or one that cannot be read, under polling:** it gets one
  `Update` when it is 2 s old, also when nothing changed, and also when it was written just
  before `watch`.
- **The link of a watched path changes** (it points somewhere else now): found by a poll,
  within a second, or within the poll interval when polling is on.
- **A watched path that is a link to a file**: a write to the file behind it is found by the
  same poll, within a second, with every backend. And a rewrite of the same size that puts the
  old modification time back is not reported for it on Linux either.
- **Windows: the directories above a watched path** are polled once a second. When one of
  them is made, made again, or removed, the events can come up to a second late. On macOS
  each of them is open for the events of the kernel instead, which tell at once; only when
  the process has no open file left for them (`ulimit -n`) are they polled, and the watcher
  reports it (`references/design.md` 7.2).
- **macOS: many watched paths and a busy disk.** Events come late when the watcher has many
  separate paths and other programs write a lot. Measured while 50,000 other files were
  written: 0.1 s with 100 paths, 0.9 s with 500, 1.8 s with 1,000, 3.5 s with 2,000. Nothing is
  lost. Other FSEvents users in the same process are late too.
- **Windows: the directories above a watched directory cannot be renamed** while it is
  watched. The OS refuses it.
- **Windows: a watched directory that is renamed**: the `Delete` events of its files come when
  the poll of the directory above finds it gone, up to a second late. Nothing is reported
  under the old name meanwhile.
- **Windows: a junction or a directory link** is made in two steps, a plain directory first,
  and the second step gives no record. A watcher that looked between the steps sees a
  directory; a directory made in the last two seconds is looked at once more at the next
  flush, within the poll interval, and the link is reported then. Until then a watched path at
  or through the link gets no events. When that next flush falls between the steps as well
  (the tools make the two steps within a millisecond; a flush comes 10 ms after a change at
  the soonest), the link is seen only when something else brings a look at its directory.
- **Windows: more than 64 directories** share handles. A watched path whose directory is then
  spelled in another way than the disk (more than the case of letters) gets no events, and
  neither does a change that a program makes through the short (8.3) name of a directory on
  the way to it.
- **Windows: a path watched by a short (8.3) name** (`LONGFI~1.TXT`) gets no events: Windows
  names an entry by its long name. Watch the long name.
- **Windows: a name with `\` or `/` in it**, which only a disk that another system filled can
  hold, is not reported: no path names such an entry.
- **Where the OS watches are put depends on what was watched and unwatched before.** A stream of
  macOS stays where it is until the streams are replaced, or until nothing watched is left below
  it. Nothing is missed.
- **The Windows backend is young.** Its tests pass on Windows 11 with NTFS, and nothing else
  was tried: no network share, no ReFS, no older Windows.
- **Android was never run.** It is set to use the same code as Linux, and only type-checked.

## Timing

- **Normally** an event comes within the quiet time (none on macOS, 10 ms on Windows, 20 ms on Linux) plus what the OS takes to tell
  about a change (about 1 to 15 ms). For a watched path that is a link to a file, a write to
  the file behind it is found by the poll of the links: within a second.
- **With polling** a change is found by the next round: within the poll interval (100 ms)
  plus the time of a round (20,000 files: about 50 ms on APFS), or within twice the round when
  the round is longer than the interval. The files written in the last two seconds are read
  in each round (on Windows a read right after a write waits for the virus scanner).
- **Under a flood** of changes a batch goes out four quiet times (80 ms; on macOS each time
  FSEvents tells) after the first change since the last batch, plus the time that the check of
  the disk takes: a check with thousands of changed paths takes a second and more (measured:
  30,000 files made by eight threads, the longest gap 2 s).
- **A `Delete`** comes one delete delay (10 ms on macOS and Windows, 50 ms on Linux) after the OS said
  that the path is gone, or one quiet time if that is longer, once the path was found empty
  twice (under polling, at the round that finds it gone); under a flood it comes with the next batch. It comes at once
  when a directory is found where the file was, or a file where the directory was, or when a
  file below a watched directory is renamed to another spelling of its name (another letter
  case, on macOS and Windows). An error of the disk that says nothing is never a `Delete`;
  "permission denied" counts as missing.
- **Lost notifications** (the OS dropped some, or more than 16 MiB of them waited for the
  watcher) are repaired by reading the disk again. These scans are paced: after one that took
  `t`, the next one waits `9 × t`, and half a second at most.

## Coming from notify, chokidar or @parcel/watcher

| What | In pathwatch | Note |
|---|---|---|
| watch | `watch(paths)` | Offered. There is no recursive mode: a directory is always watched with everything below it. A path may be a file, or not exist yet. No globs. |
| unwatch | `unwatch(paths)` | Offered differently: it takes back a `watch` of the same path. It cannot cut a part out of a watched directory; use the ignore filter. |
| close | `close()`, or drop the watcher | Offered. |
| ignored | `Config::with_ignored(f)` | Offered differently: one function, set when the watcher is made. No glob list, no change later. It decides about the paths you watch too: a path below an ignored directory, or ignored as a file and as a directory, is not watched. |
| polling | `with_polling`, `with_poll_interval` | Offered. On top of that, a directory that the OS cannot watch is polled without an option, and so is everything when the watch API cannot start or stops: then as with `with_polling(true)`. |
| debounce | `with_debounce` | Offered, and built in. Left out: waiting until a file stops growing (chokidar `awaitWriteFinish`). A slow writer gives more than one `Update`. |
| atomic writes | `with_delete_delay(d)` | Offered differently: an atomic save (write a temporary file, rename it over) is always one `Update`, with no option. A file that is removed and back within the delete delay (10 ms on macOS and Windows, 50 ms on Linux; chokidar's `atomic` waits 100 ms) is one `Update` too. |
| ignore initial | always on | Left out: what exists at `watch` time is never reported. Read the directory yourself if you need the list. |
| directory events | none | Left out: a directory is reported as its files, so a moved directory needs no walk on your side. |
| rename events | none | Left out: a rename is a `Delete` and a `Create`. The watcher never reads what the OS says happened; it compares the disk with what it told you. |
| symlinks | no option | Offered differently: a watched path is followed, a link inside a watched directory is a file. Following links inside a watched directory is left out. |
| errors | `Err(error)` to the handler | Offered. The watcher keeps running after every error. |
| depth, `since` | none | Left out: no depth limit and no changes from before the call. |

## Platforms

| OS | Watch API | Limits |
|---|---|---|
| Linux, Android | inotify | One kernel watch for each directory, so a big tree needs a high `fs.inotify.max_user_watches`. At the limit the handler gets `WatchLimit`, once, and each directory that finds no room is polled once a second. Otherwise only a watched path that is a link is polled; with none, the watcher sleeps between notifications. When inotify cannot start at all, the handler is told once and the watcher polls everything. Changes made on another machine (NFS, a folder shared with a VM) are not reported: the handler is told (`RemoteDisk`), and polling finds them. |
| macOS | FSEvents | See "Holes". A process can run 512 streams; one watcher most often keeps 8 or fewer, and at most about 70: a `watch` call that needs new ones adds up to 8, and from 64 on the streams are placed again: the ones whose paths stay go on, and the others are stopped, with the directories below them read again. No stream is put on `/`, `/Users` or the home directory: what could only be watched from there is polled once a second, as on Windows. Each directory on the way to a stream takes an open file, for the events of the kernel when it is renamed or removed, or a disk is mounted on it (a project of 20,000 watched files: about 50); all watchers of a process take half of its limit at most. |
| Windows | `ReadDirectoryChangesW` | Tested on Windows 11 with NTFS only. At most 64 handles; more directories share a handle on a directory above them, and a directory behind a junction keeps one of its own. No handle is put on `C:\`, `C:\Users` or `C:\Users\<name>`: what could only be watched from there is polled. |
| every other target | none: always polling | The cost grows with the number of watched files. On WASI (`wasm32-wasip1-threads`) the directories that the program may use lie below `/`, which is none of them: `/` counts as a directory all the same, so a path below a preopened directory is watched. |

The Rust toolchain is the one in `rust-toolchain.toml`.

## The repository

```
src/
  lib.rs                 the crate docs; re-exports the API
  config.rs  error.rs  event.rs
  watcher.rs             the thread and the queue; picks the backend
  backend.rs             the `Backend` trait: what the engine asks of an OS watch API
  inotify.rs  fsevents.rs  windows.rs  poll.rs     one backend each
  engine/                the engine: watched_paths, check, filter, scan, os_watch, events
  tree/  fs/  path.rs
tests/
  stories/               the promises above: each story on the real disk, and on the
                         simulated disk of `sim/` where its words allow
  sim/                   the engine on a disk in memory (Unix hosts): what the real disk
                         cannot play on demand, with a fuzzer and a cost program; the
                         disk and the backends are in `fixtures/`
  watcher.rs             the thread
  tree.rs                the tree of the engine, alone
  poll.rs                the poll backend on a real directory, with a clock the test moves,
                         and the hash of a file
  inotify.rs  fsevents.rs  windows/     each backend alone, on its OS; `windows/parts.rs`
                         holds the parts of that backend that call no Windows
examples/                watch.rs, dev_server.rs
references/
  design.md                  how it works
bench/                       the comparison with other watchers: a workspace of its own,
                             with the tables of the last full runs in BENCHMARK.md
```

`pathwatch` shows seven names: `Watcher`, `Config`, `Event`, `EventKind`, `EntryKind`, `Error`,
`ErrorKind`. Its other public modules are hidden from the docs. They are there for the tests and
can change in any release.

## Tests

```
cargo test --workspace
```

This runs, on the machine it is called on:

- the engine on a simulated disk, with a fuzzer (`tests/sim`, on Unix hosts),
- the backend of this OS against the real watch API (`tests/inotify.rs`,
  `fsevents.rs` or `windows/os.rs`),
- the watcher thread (`tests/watcher.rs`),
- the tree alone (`tests/tree.rs`), and on a real directory the poll backend with a clock that
  the test moves and the hash of a file (`tests/poll.rs`),
- on Windows, also the parts of its backend that call no Windows (`tests/windows/parts.rs`):
  the backend is compiled there only,
- the stories (`tests/stories`): each once with the backend of this OS and once with polling
  on the real disk, and a story that needs no more than files, directories, links and renames
  also on each simulated backend of `tests/sim` (the note above the promises says which
  of them run where). A story about the delete delay skips polling, which has none.

| Platform | How |
|---|---|
| macOS | `cargo test --workspace` |
| Linux | `cargo test --workspace`. From macOS with OrbStack: `orb bash -c 'source $HOME/.cargo/env; cd <this directory> && CARGO_TARGET_DIR=$HOME/pathwatch-target-inotify cargo test --workspace'`. The tests use the temporary directory of the VM, because inotify does not see changes that macOS makes on the shared folder. |
| Windows | `cargo test --workspace` on a Windows machine. Done on Windows 11 with NTFS. From another OS only a type check is possible: `cargo check --workspace --all-targets --target x86_64-pc-windows-msvc` |
| Android, WASI, FreeBSD, musl, 32-bit Linux | Only a lint of the crate and its tests, as on CI: `cargo clippy -p pathwatch --all-targets --target aarch64-linux-android`, and the same with `wasm32-wasip1-threads`, `x86_64-unknown-freebsd`, `x86_64-unknown-linux-musl` or `i686-unknown-linux-gnu`. The engine with the poll backend was run once under wasmtime on a preopened directory (a probe, not a test). |

The fuzzer (`tests/sim/fuzz.rs`):

```sh
# The quick run: 250 seeds for each backend. It is a part of `cargo test --workspace`.
cargo test -p pathwatch --test sim fuzz::

# The long run, with a range of seeds of your choice. CI runs it on the seeds of the day.
PATHWATCH_FUZZ_FIRST=100000 PATHWATCH_FUZZ_SEEDS=20000 \
  cargo test --release -p pathwatch --test sim fuzz::big -- --ignored

# One seed again, on every backend, with the short story of each failure. A failure of the
# `long` run needs `PATHWATCH_FUZZ_STEPS=5000` too.
PATHWATCH_FUZZ_SEED=1234 cargo test -p pathwatch --test sim fuzz::replay -- --ignored --nocapture
```

More:

```sh
# What the engine costs for big cases. Run it before and after a change and compare.
cargo test --release -p pathwatch --test sim cost:: -- --ignored --nocapture

# macOS: what 20,000 files watched one by one cost the FSEvents backend. It runs alone.
cargo test --release -p pathwatch --test fsevents the_cost_of_single_files -- --ignored --nocapture

# The FSEvents test that fills the stream limit of the process. It runs alone.
cargo test -p pathwatch --test fsevents -- --ignored failed_start

# The inotify tests of the watch limit and of a mount. They need a lowered limit and mounts of
# their own, and run one after the other: they share the limit and see each other's mounts.
unshare -Urm sh -c 'echo 8 > /proc/sys/user/max_inotify_watches &&
  cargo test -p pathwatch --test inotify -- --ignored --test-threads=1'

# Lints and the docs.
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p pathwatch --no-deps
```
