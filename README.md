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
  watcher.watch_all(["/project/src", "/project/package.json", "/project/not-there-yet.json"])?;

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
| `watch(path)`, `watch_all(paths)` | Watches a path: a file, a directory (with everything below it), or a path that does not exist yet. Returns when the path is watched. Watching a path again does nothing. |
| `unwatch(path)`, `unwatch_all(paths)` | Takes back a `watch` of this very path. Nothing is reported for it, with one exception: a link inside a watched directory that this path was followed through is a plain file of that directory again, and that change is reported (promise 12). |
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
  `error.kind()` is one of `InvalidPath`, `WatchLimit`, `PermissionDenied`, `Io`, `Closed`.
  The handler gets one error of a kind for a batch, with one path that shows the problem.

## Options

| `Config` method | Default | Meaning |
|---|---|---|
| `with_ignored(f)` | nothing is ignored | `f(path, kind) -> bool`, where `kind` is `EntryKind::File` or `EntryKind::Dir`. A path for which it returns `true` is never reported. An ignored directory is never read or watched. It must give the same answer for the same input, and if it ignores a directory it must ignore everything below it. It is asked about the paths you watch too (promise 10). |
| `with_debounce(d)` | 50 ms | The quiet time: the disk is checked, and a batch goes out, when no change was noticed for `d`. While changes keep coming, a batch goes out 500 ms after the first change since the last batch, or `4 × d` after it if that is longer, plus the time that the check of the disk takes. Zero: no waiting. The most is a year, also for the poll interval. A `watch` or `unwatch` call ends the quiet time; a poll that runs meanwhile does not, but a change it finds by itself is reported. Under polling there is no quiet time: a change is found by the next round. |
| `with_delete_delay(d)` | 100 ms | How long a `Delete` waits for the path to come back. A file that is removed and written again within `d` is one `Update`; a real delete is reported `d` after the OS said that the path is gone. Zero: no waiting, and such a file is a `Delete` and a `Create`. The most is a year. |
| `with_polling(bool)` | `false` | Read the disk on a timer and do not use the watch API of the OS. For network drives and other places where the OS reports no changes. |
| `with_poll_interval(d)` | 100 ms | The shortest pause between two rounds of polling. The least is 1 ms. After a slow round the pause is longer: nine times as long as the looks of the round took, plus the time of the reads of the files written in the last two seconds (on Windows a read right after a write waits for the virus scanner), and five seconds at most. |

The settings cannot be changed after the watcher is made.

## Promises

Each of these has a test on the real disk (`crates/pathwatch/tests/disk`; the first lines of
`promises.rs` say which). They run on every OS, with three exceptions: the test with sockets
(1) runs on Unix only; the tests that rename a directory above a watched one (6, 13) run on
Windows with polling only, since Windows refuses the rename while a handle is open; and a
test that makes a symbolic link (12, and one each of 6 and 13) stops early on Windows when the
user may not make links, as the tests with another Unicode form of a name (13) do on a disk
that tells the forms apart (every disk but the usual one of macOS).

1. **A watched path is a file, a directory or nothing.** A file is anything that is not a
   directory: also a socket, and a symbolic link inside a watched directory.
2. **Events are the difference to the disk.** The last event for a path tells its state: after
   `Create` or `Update` the file is there, after `Delete` it is not. When things are quiet, the
   files you were told about are the files on the disk.
3. **`Update`.** `touch` is an `Update`; `chmod` is not. An atomic save (write a temporary
   file, rename it over) is an `Update`, with no `Delete` and no `Create`. So is a file that is
   removed and written again within the delete delay (100 ms). There can be more `Update`
   events than writes. A file that is replaced again and again without a pause by a save that
   leaves the name free for a moment (`ReplaceFileW`, the save of .NET's `File.Replace`) can
   get a `Delete` and a `Create` when both looks of the delete delay fall into such moments
   (measured on Windows: 62 of 10,000 saves).
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
   `watch_all` call are watched. On Windows this also holds for a name that Windows does not
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
    directory is not read or watched. The filter applies to watched paths too. A watched path
    that it ignores as a file and as a directory is not watched at all: no events, no error,
    no cost. A watched path that it ignores only for the kind that is there now counts as
    missing until its kind changes. A watched path is dropped when the filter ignores the
    directory that holds it or, while directories above it are missing, the lowest one that
    exists: a file that was reported for it gets its `Delete`, then the path is forgotten as
    `unwatch` of it would do it; `watch` it again to have it looked at again. A filter must ignore everything below a directory that it
    ignores: then no path is ever dropped. One that only looks at the last name of the path
    gives results that depend on which directories exist.
11. **Nothing beside a watched path.** A file next to a watched file is not reported.
12. **Symbolic links.** A watched path that is a link, or that goes through one, is followed,
    and its events carry the spelling you gave. A link inside a watched directory is a file: it
    is not followed. While a watched path is at or goes through such a link, the link is
    followed for the directory too: the `watch` of that path reports the link as gone, and
    what is behind it is the baseline of that `watch`; the `unwatch` of it reports the link as
    a new file and forgets what was behind it without a word.
13. **Spelling.** On a disk that takes two spellings of a name for one (letter case, Unicode
    form: the usual disks of macOS and Windows, and on Linux a folder of such a system, a FAT
    or SMB disk, a directory with the casefold flag), a watched path is the text you gave:
    what is at it is what the OS finds there, and its events carry your spelling. A name below
    a watched directory is spelled as on the disk. Every watched path is a question of its own.
    Two watched paths that are two spellings of one file both get their events. So does a
    watched path in another spelling inside a directory that you watch too: the directory
    reports the file under the name of the disk, and the path under its own.

## Holes

What is not reported, or reported wrongly. All of these are known and not fixed.

- **A writer that keeps a file open.** macOS reports nothing until the file is closed; the
  same for a write through `mmap`. Linux reports no write through `mmap` at all, also after
  the file is closed: inotify has no word for it. On Windows the directory lists the new size and time only
  when the file is closed (measured on NTFS); whether the write is reported before that was
  not checked. On Windows a write through a mapped view of a file that exists is not reported
  at all, as on Linux, also after the file is closed, and neither is a write through a handle
  that was opened by the index of the file (`OpenFileById`); polling finds the second.
- **Windows: a second write of the same size by a writer that keeps the file open**, in the
  same step of the clock of the time stamps (1 to 16 ms), is not found by a look: the disk
  gives the file a new number only for its first write (measured on NTFS). It is reported
  when Windows reports the write itself, and at the latest when the file is closed.
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
- **A write through another hard link** of a watched file is not reported.
- **macOS: a socket or a FIFO that is made** is not reported (macOS 26; macOS 27 reports it).
  It is found when something else makes the watcher look at its directory. Its removal is
  reported.
- **A rewrite of the same size that puts the old modification time back** is not reported on
  macOS and with polling. Linux reports it.
- **A disk whose clock is behind the clock of the watcher** (a network disk, a shared folder
  of a VM) has no young files: a file written just now looks old by the time of the watcher.
  A second write of the same size inside one step of the time stamps of that disk is not
  found by a look there; the OS reports it where it reports writes (Linux, Windows).
- **A mount over a watched directory** is not noticed until something else makes the watcher
  look there.
- **A path longer than the OS allows** gets no events: `watch` takes it, the handler is told
  once (`Io`), and the path counts as missing.
- **A directory on a network drive, or shared with another machine** (NFS, a folder of the
  host in a virtual machine or a container): changes that the other machine makes are not
  reported, and nothing tells the watcher so. `with_polling(true)` finds them.
- **A watched path that is spelled with a look-alike character**, where the disk has plain
  letters, or the other way round: `ß` for `ss`, the long s for `s`, the Kelvin sign for `K`,
  a ligature like `ﬁ` for `fi`, a CJK compatibility ideograph for its ideograph. The disks of
  macOS take these for one name. Such a path gets no events on macOS. With polling it works.
  NTFS takes the two for two names (measured), so nothing is missed there.
- **Linux: a disk that takes two Unicode forms of a name for one, and tells the case of
  letters apart** (a case-sensitive volume of a Mac, shared into a Linux machine): it is taken
  for a disk that tells every spelling apart. A name below a watched directory that is renamed
  to its other Unicode form can stay known under the old one.
- **A young file above 16 MiB, or one that cannot be read, under polling:** it gets one
  `Update` when it is 2 s old, also when nothing changed, and also when it was written just
  before `watch`.
- **The link of a watched path changes** (it points somewhere else now): found by a poll,
  within a second, or within the poll interval when polling is on.
- **A watched path that is a link to a file**: a write to the file behind it is found by the
  same poll, within a second, with every backend. And a rewrite of the same size that puts the
  old modification time back is not reported for it on Linux either.
- **macOS, Windows: the directories above a watched path** are polled once a second. When
  one of them is made, or made again, the events can come up to a second late. On Windows the
  same holds when one is removed. On macOS a rename or a removal of such a directory is found
  at once, also a rename that only changes the case of letters (`src` to `Src`), and one that
  is taken back right away (`docs/design.md` 7.2).
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
  directory; it looks once more at the next flush, within the poll interval, and reports the
  link then. Until then a watched path at or through the link gets no events.
- **Windows: more than 64 directories** share handles. A watched path whose directory is then
  spelled in another way than the disk (more than the case of letters, or a short 8.3 name)
  gets no events.
- **Windows: a name with `\` or `/` in it**, which only a disk that another system filled can
  hold, is not reported: no path names such an entry.
- **Where the OS watches are put depends on what was watched and unwatched before.** A stream of
  macOS stays where it is until the streams are replaced, or until nothing watched is left below
  it. Nothing is missed.
- **The Windows backend is young.** Its tests pass on Windows 11 with NTFS, and nothing else
  was tried: no network share, no ReFS, no older Windows. See `docs/windows-open-points.md`.
- **Android was never run.** It is set to use the same code as Linux, and only type-checked.

## Timing

- **Normally** an event comes within the quiet time (50 ms) plus what the OS takes to tell
  about a change (about 1 to 15 ms). For a watched path that is a link to a file, a write to
  the file behind it is found by the poll of the links: within a second.
- **With polling** a change is found by the next round: within the poll interval (100 ms), or
  later after a slow round: the pause is nine times as long as the looks of the round took,
  plus the time of the reads of the files written in the last two seconds, and five seconds
  at most.
- **Under a flood** of changes a batch goes out 500 ms after the first change since the last
  batch, plus the time that the check of the disk takes: a check with thousands of changed
  paths takes a second and more (measured: 30,000 files made by eight threads, the longest
  gap 2 s).
- **A `Delete`** comes one delete delay (100 ms) after the OS said that the path is gone, once
  the path was found empty twice; under a flood it comes with the next batch. It comes at once
  when a directory is found where the file was, or a file where the directory was, or when a
  file below a watched directory is renamed to another spelling of its name (another letter
  case, on macOS and Windows). An error of the disk that says nothing is never a `Delete`;
  "permission denied" counts as missing.
- **Lost notifications** (the OS dropped some, or more than 16 MiB of them waited for the
  watcher) are repaired by reading the disk again. These scans are paced: after one that took
  `t`, the next one waits `9 × t`.

## Coming from notify, chokidar or @parcel/watcher

| What | In pathwatch | Note |
|---|---|---|
| watch | `watch(path)`, `watch_all(paths)` | Offered. There is no recursive mode: a directory is always watched with everything below it. A path may be a file, or not exist yet. No globs. |
| unwatch | `unwatch(path)`, `unwatch_all(paths)` | Offered differently: it takes back a `watch` of the same path. It cannot cut a part out of a watched directory; use the ignore filter. |
| close | `close()`, or drop the watcher | Offered. |
| ignored | `Config::with_ignored(f)` | Offered differently: one function, set when the watcher is made. No glob list, no change later. It is asked about watched paths too: a path that it ignores as a file and as a directory is not watched. It must ignore everything below a directory that it ignores. |
| polling | `with_polling`, `with_poll_interval` | Offered. On top of that, a directory that the OS cannot watch is polled without an option, and so is everything when the watch API cannot start or stops: then at the poll interval of the settings. |
| debounce | `with_debounce` | Offered, and built in. Left out: waiting until a file stops growing (chokidar `awaitWriteFinish`). A slow writer gives more than one `Update`. |
| atomic writes | `with_delete_delay(d)` | Offered differently: an atomic save (write a temporary file, rename it over) is always one `Update`, with no option. A file that is removed and back within the delete delay (100 ms, as chokidar's `atomic`) is one `Update` too. |
| ignore initial | always on | Left out: what exists at `watch` time is never reported. Read the directory yourself if you need the list. |
| directory events | none | Left out: a directory is reported as its files, so a moved directory needs no walk on your side. |
| rename events | none | Left out: a rename is a `Delete` and a `Create`. The watcher never reads what the OS says happened; it compares the disk with what it told you. |
| symlinks | no option | Offered differently: a watched path is followed, a link inside a watched directory is a file. Following links inside a watched directory is left out. |
| errors | `Err(error)` to the handler | Offered. The watcher keeps running after every error. |
| depth, `since` | none | Left out: no depth limit and no changes from before the call. |

## Platforms

| OS | Watch API | Limits |
|---|---|---|
| Linux, Android | inotify | One kernel watch for each directory, so a big tree needs a high `fs.inotify.max_user_watches`. At the limit the handler gets `WatchLimit` and the directory is polled once a second. When inotify cannot start at all, the handler is told once and the watcher polls everything. Changes made on another machine (NFS, a folder shared with a VM) are not reported: use polling there. |
| macOS | FSEvents | See "Holes". A process can run 512 streams; one watcher most often keeps 8 or fewer, and at most about 70: a `watch` call that needs new ones adds up to 8, and from 64 on all of them are replaced. It also keeps one file open for each directory at and above a place that a stream watches (about ten for a project; 64 such places, and more only for paths that have no directory in common where a stream may stand), and stops at half of what the process may open: beyond that the handler gets `WatchLimit` and the directories are polled once a second. |
| Windows | `ReadDirectoryChangesW` | Tested on Windows 11 with NTFS only (`docs/windows-open-points.md`). At most 64 handles; more directories share a handle on a directory above them, and a directory behind a junction keeps one of its own. No handle is put on `C:\`, `C:\Users` or `C:\Users\<name>`: what could only be watched from there is polled. |
| every other target | none: always polling | The cost grows with the number of watched files. On WASI (`wasm32-wasip1-threads`) the directories that the program may use lie below `/`, which is none of them: `/` counts as a directory all the same, so a path below a preopened directory is watched. |

The Rust toolchain is the one in `rust-toolchain.toml`.

## The repository

```
crates/
  pathwatch/                 the crate, and the only one that is published
    src/
      lib.rs                 the crate docs; re-exports the API
      config.rs  error.rs  event.rs
      watcher.rs             the thread and the queue; picks the backend
      backend.rs             the `Backend` trait: what the engine asks of an OS watch API
      inotify.rs  fsevents.rs  windows.rs  poll.rs     one backend each
      engine/                the engine: watched_paths, check, filter, scan, os_watch, events
      tree/  fs/  path.rs
    tests/
      disk/                  the promises above, on the real disk
      watcher.rs             the thread
      tree.rs                the tree of the engine, alone
      ahead.rs               the engine with polling: a file with a time ahead of the clock
      stopped.rs             the engine after its backend stopped
      inotify.rs  fsevents.rs  windows.rs     each backend alone, on its OS
      windows_parts.rs       the parts of the Windows backend that call no Windows (on Windows only)
    examples/                watch.rs, dev_server.rs
  pathwatch_sim/             fixtures for tests, not published
    src/                     a disk and backends in memory
    tests/                   the engine tests on them, with a fuzzer and a cost program
docs/
  design.md                  how it works
  windows-open-points.md     what to check on a Windows machine
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

- the engine on a simulated disk, with a fuzzer (`crates/pathwatch_sim/tests`, on Unix hosts),
- the backend of this OS against the real watch API (`crates/pathwatch/tests/inotify.rs`,
  `fsevents.rs` or `windows.rs`),
- the watcher thread (`tests/watcher.rs`),
- the tree alone (`tests/tree.rs`), and on a real directory the hash of a file (`tests/hash.rs`)
  and the poll backend with a clock that the test moves (`tests/ahead.rs`, `tests/pacing.rs`),
- on Windows, also the parts of its backend that call no Windows (`tests/windows_parts.rs`):
  the backend is compiled there only,
- the promises on the real disk, each once with the backend of this OS and once with polling
  (`tests/disk`; the note above the promises says which of them run where).

| Platform | How |
|---|---|
| macOS | `cargo test --workspace` |
| Linux | `cargo test --workspace`. From macOS with OrbStack: `orb bash -c 'source $HOME/.cargo/env; cd <this directory> && CARGO_TARGET_DIR=$HOME/pathwatch-target-inotify cargo test --workspace'`. The tests use the temporary directory of the VM, because inotify does not see changes that macOS makes on the shared folder. |
| Windows | `cargo test --workspace` on a Windows machine. Done on Windows 11 with NTFS; `docs/windows-open-points.md` says what was not tried. From another OS only a type check is possible: `cargo check --workspace --all-targets --target x86_64-pc-windows-msvc` |
| Android, WASI, FreeBSD, musl, 32-bit Linux | Only a lint of the crate and its tests, as on CI: `cargo clippy -p pathwatch --all-targets --target aarch64-linux-android`, and the same with `wasm32-wasip1-threads`, `x86_64-unknown-freebsd`, `x86_64-unknown-linux-musl` or `i686-unknown-linux-gnu`. The engine with the poll backend was run once under wasmtime on a preopened directory (a probe, not a test). |

The fuzzer (`crates/pathwatch_sim/tests/fuzz.rs`):

```sh
# The quick run: 250 seeds for each backend. It is a part of `cargo test --workspace`.
cargo test -p pathwatch_sim --test fuzz

# The long run, with a range of seeds of your choice. CI runs it on the seeds of the day.
PATHWATCH_FUZZ_FIRST=100000 PATHWATCH_FUZZ_SEEDS=20000 \
  cargo test --release -p pathwatch_sim --test fuzz big -- --ignored

# One seed again, on every backend, with the short story of each failure. A failure of the
# `long` run needs `PATHWATCH_FUZZ_STEPS=5000` too.
PATHWATCH_FUZZ_SEED=1234 cargo test -p pathwatch_sim --test fuzz replay -- --ignored --nocapture
```

More:

```sh
# What the engine costs for big cases. Run it before and after a change and compare.
cargo test --release -p pathwatch_sim --test cost -- --ignored --nocapture

# Also prints how long `watch` takes for 20,000 watched files.
cargo test -p pathwatch --test disk -- --nocapture

# The FSEvents test that fills the stream limit of the process. It runs alone.
cargo test -p pathwatch --test fsevents -- --ignored failed_start

# The inotify test of the watch limit. It needs a lowered limit.
unshare -Ur sh -c 'echo 8 > /proc/sys/user/max_inotify_watches &&
  cargo test -p pathwatch --test inotify -- --ignored'

# Lints and the docs.
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p pathwatch --no-deps
```
