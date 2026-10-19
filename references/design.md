# pathwatch design

## 1. Goal

`pathwatch` watches **paths** and reports **file events**: `Create`, `Update`, `Delete`. It
never reports a directory.

It is built for rolldown (`this.addWatchFile`, module files, dev and build watch): many single
files, some directories, and paths that do not exist yet. The model follows chokidar: keep a
tree in memory, do not trust what the OS says happened, ask the disk.

The contract is section 3. What is left out is section 9. The words that the code uses are
section 10.

## 2. Public API (`pathwatch`)

```rust
let watcher = Watcher::with_config(config, |result| match result {
  Ok(events) => for event in events { /* event.path, event.kind */ },
  Err(error) => eprintln!("{error}"),
})?;
watcher.watch(&["/project/src/index.js", "/project/content", "/project/not-there-yet.json"])?;
watcher.unwatch(&["/project/content"])?;
watcher.close();
```

| Name | What it is |
|---|---|
| `Watcher::new(handler)`, `with_config(config, handler)` | Start the watcher thread and the backend. They fail only when the thread cannot start. When the watch API of the OS cannot start, the handler is told once (`WatchLimit` or `Io`) and the watcher polls everything. |
| `watch(paths)` | Register a batch and return when every path is watched and its state is known. What exists then is the **baseline** and is not reported. A path that is watched already is skipped. |
| `unwatch(paths)` | Take back a `watch` of the very same paths, and report nothing but the change of a link inside a watched directory that is a plain file again (5.7). |
| `close()` | Stops the backend and waits for the thread. After it the handler is not called again, and `watch` and `unwatch` return `Closed`. A check of the disk that runs ends at its next step, so the wait is one look at most. Dropping the watcher calls it. |
| the handler | `FnMut(Result<Vec<Event>, Error>) + Send + 'static`. It runs on the watcher thread. A call of the watcher from inside it does not wait: it takes effect when the handler has returned. |
| `Event { path, kind }` | `EventKind`: `Create`, `Update`, `Delete`. |
| `Error` | `kind()` (what the caller can do about it), `path()` (one path that shows the problem; none when the problem is the watcher itself), `source()` (what the OS or the backend said). `ErrorKind`: `InvalidPath`, `WatchLimit`, `PermissionDenied`, `RemoteDisk`, `Io`, `Closed`. One error of a kind is handed over for a flush. Its text is these three in a row (see below). |
| `Config` | `with_ignored(f)` (5.11); `with_debounce(d)`: the quiet time, none on macOS, 10 ms on Windows and 20 ms on Linux (5.13); `with_delete_delay(d)`: the delete delay, 10 ms on macOS and Windows and 50 ms on Linux, none under polling, zero turns it off (5.9); `with_polling(bool)`; `with_poll_interval(d)`: 100 ms, at least 1 ms, used when the watcher polls everything. A time of the settings is a year at most (`Engine::new`): `Duration::MAX` says "never", and is no time that can be added to a clock. |

`Watcher` is `Send + Sync`.

Paths (`clean` in `watcher.rs`, then `path::normalize`): a relative path is joined to the
current directory; on Windows a path with a drive and no root (`C:name`) too, when the current
directory is on that drive, and a path that starts with two separators is a share or nothing,
never joined. `.`, repeated separators and a trailing separator are dropped, and `..` takes
away the name before it, all by text. This clean form is the watched path, and every event path
starts with it, byte for byte. A path that is empty, has a NUL, goes above the root, or is a
top directory (`/`, `C:\`, `\\server\share`) cannot be used: the call returns `InvalidPath`
with the first such path, and the other paths of the batch are still watched. On Windows the
same holds for a name that Windows does not take as it is (`path::windows_rules::is_name`): it
drops a dot or a space at the end of a name, reads `:` as the start of a stream, and has no
name with a control character or one of `< > " | ? *`. Such a path was watched before, and
never got an event (measured): no record of the OS spells it so.

A path that does not exist is fine. A path that cannot be watched or read right now stays
registered, the handler gets the error, and the path is polled.

The text of an error is its kind, its path and its source, each said once:

```text
permission denied: /p/secret: Permission denied (os error 13)
the OS has no room for one more watch, polling instead: /p/src: inotify has no watch left for this user (raise fs.inotify.max_user_watches)
input or output error: the OS watcher stopped, so every path is polled: Bad file descriptor (os error 9)
```

The source of a limit is written by the backend that met it, so it names that very limit and
what to raise. (Before, the kind named one limit for each OS, also when another one was met.)
An error
without a path is about the watcher: its backend did not start or has stopped, and the source
says which, and that every path is polled now.

Not an option: with a native backend the poll of what the OS does not watch runs once a
second. `Config` turns `loose_spelling` on for macOS and Windows: there every disk takes two
spellings of a name for one. Elsewhere every directory tells them apart (5.2).

## 3. The contract

Three definitions say all of it.

1. **A watched path stands for a set of files.** If a file is at the path: that file. If a
   directory is there: every file below it, at any depth, without what the ignore filter cuts
   off. If nothing is there: no file. A file is anything that is not a directory. A watched path
   that is a symbolic link, or goes through one, is followed; a link below a watched directory
   is a file.
2. **Events are the difference between the told set and the disk.** The **told** set is what
   the handler believes: the baseline of each `watch`, plus every `Create`, minus every
   `Delete`. When the watcher has caught up, the told set is the set of files that the watched
   paths stand for.
3. **A batch has at most one event for a path.** `Create`: not told, and a file is there.
   `Delete`: told, and no file is there. `Update`: told, a file is there, and it is another
   file or may have other content. So the last event for a path tells its state.

More `Update` events than needed are allowed. A missed change is not.

The table is these rules in examples. `R` is a watched path.

| # | What happens | Events |
|---|---|---|
| S1 | in-place write to file `R` | `Update R` |
| S2 | atomic save of `R` (write tmp, rename over) | `Update R` |
| S3 | `R` deleted, created again | `Delete R`, `Create R`. Inside the delete delay: `Update R`, or nothing when the same file is put back by a rename |
| S4 | `R` missing, a file is created there, then written | `Create R`, `Update R` |
| S5 | `R` missing, a directory is created there, then a file inside | `Create R/f` |
| S6 | `R` and several directories above it missing, `mkdir -p` + file at `R` | `Create R` |
| S7 | file `R` deleted, a directory with files appears at `R` | `Delete R`, `Create R/x …` |
| S8 | directory `R` removed, a file created at `R` | `Delete R/x …`, `Create R` |
| S9 | new file in directory `R`; mkdir + file written right away | `Create R/f`; `Create R/sub/f` |
| S10 | a tree is moved into directory `R` | `Create` for each file |
| S11 | a sub-directory is moved out of `R`, or `rm -r` | `Delete` for each told file |
| S12 | directory `R` removed or renamed away; then it is back | `Delete` for each told file; `Create` for each file. Inside the delete delay: only what differs |
| S13 | file `R = proj/lib/a.js`; `mv proj/lib proj/lib2`; and back | `Delete R`; `Create R` |
| S14 | an ignored directory below `R` | nothing from inside it; ignored → visible name: `Create` for each file; visible → ignored name: `Delete` for each told file |

## 4. Architecture

```
            signals                        events
 backend  ─────────►  watcher thread  ────────────►  handler
 (OS API)   queue     engine: tree + check
     ▲                    │
     └─ watch(key, dir) ──┘
```

| Module (`src/`) | Holds |
|---|---|
| `config`, `error`, `event`, `watcher` | The API. `watcher.rs` has the one thread and its queue, and picks the backend. |
| `engine/`, `tree`, `fs`, `path` | The engine, the tree, the disk as the engine sees it (`Fs`), path cleaning. No thread, no clock, no OS watch API. |
| `backend` | The `Backend` trait and its types. |
| `inotify`, `fsevents`, `windows`, `poll` | One backend each, in one file. Of the crate they use `backend`, and the calls of the system in `fs` that the engine makes too. |

The modules that are not the API are `pub` but hidden from the docs: the tests need them. The
tests of the engine on a simulated disk, and their fixtures, are in `tests/sim` (section 8).

- A **backend** sets up OS watches when the engine asks, and its thread copies raw OS
  notifications into the queue. On the watcher thread the backend turns a raw notification into
  **hints**.
- A hint never says what happened. It says where to check: `Changed { dir, rel }`,
  `Written { dir, rel }`, `Lost`. `dir` is the key that the engine gave when it asked for the
  watch, so the two sides never have to agree on how a path is spelled.
- The engine collects hints as **marks** until no hint came for the quiet time. Then it
  **flushes**: it reads the disk at the marked places, compares with the tree, and hands the
  events over. A hint that comes twice, or late, costs one `lstat` and changes nothing.
- **Watch, commit, scan.** The engine asks the backend to watch a directory, calls `commit`, and
  only then reads the directory. A change before the watch is found by the scan. A change after
  the watch makes a hint. Nothing fits in between.

## 5. The engine (`engine/`, `tree/`)

The words, each for one idea:

| Word | Meaning | In code |
|---|---|---|
| watched path | a path given to `watch` and not taken back by `unwatch` | `Node::is_path` |
| registered name | a watched path, or a name on the way to one | `Node::is_registered` |
| node | one name the tree knows; nothing, a file or a directory is there | `Node`, `Kind` |
| key | the name of a node, also for the backend | `DirKey` |
| root | the one fixed node above the top directories (`/`, `C:\`) | `Tree::ROOT` |
| complete / partial | a directory node tracks every entry / only its registered names | `Dir::complete` |
| told | the handler believes that a file is at the path | `Node::told` |
| baseline | what a `watch` call found; it is told without an event | `Node::baseline` |
| hint | where a backend says to check | `Hint::{Changed, Written, Lost}` |
| loss report | a hint that says "notifications were lost": everywhere (`Lost`), or at and below one directory (`Changed` with an empty `rel`) | `Mark::lost`, `Tree::lost` |
| mark | "check this node at the next flush" | `Mark { deep, written, lost }` |
| check | compare one node with the disk | `Engine::check`, `apply` |
| scan | check every node of one directory; deep: also everything below; names: only the nodes whose names the tree and a listing do not both have | `Engine::scan`, `Depth` |
| flush | check the marks, then rounds of commit and scan; then the events go out | `Engine::flush`, `check_marks`, `round`, `finish` |
| touched | a node that a flush changed; its event is read at the end of the flush | `Engine::touched` |
| want | what the engine asks a backend about a directory | `Want::{Names, Contents}` |
| coverage | what the backend answers | `Coverage::{Watched, WatchedTree, NotWatched, Failed}` |
| polled | no OS watch covers the directory, so the timer scans it | `Watch::Polled` |
| held | a look found nothing at the node; what the tree has there, and below, stays until the second look | `Node::is_held`, `Tree::hold` |
| delete delay | the time from the hint to the second look: 10 ms on macOS and Windows, 50 ms on Linux, none under polling | `Options::delete_delay` |
| young | a file whose modification time is too close to the check to trust | `Content` |
| quiet time | the debounce: how long no hint must come before the disk is checked | `Options::debounce` |
| rest | the pause after work that no hint asked for by name: a poll, the scans after loss reports | `REST`, `rest_until` |
| cover | FSEvents only: a directory that the engine watches and a stream reports | `Covers` in `fsevents.rs` |
| stream path | FSEvents only: a directory that a stream was started for; it reports the whole tree below | `Streams::paths` in `fsevents.rs` |

`tree/mod.rs` holds the data, and `engine/` has one file for each job (each file says which in its first lines).

### 5.1 The tree

The tree is a thin copy of the directory tree. **One node is one name**, and one field of the
node says what is there: nothing, a file, or a directory (`Kind`). A path that changes its kind
changes that field. Nothing moves from one place to another.

There is a node for

- the **root**: it has no path, is never listed and is never given to the backend. It always
  exists and is polled, so a top directory has a parent that checks it,
- every **registered name**. Such a node stays while nothing is on disk at its path,
- every file and every directory that was found at or below a watched directory and that the
  filter does not ignore,
- a name that waits for a check (it has a mark),
- a name where a file is gone, until the end of the flush has told the handler.

A node that is none of these is freed (`Tree::prune`).

How it is stored (only `tree/mod.rs` knows it): all nodes live in one arena. A key is an index and a
generation, so an old key never names a new node. The arena and the names are kept in blocks of
64 KiB, and a full block never moves: they grow with no copy of what is there. (Grown as one block
each, they were held twice for a moment at each growth, and macOS keeps the pages that are freed:
the heap of a watcher of 50,000 files went up to 8.0 MiB for 4.3 MiB in use, and its footprint with
it. In blocks: 4.8 MiB, and 14 MiB allocated in all instead of 39.) A node has the list of its
children, and one table for the whole tree finds a child by (parent, name). Small tables next to the
arena hold what only a few nodes have: the link of a registered name (5.14), the content hash of a
young file (5.8), and the time of a hold (5.9).

A tree that gets smaller gives its memory back (`Tree::give_back`). A node never moves,
because its key is its place. So only the end of the arena can go, as far as no node lives
there: one node that was made late keeps the arena as long as it is, and then only the names
and the table get smaller. Measured with a million nodes (94 bytes each, 91 MiB): when all of
them go, nothing stays; when the node that was made last stays, 71 MiB stay. A tree that goes
and comes back pays for its memory again, as the first time (54 ns for a node, not 12).

What a watcher holds, measured on macOS (the heap of the process, by `vmmap`): 0.1 MiB with nothing
watched; 4.3 MiB for one watched directory with 50,000 files in 5,000 directories (86 bytes for a
file); 3.0 MiB for 20,000 files in 2,000 directories that are each watched by their own path (150
bytes for a file: the backend keeps the real path of each directory twice). While a `watch` call
runs, the heap holds its paths once more, for the thread of the watcher. The lists of a flush keep
their memory for the next flush only up to 16 KiB each (`emptied`): a longer one is given back when
the flush ends. The usual work of a watcher that runs for days asks for no memory: a `watch` of
paths that are watched already (in their clean form), an `unwatch` of a path and a poll allocate
nothing, and a flush allocates for the paths of its events only (`tests/sim/allocations.rs` keeps
the budgets).

**`complete` is a rule.** A directory node is complete when its path is a watched path, or
when its parent is complete, and in both cases the filter does not ignore it as a directory.
Every check that finds the directory on disk works it out. The tree can be behind the rule for a
while: a directory that becomes a watched path is complete in the tree after its next look.
Where it matters, the rule itself is asked (`Engine::tracks_every_entry`): in `unwatch`, and in
the check of a name that is not registered in a partial directory. It uses the answers of the
filter that the nodes keep, and it asks nothing where the tree has no directory.

A **held** node (5.9) still has the file or the directory that was there, and so has every node
below it. But no directory is on disk at a held node: no hint goes into it, and nothing below
it is checked or scanned.

Invariants (`Engine::check_invariants` and `Tree::check_links`; the simulator checks them after
every engine call):

1. Every node but the root has a parent that has it in its list, and the table finds it.
2. Only a node whose kind is "directory" has a file or a directory below it.
3. The told set is exactly the nodes whose kind is "file": the files on disk, and the files
   whose `Delete` waits at or below a held node. `changed`, `baseline` and `added` are set only
   inside one call.
4. A held node has a file or a directory, and something brings its second look: its time in
   the queue, or a mark that waits for the disk to answer.
5. A node with nothing at its path is kept only while it is registered or has a mark. In a
   partial node, a file is known only if it is a watched path, and a directory only if a watched
   path lies below it. The one exception is a partial node that the rule makes complete and
   whose look is still to come.
6. A directory that was asked for names only holds no registered name with a file or nothing
   at it (5.10). Such a name would have no watch that tells when something comes there. The
   exception: a name whose first check waits for the disk to answer (then a mark waits on
   the node or above it).
7. A directory that relies on a watch above (`FromAbove`) has one there. The exception is a
   look that waits for the disk to answer: then a mark waits on the node or above it.

6 and 7 say nothing about what is below a held node: nothing is known there.

### 5.2 From hint to mark

`Changed { dir, rel }` and `Written { dir, rel }` (`Tree::mark_below`):

1. Drop the hint if `dir` names no node.
2. If no directory is on disk at the node, go up to the nearest node above that is a directory
   on disk, and put a deep mark on its child that leads to `dir`. Done.
3. An empty `rel` is the directory itself: a deep mark on its node. It is a loss report for
   that place (the watch is gone, the directory was moved, or notifications were lost), so the
   mark says `lost`, until a hint that names the path joins it. Done.
4. Walk down from the node through the names of `rel`. At node `X` with the next name `c`:
   - `X` is partial and it has no registered child `c`: the hint is dropped, or "another
     spelling" below applies. So a flood from a place that no watched path can see ends here
     after one lookup for each name.
   - `c` is not the last name and the child `c` is a directory on disk: go into it.
   - otherwise: mark the child `c` and stop. If `X` has no such child yet, a node is made for
     it. The mark is `written` when the hint is `Written` and `c` is the last name. In every
     other case it is `deep`. So a hint from deep inside a place the tree does not know (a tree
     that was moved in, an ignored directory) ends at the highest name that is not known.
   - At each `X`, the other children that `c` can mean (below) are marked too, or walked into
     when `c` is not the last name. A child that differs from `c` in the case of ASCII letters
     only is the entry that was written, when every disk is loose and the backend says
     "written" for a write and nothing else (`Tree::twins_share_writes`, from
     `Options::loose_spelling` and `Backend::written_is_a_write`): its mark says `written` as
     the hint does, so a rewrite that leaves the fingerprint as it was is an `Update` under
     every watched spelling. inotify says "written" for a file that was created or moved in
     too, so there, and for every other child (a guess), the look decides.

Two marks for one node become one. `Lost` sets a flag: the next flush that the pacing allows
(5.13) scans everything. A hint that left work moves the flush to one quiet time later (5.13).
A hint that was dropped moves nothing.

**Another spelling.** The usual disks of macOS and Windows find `App.js` when the entry is
`app.js`, and the same for two Unicode forms of a name. Such a disk is called loose here, and
one that tells the two apart strict. One rule decides everything here:

> pathwatch reports what a program would see that asks the OS about each watched path by
> itself, again and again: `lstat` for a watched file, the listing for a watched directory.

So there are two kinds of names. A **registered name** (a watched path, or a name on the way
to one) is the text that its caller gave. What is at it is what `lstat` finds there, under
any spelling, and its events carry the spelling of the caller. A **listed name** (found below
a watched directory) is the name as the disk spells it, and an entry has one such name. Every
watched path is a question of its own: two watched paths that are two spellings of one file
both get their events, and a watched path in another spelling inside a watched directory gets
its events next to the ones that the directory reports under the name of the disk. A symbolic
link is treated in the same way (a registered link is followed, a listed one is a file).
Polling works like this by itself, and the OS watches have to give the same events.

The other choices were weighed and dropped. Events under the name of the disk (notify does
that) would make every caller turn its own paths into the names of the disk before it can
match an event: rolldown matches by bytes and keeps the spelling of the import, so it would
never rebuild. A path that counts as missing unless it is spelled like the disk (Watchman)
gives no events and no error for a drive letter or a command line in another case. Whether an
import may be spelled in another way than the disk is a question for the resolver, not for
the watcher.

Three facts are behind the mechanics, the first two measured with FSEvents on APFS:

- The OS names an entry that is there as the disk spells it, whatever spelling the writer
  used.
- The OS names an entry that is gone (removed, or renamed away) as the caller spelled it.
- `lstat` finds an entry under every spelling of its name. The disk can say how it spells the
  name of one entry (`Fs::name_on_disk`: `getattrlist` on macOS, 2 µs; `FindFirstFileW` on
  Windows), and a listing names each entry in that one way.

**Which disk is loose.** `Options::loose_spelling` says it for the whole watcher. It is on
for macOS and Windows, where every disk takes two spellings of a name for one (a
case-sensitive APFS volume still takes two Unicode forms of a name for one, measured, so what
`pathconf` says about the case proves nothing): there every directory is loose, and no disk
is asked. It is off everywhere else: there every directory is strict, and no look asks how a
name is spelled. A Linux machine can have a loose disk next to its own (a folder of a Mac,
measured with OrbStack: `lstat` finds `APP.JS` for `app.js`, and no flag says so; a FAT or
SMB disk; a directory with the casefold flag). Taking it for a strict one gives wrong events
below a watched directory (section 9); a watched path in another case of its ASCII letters
still gets its events, since a hint for the one name marks the other (below). A rule that
found the kind of each directory out by its looks (an entry tried in the other case of its
letters, with a third look against a change in between, and a proof taken back by a listing)
was built and taken out again: 250 lines for that one case, with holes of its own (a disk
that gives each spelling a number of its own, vboxsf and sshfs; a disk that folds Unicode
forms and not the case).

A second name is evidence only while its node is there. So when a look leaves nothing at a
name (the filter ignores it, or it is gone) and the node goes, the marked nodes that can be
the same name are looked at first, as names that no listing gave (`Engine::check`). The order
of two looks in one flush must not decide.

In a loose directory the question is asked for a new name and for a name with a twin, not
for the edit of a file that the tree knows. A name with a real twin (`Makefile` next to
`makefile`), and a name that is not plain, cost a question at each of their looks.

**A hint is for every name that it can mean** (`Tree::others_meant`). The walk of step 4
follows the child with the very name of the hint, and also

- each registered child that can be the same name. On every system, when the two differ only
  in the case of ASCII letters: where the disk tells the two apart, the look finds what is at
  its own name, and an engine that takes a loose disk for a strict one (a folder that Linux
  shares with macOS) still follows its watched paths;
- in a complete directory, each child that differs only in the case of ASCII letters. In a
  loose directory an entry has one spelling, and the OS can name it in another one when it
  is gone. In a strict one this is one look at a file that did not change.

The walk goes on below each of them when the hint has more names, and the marks at the end
are deep. If no child can be the name, the hint costs no look at the disk.

Two names can be one name (`can_be_one_name`) when they are equal with the case of ASCII
letters left out, or when none of the two is plain. A **plain** name (`is_plain`) has only
ASCII characters and Han ideographs. An ideograph has no case and no second form, so a plain
name has another spelling only in its ASCII letters. How a disk folds any other letter (an
accent, Greek, kana, Hangul) is not known here, so two names that are not plain can always
be one name.

A plain name is found through the table: its hash leaves out the case of ASCII letters
(`child_hash`), so the children that differ only in that are filed under one hash. Such a
hint costs one lookup more, however many children the directory has: 18 ns next to 20,000
registered ones, where comparing with each child took 53 µs. The rest is for loose
directories only. The registered names that are not plain are on a list for each directory
(`Tree::not_plain`), and a hint with such a name marks all of them: one look for each. The
other children of a complete directory are on no list, and there can be many. So when a hint
names a name that is not plain, and the tree does
not have it as an entry (`Node::is_entry`), the flush compares the names of the directory
with one listing (`Depth::Names`): a node that the listing names is not looked at, and one
that it does not name is. An edit of a file that the tree knows costs no listing.

**A look asks for the spelling when it is not sure** (`Engine::spelling_is_unsure`). In a
partial directory a look asks nothing: a registered name is whatever `lstat` finds. A name
with one spelling only is never asked about: one with no ASCII letter that is plain
(`Tree::has_spellings`). In a complete directory that is loose, a look that found an entry
asks the disk for the name of that entry when

- the node was no entry of the directory at the last look (`Node::is_entry`): the tree had
  nothing there, the node is held, or the directory had the entry under another name. Such a
  name comes from a hint or from a caller, and both can spell it as no entry is spelled. So
  is a name that the running `watch` registered (`Added::Registered`), also when the tree knew
  an entry at it: the caller spelled it, and the entry can have another spelling by now;
- another node of the directory can be the same name (`Tree::has_other_spelling`): the disk
  has only one of the two. A name that is not plain counts as such a name at every look;
- the look is part of a scan that listed the whole directory, and the listing did not name it.

A look is sure, and asks nothing, for a name that the listing of the running scan gave, and
for an entry that the tree knows and that has no other spelling in the tree: the edit of a
file that the tree knows costs what it costs on any disk. A link is never asked: the disk
cannot say how it spells a link (the real path of a link is the path of its target), so the
listing is believed for it.

When the disk spells the name in another way, the directory has no entry of this name.

- A name that is not registered is gone (`Found::OtherSpelling`): at once and with no hold,
  because a second look would find the entry again.
- A registered name is still the path of its caller, but the directory above does not cover
  it: the node says so (`Node::is_unlisted`, read by `Engine::is_inside`). A watched path
  reports what is at it as before. A name that only leads to a watched path has nothing of
  its own: a file that the directory reported there is gone at once, and a directory there
  becomes partial and keeps only what the watched paths below it cover (`uncover`). The same
  when a partial directory is found under another name for the first time: its entries were
  covered through this name, and are not any more.

In both cases the disk gave the name that the entry has. That name is looked at in the same
flush, with what the hint said of the entry (`written`), whether the tree has a node for it
or not: the hint was for the entry, under whatever spelling. Once only: a disk whose answers
do not agree with each other must not keep the flush busy, so a second step is marked for the
next flush.

The flag is an answer of the disk that the tree keeps, so it can be old. It is asked again
at every look of a node that is no sure entry, and the tests hold it to the disk when the
tree has caught up (`Engine::check_spellings`). `unwatch` reads it: a path that the directory
above does not list is forgotten in silence, like a path with no watched directory above it.
If the flag was old, one `Create` follows when the look finds the entry.

What it costs: a new file in a watched directory is one `lstat` and one question for its name,
however many entries the directory has. (Before, each flush with a new name listed the
directory: with 20,000 files in it, 100 new files cost 1,617 ms of CPU time, and notify 18 ms.)
A file that goes through another spelling of its name is one lookup in the table and one
`lstat`. (Before, it was missed: `rm App.js` for the file `app.js` gave no `Delete`.) A new
name that is not plain costs one listing of the names of its directory in each flush.

What this does not cover is in section 9.

### 5.3 Flush

```
if notifications were lost and the pacing allows it: queue a deep scan of the root   # 5.13
check_moved                                          # 5.10
for each marked node, from the top down:
    if a node above it is held: drop the mark
    if only a loss report asked for the mark, and the pacing does not allow it: keep the mark
    else: check(node, mark)
while a scan is queued:                              # one round
    backend.commit()                                 # on an error: 5.10
    for each node that was queued when the round started, if its directory is on disk:
        if a scan of this round queued it again: leave it for the next round
        else: scan(node, depth)
finish: read each touched node for its event (5.9), hand the batch over
```

`check` and `scan` never go into a directory. They ask the backend for its watch and queue a
scan (5.5), which runs in the next round, after the commit. So the engine never reads a
directory before the watch on it is live, with one code path for every backend. When the disk
does not answer for a node, its mark stays for the next flush.

### 5.4 `check(node, mark)`

The parent of `node` is a directory on disk. `path` is the path of `node`.

**Read the disk** (`read_entry`).

- A name that is not registered, in a partial directory, is missing, without a call to the disk.
- If a listing gave the type of the entry and the name is not registered, the filter is asked
  first: an ignored entry is missing and costs no `lstat`.
- `lstat(path)`, unless the listing of the running scan told all of it (see below). For a
  registered name that is a symbolic link: `stat(path)` too, which follows it (5.14). A link
  that is not registered is a file. Errors: 5.12.

**A listing that tells all** (`fs::Entry::File`). On APFS one call gives the entries of a
directory with what `lstat` tells of each (`getattrlistbulk`), and a scan takes every regular
file from it: no `lstat` for a file, at the first scan and at each later one (a deep scan
after a loss, a poll). Only what a look at a file reads is taken: its modification time, its
size and its inode, which is its fingerprint (5.8). So a scan holds 40 bytes for an entry of
the directory that it is in, and not the 80 bytes of all that `lstat` tells. A registered
name, and a name whose spelling is not sure, is looked at with `lstat` as before. Measured on a tree of 5,000 directories with 50,000 files, one
directory watched: the watcher is ready in 122 ms, and in 153 ms with an `lstat` for each
file. What is left is one `open` for each directory (8 µs here, a third of the time), the
listing itself, and one `lstat` for each directory.

What the listing told is as old as the listing, and the look at a file can come later. A
write after the listing is told by the OS watch, which was live before the scan (5.3), as a
write after an `lstat` is. A file that went since then is stored and then found gone by the
look that its record asks for.

Only what was measured to be the same as `lstat` is taken:

- APFS: the same for 160,000 entries of a system in use, and for a file that is open and
  written, cut, made longer, linked twice, compressed, renamed. Other disks are listed as
  before: on HFS+ the listing keeps the old times of a file that was written through another
  hard link, on exFAT an empty file gets another inode number at each call, FAT32 gives no
  change time.
- A regular file only. Of a directory where a disk is mounted, the listing tells what is
  under the mount, and a link is followed by the engine itself. A file whose content is in
  a cloud only (the flag `dataless`) is left to `lstat` too: none with a size was there to
  measure.
- The disk is asked by path first (`statfs`), and an entry counts only if it is on that very
  device: the directory can be another one when it is opened.

**What counts.** "Inside" means: the node is a watched path, or its parent is complete.

- A **file** counts if it is inside and the filter does not ignore `(path, File)`.
- A **directory** counts if it is inside and the filter does not ignore `(path, Dir)`: then its
  node is complete. It also counts if a watched path lies below it: `watch` asked about such a
  directory and found it not ignored (5.11), and its node is partial unless the first rule
  holds too.
- What does not count is missing.

`check` makes no event. It changes the node and puts it on the list `touched`; the end of the
flush reads the list (5.9). "Tell" below means that.

| Disk | Tree has | Action |
|---|---|---|
| no answer | anything | keep the mark for the next flush |
| missing | file, directory | `missing(node)`: hold the node; at its second look, it is gone (5.9) |
| missing | nothing | nothing |
| file | nothing | store it. Tell `Create`, or nothing if it is baseline (5.7) |
| file | file | `Update` if the fingerprint differs, or the mark is `written`, or the rule for young files says so (5.8). Store the new fingerprint. A hold ends |
| file | directory | `vanish(node)`, then as "file, nothing" |
| directory | file | forget the file: `Delete`. `appear(node)` |
| directory | nothing | `appear(node)` |
| directory | directory | store the identity; make it complete if the rule says so (5.6); a hold ends; `still_there(node)` |

A change of kind is never an `Update`, and it does not wait: the `Delete` of the file, or of
every file below the directory, is in the same batch as what came in its place.

After that, the node is freed if nothing needs it any more. And if the node is a registered
name with no directory at it now, and its directory was asked for names only, the directory is
asked for `Contents` (5.10).

The identity of a directory is never a reason to skip the check when a hint names the
directory: a directory that was moved away and back keeps its inode.

### 5.5 `appear`, `still_there`, `scan`, `vanish`

`appear(node)`: the kind of the node becomes "directory", with its identity and `complete`; ask
the backend for its watch (`arm`, 5.10); queue a shallow scan. With `two_step_links` (Windows,
not under the poll) the node is also marked for one more look at the next flush, as is a
directory that is another one than before: a junction or a directory link is a plain directory
until its second step, which gives no record, so a look between the steps sees a directory.
The one more look is spent by whatever flush comes next, a poll or the hints of other paths:
when that flush falls between the steps too, nothing looks again (measured with the steps 300
ms apart, which no tool does; the story keeps them 50 ms apart, clear of the poll).

`still_there(node, deep)`: `arm` the node, and with `deep` also queue a deep scan. `deep` is
true for a `deep` mark, when the directory was held, when it is another directory than before,
and when a link on the way changed (5.14).
Nothing below the node is thrown away: the deep scan compares each file with the fingerprint
that the tree has, so only what differs is reported.

`scan(node, depth)`: `check` every child of the node. A **complete** node also lists the
directory: each entry that the tree has no node for yet gets one, and is checked with the type
from the listing. A listing follows a link at the path, so it counts only when the same
directory is still at the path when it is done (one look): a link that took the place of the
directory since the look would be read as the directory, and the files behind it reported.
Else the node is marked deep, and the next flush looks again. A **partial** node lists
nothing: its children are its registered names. In
a deep scan each check gets a `deep` mark, so every directory below is watched again and
scanned deep too. Nothing is scanned below a held node. If the listing fails because the
directory is gone, its parent checks its name; when that look finds the same directory again,
the node gets a deep mark for the next flush. If the disk does not answer, the node gets a
deep mark for the next flush. An entry that the listing named and that is not there when it is
looked at makes the directory list again at the next flush (`relist`): it can have been renamed
away and back while it was read, which the Windows backend does not report.

`vanish(node)`: the directory is gone for good, or a file took its place. The kind of the node
becomes "nothing"; `backend.unwatch` if the backend was asked for this node. For each child:
forget it if it is a file (the end of the flush tells its `Delete`), `vanish` if it is a
directory; its mark is dropped. The walk keeps a list of the directories still to do, not a
recursion: a tree deeper than the stack goes like any other.

### 5.6 A partial node becomes complete

When `watch` makes a directory `R` a watched path and its node is there as a partial node,
`register` marks the node of `R`. The check finds the same directory, sees that the rule of 5.1
makes it complete now, sets `complete`, asks the backend with the higher want, and queues a scan.
That scan checks the child directories in the same way, so this goes all the way down.

### 5.7 `watch` and the baseline, `unwatch`

**`watch`** (`engine/watched_paths.rs`). A path below a directory that the filter ignores, or
one that it ignores as a file and as a directory, is left out (5.11). For each other new
path: walk down from the root; make the missing nodes; make the last one a watched path, which makes every node above it registered; put a
deep mark on the deepest node on the way that has only directories on disk above it, each
listed by the directory above it (unless a scan that checks every name of its directory is
queued already: a scan of the names alone skips what the listing names). The way ends at a
name that the disk spelled in another way at its last look: the tree can be behind the disk
when `watch` is called, a notification still on its way, and the baseline of what is below
is decided by that answer. Then run one flush right away, without the quiet time, and return
when it is done.

The whole batch is registered before the flush, and each node is checked once in a flush. So
a batch costs one `watch` call for a directory and one `lstat` for a name (the test
`the_cost_of_a_batch`), plus one look for the kind of a watched path in each directory
that holds some (5.10).

**Baseline.** What `watch` finds is not reported. But there is no silence for a file that had
to be reported before this `watch`: an older watched path or an older complete node covered it
already. A complete node whose listing failed (no permission, or another program held the
directory) covered nothing, so a path watched below it later reports what it finds as new. So silence is only for

- a file at a path that this `watch` made a watched path, in a directory that was not complete
  before,
- every entry of a node that became complete during this `watch`.

As built: while a `watch` runs, a node says what the call made of it (`Node::added`), and a
directory says that it became complete in this call (`Node::baseline`). `check` asks at the
moment of the look whether the node had to be reported before the call (`reported_before`): it
was a watched path before, or the directory above was complete before. Where the directory
above is still partial in the tree, the rule of 5.1 is asked for the watched paths of before
the call: the look of an older `watch` can still wait for the disk to answer. If not, what the
look finds is the baseline: a file becomes `told` without an event. The end of the call clears
both fields, so a look that had to be tried again after the call reports what it finds.

**`unwatch`** (the same file) is the inverse of registering, and it reports nothing:

- What only this path covered is forgotten: the file at it, or every file below it down to
  the next watched path. The handler is not told, also when a `Delete` of the file was
  waiting. "Covered" is decided by the rule of 5.1, by the paths alone, with no look at the
  disk. What another watched path still covers stays as it is.
- The nodes that nothing needs any more are freed, and the backend forgets each directory
  among them. A directory that stays, because a watched path lies below it, is partial.
- A directory that stays may want less than it was asked for (`narrow`): the nearest one
  above what went, asked once at the end of the call, however many paths went out of it. A
  watch that was asked for `Contents` is given back when the tree says that names are enough
  (every registered name in it is a directory on disk), and the backend is asked again, for
  names.
  After a `WatchedTree` watch, everything that stays below is scanned deep, so each node that
  relied on the watch asks for one of its own. After a watch for the one directory (inotify),
  the scan that every ask queues looks at its registered names. Before, such a watch was left
  as it was, and each write in a directory that held no watched file any more still woke the
  watcher: 6 ms of CPU for 20,000 writes in half a second, and 0.1 ms now. No event was wrong
  either way. While the tree has nothing at a
  registered name, the wide watch stays. When `unwatch` of a tree makes many levels give such
  a watch back, only the top one queues the deep scan: that scan comes to the others, and
  everything below is scanned once.
- A link is followed only while it is registered. Inside a watched directory it is a file
  entry again, so it gets one check. That check can make events: it is the one case in which
  an `unwatch` call reports something.

Then one flush runs, as for `watch`. A path that is watched again later gets a new baseline.

### 5.8 Fingerprint, identity, and young files

- File fingerprint: modification time (full resolution), size, file id. The file id is the inode
  on Unix and the file index on Windows.
- Directory identity: device, inode and birth time on Unix (birth time zero where there is
  none); volume serial number, file index and creation time on Windows. The creation time
  alone is no identity there: a new entry gets the one of the entry that was removed under
  its name a moment ago.
- No ctime: chmod, a rename, a new hard link and an xattr write change it without a content
  change.

Two writes of the same size can share one time stamp. Equal fingerprints then hide a change.
Two guards:

1. A backend that knows of a write sends `Written`, and the engine reports `Update` without
   asking the fingerprint. Only a backend whose notifications are one for each event does this
   (inotify, Windows). FSEvents never sends `Written`.
2. **Young files.** A file is young when its modification time is close to the flush (or in the
   future). When a young regular file is stored, the engine also stores a hash of its content.
   At the next check with an equal fingerprint the hash decides. When the file is no longer
   young, the hash is compared one last time and dropped. A link entry is never opened: its
   hash is the hash of the text of its target. A watched path that is a link is followed for
   its fingerprint, and so for its content: the file is read by its real path. Any other read
   opens the entry so that a link that took the place of the file since the look is not
   followed, and a FIFO or a device is not waited for (`O_NOFOLLOW` and `O_NONBLOCK`; on Windows a reparse point that stands for
   another name is refused). A file above 16 MiB is not hashed, and the read stops at the
   limit when the file grew since the look: such a file, and a file that cannot be read, has
   nothing to compare:
   - where an OS watch covers it, an equal fingerprint counts as a change at every look while
     it is young, and at the first look that finds it old;
   - where only the poll looks at it, an equal fingerprint says nothing while it is young, and
     the first look that finds it old reports one `Update` for all of them. A later look is
     sure to come there, and a write that leaves the time stamp as it is can only come while
     the file is young.

   On Windows the read of a file that was just written waits for the virus scanner (2 ms,
   and 14 ms for one in ten, measured). (A number that NTFS keeps for each file, the place of
   its last record in the change journal, stood in for the hash there for a while: 0.04 ms
   instead of the read. It does not move for a second write through a handle that stays
   open, which was a hole of its own, and it was one more thing to keep. Taken out.)

| The modification time | Young for |
|---|---|
| has a part of a second in it, and an OS watch covers the file | 20 ms |
| is a whole second (the file system counts in seconds) | 2 s |
| any, and only the poll looks at the file | 2 s |

The clock is the clock of the watcher (`Fs::now`), not of the disk: a disk whose clock is
behind it (a network disk) has no young files, and a second write of the same size inside one
step of its time stamps is found only where the OS reports writes (README, "Holes"). A disk
whose clock is ahead is the next case.

**A time far ahead of the clock** (more than 2 s: the clock of another machine, a time that a
tool set) never gets old by the clock. Such a file was young for ever: with polling, each
round read every such file again (measured: 5 files of 4 MiB, 420 MiB read in 20 rounds). A
write after a look gives the file another time, whatever the clock of its disk says. So the
file is young for 2 s, on the clock of the engine, after the first look that found the
fingerprint it has now (`Tree::ahead_since`), and old after that.

"Only the poll looks at it": the directory of the file is polled, or the node is a symbolic link
(5.14). So with polling, a young file above 16 MiB gets one `Update` when it is 2 s old, also
when nothing changed (section 9).

When the file is gone between the `lstat` and the read of the hash, the check says nothing and
the node is checked again at the next flush.

### 5.9 Events, and the delete delay

**Events are read off the tree.** A node has two bits for the handler: `told` and `changed` (the
file is another one than the handler was told about). A check only changes the node and puts it
on the list `touched`. At the end of the flush, `finish` reads each touched node once
(`engine/events.rs`):

| `told` | A file is at the node now | Event |
|---|---|---|
| yes | no | `Delete` |
| no | yes | `Create`; none if the file is the baseline of the running `watch` (5.7) |
| yes | yes, and `changed` | `Update` |
| otherwise | | none |

Then `told` is what is there, and `changed` is off. So whatever happened inside one flush needs
no rule: a file that came and went gives nothing, a file that went and came gives one `Update`.

**The delete delay.** Tools replace a file by removing it and writing it again. So a `Delete`
is not told at once.

- The first look that finds nothing where the tree has a file or a directory only **holds** the
  node, with the time of its second look: one delete delay after the hint that asked for the
  first look, so the quiet time before the look does not add to the delay; one delete delay
  after the look where no hint asked for it (a poll, a scan), the time of the look itself and
  not of the start of the flush, which can be long before it. A first look that comes later
  than that, after a quiet time longer than the delay or under a flood, gets its second look
  at once, in the next flush. So a path that is back within the longer of the two is one
  `Update`. Nothing else changes: the fingerprint stays, and under a directory every node
  stays as it is. A directory that vanishes with 50,000 files is one held node. The backend
  forgets the watch of a held directory at once.
- **The second look** comes at its time, also when no hint came and also with polling. Still
  nothing: a file is forgotten (`Delete`), a directory is `vanish`ed (`Delete` for each told
  file below it, in one batch). If the disk does not answer, the hold stays until it does: an
  error that says nothing is never a `Delete`.
- **It is back.** A file is compared with the fingerprint that was kept: `Update` if it
  differs, nothing if it is the same file. A directory is watched again and scanned deep, and
  each file below it is compared in the same way.
- **A change of kind ends the hold at once** (5.4).
- A file that is not told yet is not held: when it is gone, it is forgotten.
- If a node above a held node is found missing too, the upper one decides for both.
- With a delay of zero (`with_delete_delay`) nothing is held: a `Delete` comes at the first look.
- **The default** is 50 ms on Linux: vim renames
  the file away and writes a new one, and on a busy machine the path was missing for long
  enough that no delay made 4 of 20 saves a `Delete` and a `Create`, and 30 ms 2 of 60; with
  50 ms, none of 60 was. On Windows it is 10 ms: the tools that remove a file and write it
  again (a delete and a new file, a delete and a rename, a rename away and a new file, vim,
  `git checkout` of 100 files) leave the path missing for microseconds, and with 10 ms none of
  505 saves a round was a `Delete`, idle or under a load of 16 busy threads on 20 cores and a
  writer of files next to the tree (2026-10-20, with each quiet time of 5.13). On macOS FSEvents tells about 10 ms after the change, when a save is
  done (5.13): the delay is 10 ms there, a guard that costs little, and the only wait, as
  there is no quiet time (with a quiet time of 10 ms or 20 ms and no delay, none of VS Code,
  vim, `sed -i`, `git checkout` and a JetBrains-like save was wrong, idle or busy). When the watcher polls everything there is none, as there is no quiet time
  (`Config::into_options`), and as in chokidar (`atomic` is off under `usePolling`), notify's
  `PollWatcher`, watchdog's `PollingObserver` and watchpack: a round that falls into a save
  that leaves the path empty reports a `Delete`, and the next round a `Create` (vim on Linux,
  polled every 100 ms: 4 of 100 saves; with a delay of 10 ms or more, none). The engine still
  has the second look under polling (`Options::delete_delay`), for when users ask for it.

### 5.10 Want, watch, poll

**The want rule.** What a directory node asks from the backend:

- `Contents` if the node is complete, or if one of its registered names has a file or nothing
  at it. A file there is written, and where nothing is, a file can come, or a directory with
  everything below it at once.
- `Names` otherwise: every registered name in it is a directory. Such a directory asks for a
  watch of its own. So `watch("/home/me/project")` puts a stream on the project, not on the
  home directory with everything else in it.

So when directories above a watched path are missing, the lowest directory that exists is
asked for `Contents`. On FSEvents and Windows that watch reports the whole tree below it, and
`mkdir -p` with a file at once is found without a poll. The watch stays there afterwards.

As built (`Tree::want`, `Engine::want`, the end of `check`), so that no file is looked at
before its watch is live:

- The tree answers when it knows: complete, or a registered name with a file or a held node
  → `Contents`; every registered name a directory → `Names`.
- Where the tree has nothing at a **watched path**, the disk is asked before the backend is,
  **only for the kind** of what is there. Nothing is stored. In a batch of watched files this
  is one more `lstat` for each directory that holds some.
- Where the tree has nothing at a **name on the way**, the disk is not asked: a directory is
  expected there.
- When a check finds no directory at a registered name whose directory was asked for names
  only, the directory is asked for `Contents`. That queues a scan of it, so the name is looked
  at once more when the wider watch is live.
- For one key the want only goes up. It goes down in one way: `unwatch` (`narrow`, 5.7).

`arm(node)` asks the backend: when a directory appears, when a check finds it still there, and
when the node wants more. It calls `backend.watch` when the node wants more than was asked, or
when the caller says the watch may be gone. Each call queues a scan of the node for after the
next commit. The watch state of the node:

| State | Meaning |
|---|---|
| `Watched` | the backend watches this directory |
| `WatchedTree` | the backend watches this directory and every real directory below it |
| `FromAbove` | a node above is `WatchedTree`; the backend is not asked for this one |
| `Polled` | no OS watch covers it (the answer `NotWatched`, a held node, or the backend is dead) |
| `Failed` | the backend could not watch it; the error is reported once |
| `None` | the directory was just found and the backend was not asked yet; also: no directory is on disk |

- A node that loses `WatchedTree` gets a deep scan: it asks for the watch of every node below.
- A node whose key `commit` names as failed becomes `Failed`, with the reason the backend
  gave; no other node does.
- If the backend says that it cannot go on, the error is reported once, every node becomes
  `Polled`, and everything is scanned.

**The poll.** The tree keeps a list of the directories that the timer looks at
(`Tree::polled`), so a poll does not walk every node. A directory is put on it when the
backend answers for it and when its scan ends, and a stale key stays until a tidy: each poll
tidies the list, and so does a push that finds it longer than twice the arena, and the end of
the poll when nothing is watched any more. So a `watch` and `unwatch` of ever new paths, each
shorter than the poll interval, cannot grow it without bound. On the poll timer, for each node
on the list whose directory is on disk:

- `Failed`: ask the backend again (it may have room now),
- `Polled`, or a scan of it had an error: queue a shallow scan,
- else: put a plain mark on each registered child that is a symbolic link (5.14).

The marks and scans go through the normal flush, and the next poll is paced (5.13). With a
native backend the timer is one second and the polled nodes are few: the root, the directories
that the backend answered `NotWatched` for, and what failed. The timer runs only while the list
holds more than the root, a child of the root is not on disk (a drive that is not there yet), a
registered name is a link (5.14), or a flush left work behind because the disk stopped
answering, which the next poll takes up: with inotify, which watches the directories above too,
and with FSEvents, whose guard does (7.2), the thread sleeps until a notification comes. With
the poll backend every answer is `NotWatched`, so every node is polled, and the timer is the
poll interval. **Polling is this
timer with the poll backend.**

**Asked again.** `NotWatched` is not for ever. A commit can put an OS watch over a polled
directory (Windows: a handle that comes to stand at or above a directory that asked for names;
not FSEvents, whose guard answers such a directory, and whose streams never stand at or above
a directory that got `NotWatched`). Then `commit` returns the keys of these directories, and the engine
calls `watch` again for each of them (`ask_again`). One that is reported now gets a shallow
scan after the next commit (watch, commit, then scan), and no poll looks at it any more.

**A watch below a watch.** A directory can have a watch of its own and lie below a watch for
a tree: its path was watched first, and a path above it later. It keeps its own watch until the
directory goes or the tree above it is taken back: a record below it then makes one hint for
each of the two keys, and the tree merges them into one mark. (Until 2026-10-19 the lower watch
was given back after a deep scan, so that the OS watches did not depend on which path was
watched first: 40 lines and two rules of their own, for one cover and one doubled hint a
directory on FSEvents and Windows.)

**A directory above a watch by path.** A watch by path (FSEvents) stands at the text of a
path. When a directory above it is renamed, also only to another spelling of its name, the
watch hears nothing more; and when the directory is renamed away and back, the watch heard
nothing meanwhile. The poll of the directory above finds the same directory at the watched
path both times. The engine does not try to find such a rename: the backend is told by the
kernel and reports each of its watches below that directory as lost (7.2), and the engine
watches the directory again and scans it deep, as after every loss report.

Time stamps do not tell such a rename, and the engine keeps none for directories. Measured: a
rename moves the change time of the directory on APFS only (not on HFS+, exFAT or FAT32), and
the modification time of the directory above. A file or a directory that is moved from the
directory above into the renamed one, with one `rename` call, moves the same two stamps. A
rule that took this for a rename scanned a watched project of 50,000 files again each time a
directory was moved into it from next to it (250 ms of CPU time).

**`check_moved`.** A mark can come through a watch that moved away with its directory. If the
parent of that directory is only polled, nothing says so in time, and the marks would be
checked against the wrong directory. So at the start of a flush, for each marked node, the
nearest directory at or above it whose parent is only polled (or that is reached through a
link) gets a plain mark, unless it has a mark already. Marks are checked from the top down, so
the parent checks it first.

### 5.11 The ignore filter

`ignored(path, kind)` with `kind` = `File` or `Dir`. The engine asks (`engine/filter.rs`); a
backend never sees the filter, so the result is the same on every backend.

**The contract of a filter.** It gives the same answer for the same path and kind. It should
ignore everything below a directory that it ignores (a **closed** filter). A filter that only
looks at the last name is allowed, and gives the same answers as a closed one for everything
below an ignored directory: `/p/node_modules/pkg/x.js` is never reported, whether `/p` or the
file itself is watched, since `node_modules` is on the way. The stories in
`tests/sim/filter.rs` pin it; the fuzzer uses a closed filter and one for a name.

**The kind is always known.** It comes from `lstat` or from a listing. An entry that is gone
is never asked about: what the tree stored decides. The path has the registered spelling.

**The answer is kept with the node**, one for each kind (`Engine::ignores`). So an entry is
asked once when it is found and again when its kind changes, not at every check. An ignored
entry inside a watched directory keeps no node, so each listing of its directory asks about it
again.

Three things are asked:

1. **An entry inside a watched directory** (`read_entry`). An ignored entry is missing. An
   ignored directory is never listed and never watched, and nothing below it is asked. Hints
   from inside it end at its parent and cost one `lstat` in a flush. A directory that goes is
   decided at the directory: one `Delete` for each told file, and no question. A directory that
   comes is asked once, and then each entry in it once.
2. **The directories above a path given to `watch`** (`is_ignored_above`), as directories,
   from the top down and whether they exist or not, so that nothing is asked below an ignored
   one. A node on the way keeps the answer. If one is ignored, the path can never be reported:
   it gets no node and costs no watch, no look and no poll, and `unwatch` of it does nothing.
   The spelling of a watched path never changes, so this is asked once. A directory on the way
   to a watched path is therefore never ignored, and `read_entry` does not ask about it.
3. **The path itself** (`register`), for both kinds, whatever is on disk. If both are ignored,
   the path is left out in the same way. Every other path becomes a watched path. When it
   exists, it is asked with its kind like every entry. While the answer is "ignored" it counts
   as missing: nothing is listed or watched for it, and it is asked again when its kind
   changes.

**No event comes from the filter alone.** A watched path that counts as missing was never
reported, so nothing is deleted when it goes. A file that takes the place of an ignored
directory at a watched path came to the disk: a `Create`. The files of a watched directory
that an ignored file replaces are gone from the disk: a `Delete` for each.

(Before, the filter asked about the "watch target" of a path too: the directory that holds a
watched file, or the lowest directory that exists above a missing path. A path whose target
the filter ignored was dropped with the code of `unwatch`, with a `Delete` for each of its
files that still existed, and nothing woke it up. That gave events that the disk did not
have, and answers that depended on which directories existed. Rule 2 replaces it.)

### 5.12 Errors from the disk

| Error | Meaning |
|---|---|
| not found, not a directory | missing |
| an invalid name (a path longer than the OS allows) | missing; reported once as `Io`; the node is polled: nothing can ever be at the path |
| an entry that is being deleted (Windows: the name goes when the last handle on it is closed) | missing; the node is looked at again at each flush until the name is gone or the file is there, since the holder can take the deletion back and the OS says nothing then |
| permission denied, link loop | missing for events; reported once; the node is polled until a scan of it has no error |
| a server that cannot be reached (Windows `ERROR_BAD_NETPATH`, which std calls not found) | no answer |
| another program holds the entry with no sharing (Windows: a sharing or a lock violation, `ResourceBusy`) | no answer: the mark stays for the next flush and the node is polled until a scan of it has no error; not reported, since nothing is wrong with the disk and the holder lets go in a moment as a rule |
| anything else | no answer: the tree stays as it is, no `Delete` is told, the mark stays for the next flush; reported once; the node is polled |

A failed `watch` is reported in the same way (`WatchLimit`, `PermissionDenied` or `Io`), but not
when the directory is gone: the scan that follows finds it missing. `WatchLimit` is reported once
for the life of the engine, not once for each directory: a tree past the limit finds no room for one
new directory after the other, and each was one more error (`the_watch_limit_is_reported_once`).

**A disk that other machines change too** (NFS, SMB, a folder of the host in a VM or a container):
an OS watch reports only what changes on this machine, and no error of the OS says so. Measured: a
folder of the Mac in a VM of Linux (virtiofs, FUSE) gave no inotify event for a write made on the
Mac; NFS on macOS gave no FSEvents record within 15 s for a write made in the VM. So the first OS
watch on each disk (by its device) asks the type of the file system (`Fs::remote_disk`, one
`statfs`: 0.2 to 3 µs): on macOS a disk that is not `MNT_LOCAL`, on Linux NFS, SMB, CIFS, 9P and
FUSE, by the number of the type and not its name (the same folder was `fuseblk`, and later
`virtiofs`). The handler is told once for each such disk (`RemoteDisk`), and nothing more is polled:
a big tree on a slow disk read every second can cost more than the changes are worth, which only the
user can weigh (`with_polling`). A local disk through FUSE is told too (an error too many), and a
disk that is polled anyway is not told (`a_disk_that_other_machines_change_is_told_once`). Windows
asks nothing: a server of SMB reports the changes of other machines to the handle.

### 5.13 Time and the queue

The engine has no thread and reads no clock. Its entry points get the time from the caller
(`engine/mod.rs`): `watch`, `unwatch`, `signal` (one signal of the backend), `overflow` (the
queue dropped signals), `deadline` (the next time there is work: a flush, the second look of a
held node, a poll, or a loss report that waits for the end of a rest) and `tick` (do what is
due). It reads the disk through the trait `Fs`: `StdFs` is the real disk, `SimFs` the one of
the tests.

**The watcher thread** (`watcher.rs`) is one loop around a queue. A backend thread never
blocks: at most 16 MiB of signals wait (`Backend::raw_bytes` says how big a raw notification
is). A signal that would make it more is dropped and a flag is set, which the engine takes as
lost notifications (`overflow`). The drop that sets the flag also sends one message that only
wakes the thread, so the flag is read at once. Commands (`watch`, `unwatch`) are never dropped
and not counted. The loop takes at most 512 messages, then does what is due, so a flood cannot
hold back a flush. A command from inside the handler goes a way of its own: the thread takes
it when the handler has returned, before the next message of the queue, so what the handler
asked for is in place before anything that came in meanwhile. On Windows the thread does not
wait on the channel for a deadline: a timed wait of a channel ends at a tick of the clock, which
comes every 15.6 ms unless a program asked for more (a wait of 20 ms took 31 ms, and one of
1 ms 15.6 ms, measured), and that was the whole of the 11 ms between the quiet time and the
reported edit. The thread waits on an event that every sender sets after its message and on a
waitable timer of high resolution, which comes within half a millisecond (`watcher::inbox`);
on the other systems the wait of the channel keeps the time.

**The debounce.** The disk is checked when no hint came for the quiet time: every hint that
leaves work moves the flush to one quiet time after it. So a save in several steps is one
batch. Hints that keep coming do not hold a batch back for ever: counted from the first hint
after the last flush, the flush comes after four quiet times at the latest, and after 500 ms
at most unless the quiet time is longer (`Engine::schedule`), as lodash's `maxWait` counts
from the first call. Measured on Linux with a file in the watched tree written every 5 ms: an
edit next to it waited 364 ms (p50) when the longest wait was 500 ms, and 25 to 28 ms with
four quiet times; on macOS FSEvents leaves gaps of about 10 ms, and the edit waited 22 ms
either way. A burst of 2,000 edits went out in 3 batches on macOS instead of 1, and all were
there as soon (2026-10-15). @parcel/watcher counts its 500 ms from the last batch instead: the
first change after half a second of quiet goes out at once, alone, and a check of the disk at
that moment can catch a save half done (5.9). The default quiet time is 20 ms on Linux: long enough for the steps of one save (a temporary file, a rename) to be one batch,
also on a busy machine, and short enough not to be felt; watchman and webpack wait the same.
Measured on Linux with the saves of 5.9 on a busy machine: with 5 ms, 4 of 60 saves and 2
files of 1,000 that `git checkout` wrote were reported twice, the first time half written;
with 10 ms, none. On Windows a save is two records, one at the open that truncates the file
and one at the close, and nothing for the writes between (measured): the quiet time can only
hold those two together, and a flush that falls between them reports the file twice, as the
record of the close is a `Written` hint (5.8). Measured with the saves of 5.9, 15 of each way,
and `git checkout` of 100 files 4 times, idle and under a load of 16 busy threads on 20
cores: with 20 ms nothing was wrong; with 10 ms only `git checkout` was, 1 file of 1,600 idle
and 2 or 3 of 400 under the load; with 5 ms, 3 to 5 of 400, one of them half written. The
default there is 10 ms, for the idle machine that editing happens on (2026-10-20). On macOS FSEvents tells about 10 ms after the change, by when a save is
done: no value was wrong there, idle, busy, and under a flood of 30,000 writes a second,
when it tells at once (a 1 MB save was told 1 ms after it, still one `Update`). So the
default there is zero, as in watchdog, notify and Node, and an edit is told twice as soon
(12 ms instead of 24). The price: a temporary file that lives longer than FSEvents waits is
reported, a `Create` and a `Delete` for a file that no one wants to know (the swap file of
vim: up to one pair a save; with 10 ms, none when idle, and fewer under a flood), and a
flood comes in more, smaller batches. The marks
of hints wait for that time, and nothing else takes them: the quiet time can be longer than
the poll interval, and a poll or a second look that runs before it checks its own work and
leaves them. Two exceptions, each because a look is made anyway: a
`watch` or `unwatch` call ends the quiet time, since its baseline is decided by a tree that
is in line with the hints; a hint for a directory above a held node is checked with the
second look of that node, since the directory decides for it. And a check that a scan makes
takes the mark that waits on its node with it: the look is made now, and what the hint said of
the node (`written`) holds. The delete delay is not touched by any of this: it counts from the
hint, or from the look that found nothing.

**Pacing.** Two kinds of work are asked for by no hint that names a path, and can come again
and again: a poll, and the scans after a loss report. Each is paced by the time it took.

- Loss reports: all scans that the loss reports of one flush asked for are timed together, and
  the next ones wait for the rest after them, which is half a second at most
  (`LONGEST_LOSS_REST`): the rest is against a storm of loss reports, and one long scan (a
  directory of 20,000 entries takes about 200 ms) must not put the next loss report off by
  seconds. A mark that only a loss report asked for stays on its node until then. Every other
  mark is checked as always, and a hint that names a path with a waiting mark takes it out of
  the wait.
- The poll: the next round starts one poll interval after the end of this one, and at least
  one round's time later, so that polling takes half of the time at most; a `watch` call in
  between does not move it. The pause is the one knob of polling, as in notify and watchdog:
  a round looks at every watched file once (20,000 files in 2,000 directories: 22,000 `lstat`,
  50 ms on APFS, measured), and a user with many files raises the interval. (A rest of nine
  times the looks, a tenth of a core at most, was the rule for a while: it made the interval a
  floor that the user could not see, and 20,000 files polled every half second.)

The time is the time of the whole flush that the poll or the scans ran in: the checks that
hints asked for in the same flush count with it, and so do the reads of young files and what
the backend takes to make watches live. A flush counts for a minute at most (`LONGEST_FLUSH`): the clock runs on while the machine
sleeps, and a flush that the sleep fell into would pause for hours.

### 5.14 Symbolic links

- A link **inside a watched directory** that is not registered is a file entry. It is never
  followed and never opened (5.8).
- A **registered name** that is a link is followed. The event paths keep the registered
  spelling.
- For such a link the engine stores a hash of the real path behind it. When that changes (link ↔
  no link, or another real path) and the directory is still the same one, the directory is
  watched again and scanned deep: a watch that goes by path would be dead otherwise. If the
  look at what is behind the link gets no answer, the stored hash stays as it was.
- A watch for a tree does not report what is behind a link. So a node that is reached through a
  link never becomes `FromAbove`: the backend is asked for it.
- Nothing watches a link itself. Every poll checks each registered name that is a link, and
  `check_moved` takes a node behind a link like a node with a polled parent.
- Baseline: when `watch` puts a watched path at or behind a link that was not followed until
  then, what is behind the link is the baseline of that `watch`. After `unwatch` the link is a
  file entry again (5.7).

## 6. Backend interface

`src/backend.rs`. The engine asks a backend to watch directories, and the
backend tells the engine where to check. A backend never says what happened: the engine reads
the disk itself. The items of the file have short docs; this section is the one place for the
rules.

**The trait `Backend`.** Every backend writes `watch`, `unwatch` and `translate`. A backend
type has its own `new(sink: Sink<Self::Raw>)`.

| Item | Meaning |
|---|---|
| `Raw` | A raw OS notification. Cheap to make: the backend thread only copies it into the sink. |
| `watch(key, dir, want)` | Report changes in `dir` as hints that carry `key`. The answer is a `Coverage`. |
| `unwatch(key)` | Forget `key`. No hint should carry it after this; a late one costs the engine one check. |
| `commit()` | Make every watch asked for since the last call live. It returns two lists of keys (`Committed`). The keys whose watch could not be made live, each with the reason: the engine reports it, polls the directory, and asks again later; a key that is not named is live. And the keys that got `NotWatched` and whose directories an OS watch reports now: the engine calls `watch` again for each of them, with the `Want` of before, and does not poll one that is reported (5.10, "Asked again"). Only a backend that puts one OS watch over many directories has work here, as its answer for a directory depends on the OS watches of other directories: FSEvents names the keys that got no stream or no guard, and the Windows backend the keys that a handle reports now. Every other backend leaves the method out: then it does nothing and returns no key. |
| `translate(raw, hint)` | Turn one raw notification into hints. |
| `raw_bytes(raw)` | How many bytes `raw` keeps on the heap (5.13). A backend that leaves it out says 0. |

**The types.**

- `DirKey` names one directory node of the engine. A backend stores it and puts it into every
  hint for that directory, so no path spelling has to match between the two sides.
- `Want` is what the engine asks about a directory (5.10). `Names`: tell when a name in the
  directory comes, goes or is replaced. Enough for a directory that only leads to watched
  paths. `Contents`: tell that, and also writes to the files in the directory. For a watched
  directory, and for a directory that holds a watched path with a file or nothing at it.
- `Coverage` is the answer to `watch`. `Watched`: changes in this directory make hints.
  `WatchedTree`: changes in this directory and in every real directory below it make hints;
  the engine does not call `watch` for the directories below. `NotWatched`: the backend does
  not watch this directory, and the engine polls it. `Failed(error)`: the backend tried and
  could not; the engine reports it, polls, and asks again later.
- `Hint` says where the engine has to check.
  - `Changed { dir, rel }`: the entry at `rel` below `dir` came, went or was replaced. If it is
    a directory, anything below it may have changed too. An empty `rel` is a loss report for
    `dir` itself: the watch is gone, the directory was moved away, or notifications were lost.
    The engine checks the directory, watches it again and scans everything below it. It paces
    these scans (5.13), so a backend may send this often.
  - `Written { dir, rel }`: the content of the file at `rel` was written. For a directory it
    means nothing.
  - `Lost`: notifications were lost, and the backend cannot say where. Check everything.
  - `Mount { path }`: a volume was mounted or unmounted at the real path `path`, and the backend
    knows no key for it (inotify, 7.1). The engine checks the node at that path, if it has one.
- `Sink` is the way from a backend thread into the engine. It never blocks. It hands over a
  `Signal`: `raw` a raw notification, for `translate` on the watcher thread, and `failed` the
  word that the backend cannot go on. From then on the engine polls everything, and it drops
  the backend at once: its kernel watches, streams, handles and threads do not wait for the
  end of the watcher. (An inotify watch counts against a limit that all programs of the user
  share.)
- `watch_limit(message)` makes the error for "the OS has no room for one more watch": an
  `io::Error` with a mark of its own inside, so that no error of the OS is taken for one. (The
  mark was the kind `StorageFull` before, which the OS gives too.) Its message names the limit
  and what to raise. `is_watch_limit` tells whether an error is one, and the engine reports
  such an error as `ErrorKind::WatchLimit`.

**The rules for a backend.** Other files name a rule by its number.

1. **Threads.** Every method runs on the watcher thread. A backend thread only copies raw
   notifications into the `Sink`.
2. **Watch, commit, scan.** The engine reads a directory only after `watch` and then `commit`.
   So a watch is live when `commit` returns, unless `commit` named its key, and `commit` opens
   no gap in a watch that is live.
3. **`watch` again** for a key means: the watch must stand on the directory that is at `dir`
   now. If that needs a new OS watch, the new one is live before the old one goes.
4. **The answer is the truth.** `Coverage` says which changes make hints from now on. After
   `NotWatched` or `Failed` nothing is watched for the key. A `commit` can make an OS watch
   report a directory that got `NotWatched` before: then `commit` returns its key, and the
   engine asks for it again.
5. **Want.** For one key the `Want` only goes up, until `unwatch(key)`. More hints than asked
   for are fine, fewer are not.
6. **Keys.** One OS watch can serve several keys: two spellings of one directory, or a renamed
   directory under its old and its new key. The OS watch goes with its last key.
7. **One hint for each key** that can see the change, with `rel` relative to the `dir` given
   with that key. A backend whose OS reports real paths maps them back.
8. **Where, not what.** `Changed` for a directory makes the engine scan everything below it.
   So "this directory was touched" is dropped, or sent as `Written`.
9. **`Written` only when the OS says so for each event.** An OS that merges its flags, or
   repeats them, never sends it.
10. **A dead watch.** The OS dropped the watch: forget it. The directory was moved: keep the
    watch and its keys. Both times send `Changed` with an empty `rel` for each of its keys.
    A watch that stands at the text of a path is dead as soon as a directory at or above that
    path is renamed or removed, and it does not say so by itself: the backend has to find out,
    at once and also when the directory is back a moment later. And a key that is watched
    again and whose directory has another real path than before gets the same hint: its old
    watch was dead for a while.
11. **Lost notifications.** The OS names the place: `Changed` for each key of it, with an empty
    `rel` or the `rel` of a place below; the watch stands. No place, or unreadable data: `Lost`.
12. **The end.** A backend that cannot go on calls `Sink::failed` once. Dropping the backend
    stops and joins its threads, and the sink is not called after that.

## 7. Backends

### 7.1 inotify (`inotify.rs`)

- One kernel watch for one directory. It does not reach the directories below, so every answer
  is `Watched` and the engine asks for every directory of a tree.
- Events asked for: `Names`: `CREATE | DELETE | MOVED_FROM | MOVED_TO | MOVE_SELF |
  DELETE_SELF`. `Contents`: these and `MODIFY | ATTRIB | CLOSE_WRITE`. Flags: `EXCL_UNLINK |
  ONLYDIR | MASK_ADD`. No `OPEN` or `ACCESS`, so the reads of the engine make no events.
- A kernel watch belongs to the inode. A second `inotify_add_watch` for a directory (another
  spelling, or a renamed directory) gives the old `wd`: the new key is added to it, and every
  key of a watch gets every hint. The kernel watch goes with its last key. A watch has one
  key nearly always, so the first key lies in the table itself and only a second one asks
  for memory: 71 bytes of heap for a watched directory and no allocation that stays (92 bytes
  and one allocation before; the kernel keeps about 1 KB for a watch). The two tables are
  given back when most of their keys are gone: after 100,000 directories were unwatched, 67
  KB stay and not 6.1 MB.
- Hints: `MODIFY`, and `CREATE` or `MOVED_TO` of a non-directory → `Written` (the name holds
  other content now). Every other event of an entry → `Changed`. `ATTRIB` of a directory is
  dropped. `Q_OVERFLOW` → `Lost`. A full queue drops `IGNORED` and `DELETE_SELF` too, so a
  watch that the kernel gave up can stay in the table: the scans after `Lost` ask for every
  directory again, a new `wd` comes for such a one, and the old entry goes with it.
- The watched directory itself. `MOVE_SELF` → `Changed` with an empty `rel` for each key; the
  watch and its keys stay, and the next `watch` of the engine moves the key to the directory
  that is at the path now. `DELETE_SELF`, `IGNORED` and `UNMOUNT` → forget the watch, `Changed`
  with an empty `rel` for each key.
- A reader thread hands over the bytes of each read. It reads at most once a millisecond
  (`READ_INTERVAL`), and at once again after a read that filled more than half of its buffer.
  A first change is read when it comes; what follows inside the millisecond comes with the
  next read. While events wait in the queue, the kernel merges one that repeats the last one,
  and the two threads wake up a thousand times a second at most (the probe below). The watcher
  thread adds and removes the kernel watches itself, so nothing needs a lock.
- **A volume mounted or unmounted** over a watched directory, or over a directory above one, gives
  no inotify event (measured: tmpfs over a watched directory, its parent and its grandparent; the
  watches stay on the directories that the volume hides). The reader thread also waits on
  `/proc/self/mountinfo`, where the kernel tells of each mount and unmount of the namespace by a
  priority event (`POLLPRI`; measured: 19 to 27 ms after `mount`, none in idle time, none for a
  mount in another namespace). Then it reads the table again and compares it with the last one by
  mount id, as a volume mounted over another one at the same point is a change too, and each mount
  point that came or went becomes a record of its own (`wd` -2) → `Hint::Mount` with the path. The
  engine checks the node at that path, if it has one, and finds another directory there by its
  identity (`a_mount_over_a_watched_directory_is_told`,
  `a_volume_mounted_over_a_watched_directory`). The path is real, so a mount that a watched path
  reaches through a link is not found this way. (macOS tells it in its own way, 7.2.)
- `ENOSPC` → `Failed` with the watch limit error. Every other error → `Failed` with that error.
  `EMFILE` and `ENFILE` when the backend starts are the limit of inotify instances
  (`fs.inotify.max_user_instances`) or of open files: the watch limit error too.

Limits: `MOVE_SELF` comes only when the watched directory itself is renamed; a move of a
directory above it is told by the watches on the way. Expected and not tested: a removed
directory that a process holds open sends `DELETE_SELF` only when that process lets go.

Facts from probes on Linux (13 CPUs, the queue limit of 16,384 events that Linux has by
default, two events for a new file):

- With 20,000 directories, `watch` takes 0.6 to 0.7 µs for a directory (0.3 to 0.4 µs for one
  that has a watch), `unwatch` 0.6 to 0.7 µs, and `translate` 7 ns for an event.
- New files on a disk (btrfs), 200,000 a second from 4 or from 8 threads: no overflow, and
  every event comes.
- New files on tmpfs. 800,000 a second from 4 threads: no overflow. 900,000 a second from 8
  threads: 15 to 20 overflows in a run, and 36 to 47 % of the events come. A reader with the
  bare API that only counts gets the same, so the reader thread of the backend is not the
  cause: the writers hold the lock of the queue.
- Small writes into files of one watched directory, on a disk: every `write` call is an
  event. With a read for every wake-up, one writer (a million writes a second for half a
  second) cost the reader thread 220 ms on a CPU and the thread that takes its reads 130 ms,
  for 85,000 events in 75,000 reads. With a read a millisecond: 2 ms and 3 ms, for 190 events
  in 135 reads. Eight writers for five seconds: 3 s and 1 s before, 0.6 s and 0.05 s now, for
  the same four million events. No overflow in either case. One new file is read 60 µs after
  the call, as before. The floods above overflow as often as before, and the writers of the
  flood on tmpfs are faster (1.3 million files a second, from 0.8): the reader takes the lock
  of the queue less often.
- A directory that is shared with another machine (seen from a virtual machine, file system
  `fuseblk`): `inotify_add_watch` works, changes that are made inside the machine come, and
  changes that the other machine makes do not come at all. Nothing tells the backend so: the
  engine asks the type of the file system (5.12).
- The queue and its lock belong to the inotify instance. With the directories on two
  instances the overflow is gone or rare, with four it is gone, also when one thread reads
  all of them. This is not built: a user has 128 instances by default, an overflow needs more
  events than a disk gives, and the hints of one instance come in the order of the changes,
  which the tests of the backend rest on. After an overflow the engine reads everything
  again (`Lost`).

### 7.2 FSEvents (`fsevents.rs`)

Facts from probes on macOS (APFS):

- A stream watches whole trees and matches by path text. Records carry the **real** path:
  links resolved, the letter case and Unicode form of the disk.
- A renamed or moved directory gives one record for the directory and none for what is inside.
- A stream stands at the text of its path (the text is resolved once, when the stream is
  made). After a directory above its path is renamed, also only to another case of its
  letters, it reports nothing more, and it says nothing about that rename. When the
  directory is renamed back, the stream reports again. A rename of its own directory to
  another case gives one record for it; one through another spelling (`mv X away` where the
  disk has `x`), and a removal in that way, give no record for the directory.
- A directory that is renamed inside a stream gives two records, for the old path and for
  the new one, both with `ItemRenamed` and `ItemIsDir`, also when only the case changes.
- `kFSEventStreamCreateFlagWatchRoot` gives a `RootChanged` record when a directory at or
  above the path of a stream is renamed or removed. Not used, for three reasons, all
  measured. It looks at the path some 100 µs after the rename and sends the record only if
  the path is still another one: "rename away, make a file below, rename back" with no pause
  gave no record at all in 3 of 230 runs, and the file was not reported. It opens one file
  for each name of each stream path, and paths do not share them: 64 paths of 9 names are
  769 open files. And a stream with it takes 0.09 to 0.16 ms to make for each path (paths
  of 9 and of 11 names), without it 0.002 ms. (Other watchers that set it stop watching on
  that record, or report the watched directory as removed: Watchman, .NET, notify, watchdog.)
- Used by the guard (below): the kernel tells about a rename or a removal of a directory
  through a queue for vnode events (`kqueue`, `EVFILT_VNODE` with `NOTE_RENAME`,
  `NOTE_DELETE`, `NOTE_REVOKE`), on a descriptor of that very directory that was opened for
  events only (`O_EVTONLY`; it does not keep a disk from being unmounted: `diskutil eject`
  works, and the descriptor gets `NOTE_REVOKE`). Measured: the event is there when `rename`
  returns; two renames with no pause give one event; none was missed in 4,000 runs of
  "rename away, make a file below, rename back", of a rename that only changes the case, of a
  swap of two directories in one call, and of a directory that is replaced; the same on APFS,
  HFS+, exFAT and FAT32. Nothing comes on a descriptor of a directory below the renamed one, so
  each directory on the way needs its own. To open and register one takes 10 µs, and the
  kernel keeps 248 bytes for it (a `fileproc`, a `fileglob` and a `knote`; 10,000 of them
  added 10,000 of each in `zprint`); the process itself keeps nothing more. A queue that waits
  takes no time: 0.02 ms of CPU in 5 s with 10,000 directories. A dispatch source of type
  `READ` on the descriptor of the queue calls its handler 0.6 ms after a rename, and again
  while events wait: 200 that waited at once came in 4 calls of at most 64.
  `fcntl(F_GETPATH)` on such a descriptor gives the path of the directory as it is now, in
  0.3 µs, in the short spelling of a path on the data volume (`/Users/...`).
- A volume mounted on a directory (a disk image): the descriptors of that directory and of one
  below it get no event, and the one of its parent only `NOTE_WRITE`. A stream below the mount
  point gets no record, and goes on by the text of its path, into the volume. A stream at or
  above the mount point gets a record for it with `Mount` (`Unmount` when the volume goes),
  and with no `ItemIsDir`. The kernel queue tells of each volume that comes or goes
  (`EVFILT_FS` with `VQ_MOUNT | VQ_UNMOUNT`; also for a process that is not root, and only
  the kinds asked for), and not where.
- Flags are merged and stay set for many seconds. So flags are never read as "what happened".
- Stopping a stream throws away what it has not delivered.
- Records are lost without a word when one stream takes over from another while files are
  written fast (4 threads without a pause, 300 files). An old stream that was stopped as soon
  as it had delivered the event id of the moment of the start: a change was missed in 5 of 30
  runs, also with a synchronous flush before the stop. A new stream that was started from the
  event id of that moment (`sinceWhen`), with a flush and no overlap: 8 of 30 runs. With 500 ms
  of overlap: none in 40. With a loss report at the stop: none in 40, also with no overlap.
- Records are lost with a word when files come to one stream faster than it hands them over.
  A callback call of a stream brings at most 32 records. The flood of the probe: 20,000 new
  files from 4 threads, about 50,000 a second, in turn into 8 or more directories. A stream
  for the directory above them, or one stream with these directories as its paths, says
  `UserDropped` and `MustScanSubDirs` for each of its paths in 4 of 14 floods (with 64 paths:
  in 12 of 12, 1 to 15 times in a flood), and names 87 to 99 % of the files. With the same
  paths in 8 streams: in none of 72 floods. With 2 streams: in 1 of 10. No loss either with
  26,000 files a second, or with all files in one directory. A program with the bare API and
  a callback that only counts gets what the backend gets (a loss report in 7 of 15 floods
  each): the callback is not the cause. The latency of the stream (0 to 0.2 s) changes
  nothing. So the paths are spread over several streams, and a loss report that still comes
  (a flood below one path) has to be cheap to act on.
- A stream takes longer to start, the more files lie next to the program of the process: under
  1 ms next to few files, 0.1 to 0.2 s next to 100,000, 0.3 to 0.9 s next to 360,000 (a
  `target/debug/deps` that was never cleaned). The same program is fast again from a copy in
  another directory.
- `FSEventStreamStart` waits for an answer of fseventsd, with no time limit. While fseventsd
  is busy, a start took 16 to 430 ms, and 20,000 `watch` calls of one file each took 13.8 s
  in place of 0.47 s.
- Limits: 512 started streams for a process, 4,096 paths for a stream.
- A write to a file that stays open, and a write through `mmap`, is not reported before close.
- Many stream paths lose nothing, but they are slow while other programs write a lot. Time
  from the last write until all records were there, with 50,000 other files written: 0.1 s
  with 1 or 100 paths, 0.9 s with 500, 1.8 s with 1,000, 3.5 s with 2,000, 6.9 s with 4,000.
  A stream with one path in the same process was as late. With no other writes, 2,000 paths
  take 0.1 s.

As built:

- Stream flags: `FileEvents | NoDefer`, latency 0, since "now". One serial dispatch queue serves
  all streams. The callback only copies the path and the flags of each record into the sink.
- **Cover.** A directory that the engine watches and that a stream reports, filed under its
  real path with every key, in a table by path (`Dirs`). `watch(key, dir, Contents)`: if no
  stream reports the real path of `dir` yet, it goes into the stream of the next `commit`; the
  answer is `WatchedTree`. `watch(key, dir, Names)`: `WatchedTree` if a stream reports the
  directory, else `Watched`: names come and go in a directory on the way only when one of them
  is renamed or removed, and the guard of that one tells (below), as its own guard does for
  the directory. A stream never stands on `/`, `/Users` or a home directory
  (`may_hold_a_stream`), where every program of the user writes: a cover there is answered
  `NotWatched` and polled, as a handle of Windows is never put on `C:\`, `C:\Users` or
  `C:\Users\<name>` (7.3). (Before, a watched path whose lowest existing directory was the
  home directory put a stream on the home directory: 2,000 files written elsewhere in it gave
  3,680 records, each dropped after one lookup.)
- **Stream paths.** A stream path is a directory that a stream was started for. Usually it is a
  cover with no cover above it. With more than 64 stream paths (`MAX_STREAM_PATHS`; the probe
  above: up to 100 paths cost nothing), a commit places all covers again by the rule of
  `place`, the same rule as for the handles of Windows: paths go up to directories that covers
  have in common, the deepest first, until 64 are left. A path never goes up to a
  directory that is too high (`is_too_high`): `/`, a directory in `/`, one more name down (a
  home, the top of a volume, `/private/tmp`), and the temporary directory of a user with the
  directories above it. Where only such a directory would do, the paths stay apart. A cover
  below a running stream path needs no new stream. `place` sorts the covers and walks them
  once: it finds the depth of the last directory that has to be taken, takes every common
  directory below that depth, and at that depth one after the other while too many paths are
  left. For 20,000 covers in a deep tree that is 13 ms, for 100,000 it is 69 ms (38 ms and
  724 ms with a pass over all paths for every directory that was taken).
- **The real path** of a directory is made of the real path of its parent, which is kept until
  the next commit, and of its own name as the disk spells it (one `getattrlist` call). A link, and a name that differs by more than the case of ASCII
  letters, are left to the OS (`realpath`).
- **Streams are only added.** `commit` starts streams for the new paths. Old streams keep running,
  so nothing is ever in a gap. With 64 streams running (`MAX_STREAMS`), with too many stream paths,
  or when a new path lies above a running one, `commit` places all covers again: so no stream path
  lies below another, and no record comes twice for long. The covers are then placed on 56 paths,
  not 64: room for the paths of the next few commits, so that a few new directories do not replace
  every stream again. A running stream whose paths are all placed again, and still trusted, goes on
  (the newest of two on one path): new streams start for the other paths only, and only the streams
  that do not fit are replaced. With 64 streams running, only the 24 with the most covers at and
  below their paths go on, so that the next commits have room. (Before 2026-10-19 every stream was
  replaced: after one `watch` call for each of 20,000 files, the stops of the old streams made the
  engine read 44,058 entries again, 11,990 now.) If the streams cannot start, `commit` names every
  key of the new paths as failed, with the watch limit error, and the old streams keep running.
- **Up to 8 streams for a commit** (`MAX_NEW_STREAMS`; more only where a stream would get more
  than 4,096 paths). The paths of a commit go to its streams in turn: so neighbours in the
  tree have different streams, and a flood over many watched directories is shared by all of
  them. One stream for 64 directories reported a loss in
  every flood of the probe, 8 streams in none; 40 directories of a second `watch` call in one
  stream reported a loss in 10 of 12 floods, in 8 streams in none. A loss report makes the
  engine read all covers of the stream again. The 8 streams take 3 ms longer to start than
  the one. Two streams keep no order between their records: the engine needs none. The limit
  of 64 running streams is high so that the streams are replaced seldom: one `watch` call for
  each of 20,000 files took 540 ms with a limit of 16, 420 ms with 32 and 350 ms with 64
  (390 ms before, with one stream for all covers and a limit of 8). What stays: covers that
  share one stream path (more than 64 directories with one directory above them, or a watched
  tree) have one stream, and a flood below it can still end in a loss report.
- **An old stream that stops.** A replaced stream is stopped as soon as the new ones run, and the
  backend reports each cover at and below its paths lost: the engine reads them again, as a stream
  that stops takes with it what it has not delivered, and the new one may not have told everything
  from its first moments (see the probe). What the old stream did not deliver is on the disk, and
  the read after the stop finds it. A cover above its paths has a stream above of its own, as
  trusted stream paths never lie one below another: that stream goes on, or stops with a report of
  its own, or came with the replacement and started before the cover was read, as for every new
  cover. (Before 2026-10-19 the report was for `/`, so `Lost`, and the engine read everything. Until
  2026-10-19 a replaced stream also ran on for 500 ms next to the new ones, with a timer of the
  guard's queue that brought the stop: the report alone was measured to lose nothing, and a probe
  of 480 replacements, each under a burst of 300 files from 4 threads, lost nothing with the report
  alone, and nothing with no report either.) Streams that stop at the same time share one report,
  and the scans of loss reports are paced (5.13). A stream stops on the queue of the streams, and
  the report goes from there after the stop, but the thread of the watcher does not wait for it: a
  callback can keep the queue for a long time, and under a busy fseventsd the waits for 1,161 stops
  in one `watch` call for each of 20,000 files came to 21 to 27 s. The `Drop` of the backend waits
  on the queue, after these stops: no report comes after it.
- **Hints.** A record becomes one `Changed` for each key of each cover that holds its path;
  the rest of the path is `rel`. The covers are found by one table lookup for each directory
  above the path that can be a cover. A record under no cover is dropped: a stream path on a
  common directory also reports what lies next to the covers. A directory record without
  `Created`, `Removed` or `Renamed` is dropped. A record with `Mount` or `Unmount` is for a
  directory, though it does not say so.
- **Lost records.** A record with `MustScanSubDirs` for a path other than `/` → `Changed` with
  an empty `rel` for each key of every cover that holds the path, and of every cover below the
  path. If there is no such cover, no hint: no key can see that place. A record for `/`, and
  one with only `UserDropped` or `KernelDropped` → `Lost`. These hints come after the other
  hints of the callback call. The covers are kept in the order of their paths (`Dirs`, an
  ordered map), so the covers below a path are one search away: 0.2 to 0.3 µs for a path
  with 50 to 100 covers below it, among 2,000 covers or 20,000. A flood of renamed
  directories costs one search for each record. A record finds the covers that hold it in
  the same map (75 ns with 2,000 covers, 90 ns with 20,000; a hash map took 65 to 75 ns).
  No second list of the paths is kept. (A look at every cover for every record took 5 to
  10 s for 4,100 records and 4,100 covers in one call. A sorted copy of all cover paths,
  made after each change of the covers and kept, was as fast as the map, and held every
  path once more.)
- `unwatch(key)` forgets the key. The path stays in its stream until the streams are replaced,
  or until no path of that stream has a cover at or below it any more: then the stream is
  stopped at the next commit, with no loss report, since none of
  its records would reach a key. With no key left at all, the streams that a replacement left
  running are stopped at the next commit too, before their time: nothing stays open.
- **A cover is a text.** A stream stands at the text of its path, and a record finds its
  cover by the text of the cover. Both hold only while every name in them is the one that the
  disk has. Each way for a name to change has one place that finds it, and all of them answer
  in the same way: the cover is reported lost, and the engine asks for it again (which reads
  its real path anew) and scans it deep. A directory at or above a stream path: its guard. A
  directory inside a stream: the record of its rename. A change between
  `watch` and the start of the stream: the check after the start. A link that leads
  somewhere else: the poll (5.14). The notes that follow are these four.
- **The guard.** Each directory at and above a stream path, and each directory that a key asks
  names of and no stream reports, is open for the vnode events of the kernel (`Guard`):
  `O_EVTONLY | O_SEARCH`, and `EVFILT_VNODE` with `NOTE_RENAME`, `NOTE_DELETE` and
  `NOTE_REVOKE`, under an id of its own (a descriptor number comes back for another directory).
  One kernel queue serves the backend, and a dispatch source on the queue of the streams reads
  it (`Notices`): no thread waits for it. It puts the ids that told in a list, and its signal
  only wakes the engine: a full queue of the watcher drops the signal, never the list.
  `translate` takes the list, and so does `commit`, which comes after the engine checked
  everything for a dropped signal (before, the guards stayed on the old directories:
  `a_notice_whose_signal_was_dropped_is_taken_by_the_commit`). For each id that told, a key that
  asks names of a directory at or below the one that told gets a hint for its directory, each
  stream path at or below is trusted no more (it leaves the list of stream paths), and every
  cover at or below is reported lost, also when the path is back by then: the stream heard
  nothing while its path was another one. The engine watches each cover again, which resolves
  its real path anew and starts a stream at it, and scans it deep; the stream that stood there
  is idle then, and the next commit stops it. The guard that told is closed, as another
  directory may stand at its path now, and so is each guard below it: its directory left with
  the one that told, and no notice comes on it. The next commit opens one where one is still
  needed. (Kept open, the guards below held the old directories, and a rename of a new one at
  their paths went unnoticed: `a_directory_above_a_cover_is_replaced`.)
  - A volume mounted on a guarded directory, or above one, tells no guard, and no stream
    below it. So the kernel queue also tells of each volume that comes or goes, and then
    each guard whose open directory is not the one at its path now (device and inode: one
    `stat` for each guard) counts as one that told (`a_volume_mounted_above_a_cover`). A
    volume inside a stream is told by its record (below).
  - At a commit when the stream paths or the directories asked for names changed, the guards
    that the new streams need open before they start: a rename between the guard and the
    start is told, and one before the guard is found by the check after the start. The
    guards that nothing needs any more close at the end of the commit: the old stream paths
    keep theirs until new streams stand, and for good if none could start.
  - The guards of all watchers of the process take at most half of the open files that the
    process may have (`getdtablesize`): 128 for a program started from a shell, which gets
    256; Node asks for far more when it starts. A directory that gets no guard, for that or
    another reason, gets no stream below it: the keys of the directories at and below it
    are named as failed (the watch limit error, `ulimit -n`), and they are polled and asked
    again. A directory that is not there is no error, but it gets no stream below it either:
    the keys at and below it are reported lost, as it may be back by then, with no guard and
    nothing to tell its next rename. The engine checks them and watches them again, and the
    next commit opens the guard (`a_directory_away_when_its_guard_opens_is_checked_again`).
  - So nothing is polled on the way to a watched path, and an idle watcher has no timer
    (5.10). The project of the bench with 20,000 watched files has 40 stream paths and about
    50 guards.
  - Before 2026-10-15 the real path of each stream path was asked at each commit
    (`realpath`), which a poll of the directories above the covers brought once a second:
    1.6 ms a second for 40 stream paths, 23 to 28 s for 20,000 `watch` calls of one file each
    (0.44 s now, side by side), and a rename taken back within the second, with changes below
    meanwhile, went unnoticed.
    (Before 2026-10-13 a guard ran next to that poll, with a thread of its own, for the
    second it saved; it was taken out.)
- **A stream that cannot start** fails every path of the commit, with the watch limit error:
  `commit` names the keys of their covers as failed, so that they are polled and asked
  again, and the old streams stay.
- **The check after the start.** The real path of a cover is asked at `watch`, and its stream
  starts at `commit`. A directory above that was renamed in between leaves the stream at a
  path where nothing is. So after the start, the real path of each stream path that is new
  is asked again, in the way it was asked before (`realpath`, 8 µs). One that is another
  path now is reported lost.

  A directory between a new stream path and a cover below it can get another spelling of
  its name in that time too (`src` to `Src`: the watched path stays valid, so no scan finds
  it, and no stream was there to report it). The cover would stand at a path that no record
  spells, and hear nothing. So each name between a new stream path and a cover below it is
  asked from the disk after the start (`getattrlist`, 1 µs for a directory, each directory
  once), and a cover with a name that is spelled in another way, or that is a link now, is
  reported lost. Measured with 20,000 watched files in 2,000 directories: 2 ms of a setup of
  50 ms. A watched tree has nothing below its stream path and pays nothing.
- **A path through the data volume.** `realpath` keeps a path that is spelled through
  `/System/Volumes/Data` (where `/Users` and `/private` really are), and FSEvents reports the
  directory under its short path: such a cover heard nothing. So a real path with that
  prefix is taken without it, when the short path is the same directory (device and inode)
  and is a real path itself. `/home` is the same directory as `/System/Volumes/Data/home`
  too, but it is a link, and a stream at a path that is not real is never trusted (the
  check after the start), so that one stays long. The directory itself is asked, not only
  its parent: `/System/Volumes/Data/Users` has a parent with no short way, and is `/Users`.
- **A cover at another real path.** A key that is watched again and whose directory has
  another real path than before is reported lost, if its old path still has a stream that
  is trusted: then a directory on the old path was renamed and the notice of its guard was
  not read yet, or a link leads somewhere else. Without this, a key that
  asks for more in that moment would take the new path with a scan of its own directory
  only. (A key below a stream path that is trusted no more was reported with it.)
- **Covers below a renamed directory.** A record for a renamed directory inside a stream, or
  for a volume that came or went there (`a_volume_mounted_inside_a_stream`), is also a loss
  report for every cover below the path of the record. The engine finds such a
  cover by itself when the renamed directory is a node of its tree. It is none when a link
  leads to the cover from somewhere else.

What stays open:

- A link on the way to a watched path that points somewhere else now is found by the poll
  (5.14), within a second.
- A stream path that is trusted no more keeps its stream until the streams are replaced,
  unless no path of that stream is trusted any more: then the stream is stopped at the next
  commit. While it runs on and its directory is back, that stream and the new one both
  report it, so a record comes twice, which costs one more table lookup and nothing else.
- A directory inside a stream that gets another spelling of its name: each cover below it
  is scanned deep twice, once for the record of the rename and once because its real path
  is another one. The scans are paced, and only what differs is reported. The second report
  cannot go: a cover whose path changes has missed the records that spelled the new path,
  whatever made it change, and the backend does not know that a scan is on its way.

### 7.3 ReadDirectoryChangesW (`windows.rs`)

The tests of this backend (`tests/windows/os.rs`, and `tests/windows/parts.rs` for the parts that
make no Windows call: `place`, `Handles`, `Records`, `translate`) pass on Windows 11 with NTFS.

- One thread owns one I/O completion port and every directory handle (`port`). A handle
  reports its directory and everything below it. A request that needs a new handle is
  answered at once, and the handle is opened by the next `commit`, for all the requests of a
  flush together: `place` is asked once, not once for each directory (2,000 directories:
  350 ms the one way, 10 ms the other, measured).
- **Where the handles go** (`place`): each directory that wants `Contents`, with no other such
  directory above it, gets a handle. It serves every key at or below its directory. With more
  than 64 handles, handles go up to a directory that they have in common and become one: the
  deepest common directory first, until at most 64 are left. No handle goes on `C:\`,
  `C:\Users` or `C:\Users\<name>`. A handle reports nothing behind a junction or a directory
  link below it, so a directory with a link on the way down from the common directory is not
  joined there (nor is any directory it was joined with deeper down): it keeps a handle of its
  own, which can stand below the shared one, and the others share without it. A key goes to
  the deepest handle at or above its directory. The links are found where a key would come to
  lie behind one (`System::is_link`, one look for each name on the way, once per settle), and
  the placement is made again with them known, before a handle moves.
- **Answers.** A key that wants the contents and that a handle serves: `WatchedTree`. A key
  that wants names only and that a handle serves: `Watched` (`WatchedTree` would make the
  engine rely on it for the directories below, and only a key that wants the contents holds a
  handle). A `Names` request that no handle covers, and a directory that
  could only be watched from one of those three directories: `NotWatched`; when a handle
  comes to stand at or above such a directory later, `commit` returns its key. A key that the
  commit can give no handle (no room and no allowed directory in common, or the open fails)
  gets `Changed` with an empty `rel`, the engine asks again, and that request is answered
  `Failed` with the reason, or `NotWatched` when the handle that came to serve it stands
  above a link.
- `Handles` keeps every served key. When a key comes or goes, the handles move to where `place`
  puts them, once at the next `commit` for all the keys of a flush: `unwatch` only closes a
  handle that no key uses any more (rule 6: the OS watch goes with its last key), and the
  engine commits after every flush that gave a watch back, also one that queued no scan. New
  handles are opened first, then old ones are closed. A key that got another
  handle is not told: the new handle reported before the old one stopped, and the old one is
  read until the kernel holds nothing more for it, then closed; the read that the close
  cancels fails (`ERROR_OPERATION_ABORTED`), so it is not taken for lost records. The call
  of the backend returns when the closes are done: a directory above a closed handle can be
  renamed then. If one open fails, nothing changes; a handle that only keys wanting names
  still use closes with the last of them. A `watch` for the very directory of a handle puts a
  new handle there (the old one may be on a directory that was renamed away), unless the key
  only waited for a handle, or got none at the last commit. A handle whose read fails is
  given up and its keys are told; when the next handle on the directory dies too before it
  delivered anything, the ask after that is answered `Failed` with the error of the read: the
  directory is polled, and tried again at each poll. The tables of the keys are given back
  when most of their keys are gone, as the tables of inotify are (7.1).
- Filter: `FILE_NAME | DIR_NAME | SIZE | LAST_WRITE`. 64 KiB buffer. The bytes of a
  completed read are read on the watcher thread (`Records`, `translate`): the raw
  notification holds the bytes and shares the path of the handle, and the path of each
  record, with its lower-case spelling, is made into room that `translate` keeps from one
  record to the next, so a flood of records asks for no memory. The reads ask for the longer
  records (`ReadDirectoryChangesExW`, Windows 10 1709 and later) where Windows and the disk
  have them: they tell whether an entry is a directory.
- **Hints.** A record goes to every key whose directory holds its path, the deepest first,
  with the names compared in lower case (rule 7): two watched paths that are two spellings of
  one directory both get it, and so does each key above. The lower case is the one of each
  character (`char::to_lowercase`), measured against NTFS, which takes `οδοσ` for `ΟΔΟΣ` and
  not `οδος` (the final sigma that `str::to_lowercase` gives a word), nor `STRASSE` or
  `STRAẞE` for `straße`, nor the Kelvin sign for `k`, nor `I` for `ı`; the fold takes `STRAẞE`
  and the Kelvin sign for one with theirs, which costs one check too many and misses no
  record. A key above gets a hint it may not
  need, which costs one walk; without it the path would be missed where the tree reaches it
  only from above: a second spelling of a name below, or the other letter case of the drive,
  which is another top node of the tree. A record that no key holds is dropped, and so is one
  whose name is no name of an entry below the directory: a `..`, a root, a drive, a NUL, or a
  `:`, which starts a stream of a file (NTFS gives none of these; a write to a stream of a file
  is a `MODIFIED` record for the file, measured; a disk that another system filled, or a
  server, could give one, and a `..` or a drive would reach another directory). `MODIFIED` →
  `Written`; it is dropped when it names a directory (the longer records tell) or the
  directory of a key: Windows sends it whenever an entry in a directory changes, and that
  entry has a record of its own. Every other record → `Changed`. A read with no bytes, and a record with no name →
  `Changed` with an empty `rel` for each key at or below the directory of the handle. Bytes
  that cannot be read → `Lost`. A read that fails → the handle is closed, `Changed` with an
  empty `rel` for each of its keys. A short (8.3) name in a record is a name like any other:
  below a watched directory the engine asks the disk how it spells a name it does not know,
  and finds the entry under its long name; one that no key holds is dropped (README, "Holes").
- The kernel completes a read as soon as one record is in it. After a read that came back
  with less than half the buffer, the next read is queued a millisecond later: while no read
  is queued the kernel keeps the records in a buffer of its own, as big as ours, and the next
  read takes them all. Without the rest a flood is one read, one packet and one wake-up of the
  worker and of the watcher thread for each record (93,000 reads for the 63,000 records of
  30,000 new files in an ignored directory, 2 s of CPU; measured). The first record of a quiet
  handle is not delayed, as the read that waits for it comes back at once; a read that came
  back fuller is queued again at once, as the flood is heavy; a save of 20,000 records a
  second fills 2 KiB of the kernel's 64 KiB in the rest.
- Directories are compared by path text, and a handle reports nothing for what is behind a
  junction or a directory link below it. So each name between a handle and a key that it
  would serve is looked at (`leads_through_a_link`), and a key behind a link, below a
  directory that has a handle anyway, is answered `NotWatched`: the engine polls it. A
  directory behind a link that wants the contents gets a handle of its own (above), so such
  a key never makes the handles move back and forth.

### 7.4 Poll (`poll.rs`)

`watch` answers `NotWatched`, and nothing else does anything. See 5.10.

## 8. Tests

**The simulator** (`tests/sim`: the fixtures in `fixtures/`, the tests
beside them; on Unix hosts). `SimFs` is a file system in memory with inodes, a clock, a change time for each
entry, and a mode that ignores the case of letters. `SimBackend` plays an OS watch API on it in
three modes: by inode (inotify), by path prefix with real paths (FSEvents), by handle for a tree
(Windows). A stream of the second mode ends when a directory at or above its path is renamed or
removed, and says so at once, where the real backend finds it out at its next commit (7.2). The test decides when each
notification is delivered: now, late, twice, or never, with or without the word that it was
lost. The disk can change in the middle of an engine call.
The backend keeps a log of every `watch` and `unwatch` call, and the disk tells where an OS
watch is now. Not played: `MOVE_SELF` of inotify, and the shared handles of Windows.

**The two checks at every settle** (`src/caught_up.rs`). Each time a test lets everything
settle (every notification delivered, every delay over, and more than two polls), the fixtures
check, by rules that they write down a second time:

1. each fingerprint that the engine stored equals a fresh look at the disk,
2. the told set is the set of files that a walk of the disk finds from the watched paths.

Every story ends with this (`caught_up`), or says why it cannot (`not_caught_up`). A batch is
sorted before it is compared, so no test depends on the order of the engine. An engine whose
deadline never passes fails the test and does not hang it. **The invariants** of 5.1 are
checked after every engine call.

The test files of the simulator:

- `scenarios`: every row of section 3, in each mode and with the poll backend.
- `check_table`: one test for each row of 5.4. `baseline`, `young`, `delete_delay`: 5.7 to 5.9.
  `unwatch`: 5.7. `want`: 5.10, and where the OS watches are put. `filter`: 5.11.
  `spelling`: 5.2.
- `links`, `moved_directories`, `failures`, `timers`, `regressions`: every case that was once a
  way to miss a change, or to report one that did not happen, by topic. `regressions` has the
  ones that fit no topic.
- `fuzz`: random file system operations mixed with deliveries, clock steps, `watch` and
  `unwatch` calls and failures, also a disk that stops answering in the middle of an engine
  call. A run plays the handler by its own rules. When everything has settled, the handler
  must know exactly the files that the watched paths stand for, each with the content it has
  now, and the two checks must hold.
- `sim`: the fixtures themselves, and that the checks above fail when they have to.
- `allocations`: budgets for how often the engine asks for memory; a test fails when one is
  passed.
- `cost`: no test, a measure. It prints what the engine costs for big cases (20,000 watched
  files, a directory of 50,000 files, 100,000 hints, a polling round, `unwatch`), in time,
  allocations and bytes. Run it before and after a change of the engine and compare.

**The other tests** (`tests/`, and the `mod tests` of `src/`):

- Private functions (a `mod tests` at the end of the file): `tree/mod.rs`, `tree/names.rs`,
  `path.rs`, `fs/mod.rs`, `fs/windows.rs`, `engine/events.rs`, and helpers of the backends.
- The tree alone (`tests/tree.rs`): the arena, the table, and the way from a hint to a mark.
- The engine on a real directory with the poll backend and a clock that the test moves
  (`tests/poll.rs`): a file whose time is ahead of the clock (5.8; it counts the reads), the
  rest after a poll (5.13), the engine after its backend stopped (5.10), and the hash of a
  file: the limit, and that nothing but a regular file is opened (5.8).
- Each backend alone (`tests/inotify.rs`, `fsevents.rs`, `windows/os.rs`, each on its OS);
  `windows/parts.rs` runs on Windows only. In `fsevents.rs` a test
  that makes many changes runs alone, and after the others where it can (`Turn`): a flood
  makes FSEvents report losses, and a test next to it was seen to get one too. No test there
  starts when more than 50,000 files lie next to the test program (the probe in 7.2).
- The watcher thread (`tests/watcher.rs`): `watch`, `unwatch`, `close` and `drop` from every
  side, relative paths, a bad path, the queue bound, a backend that cannot start.
- The stories (`tests/stories`): `contract` (the rows of section 3), `projects` (what editors
  and big projects do), `promises` (the promises of the README; its first lines say which test
  proves which), `engine` (what the engine has to get right below the promises: a batch of
  paths, a directory that becomes a watched path above others, files and links on the way,
  directories that are renamed or replaced), `operations` (every way a user writes, renames
  and links a file; a hard link; two watchers in one process). Each story runs on the real disk, once with the backend of the OS and once
  with polling, both at the same time; a story that needs no more than the words of `World`
  (files, directories, links, renames, `watch`, `unwatch`, `expect`) runs first on each
  simulated backend of `tests/sim`, where it takes no time and its end is checked against
  the disk. A story that sets a delete delay skips polling, which has none (5.9), and says
  so. On the real disk these tests wait, they do not compute. Measured on macOS, each test alone: of
  56 s, 34 s are the quiet time of 300 ms after each `expect`, 17 s the wait for the events
  (the debounce, the delete delay, the poll of a link once a second, and about 10 ms of the
  OS), 4 s the changes that the tests make, and 0.2 s the `watch` calls.

How to run them (the README has the commands for the other platforms and the lints):

```sh
cargo test --workspace                # everything for this OS; 250 fuzz seeds for each backend
PATHWATCH_FUZZ_FIRST=100000 PATHWATCH_FUZZ_SEEDS=20000 \
  cargo test --release -p pathwatch --test sim fuzz::big -- --ignored         # the long fuzz
PATHWATCH_FUZZ_SEED=1234 \
  cargo test -p pathwatch --test sim fuzz::replay -- --ignored --nocapture    # one seed again
cargo test --release -p pathwatch --test sim cost:: -- --ignored --nocapture  # the cost program
```

## 9. Not in version 1

Left out on purpose: events for directories; rename pairs; a report of what exists at `watch`
time, and changes from before the call; globs; a depth option; following symbolic links inside
a watched directory; an option for the one-second poll; kqueue (FreeBSD
uses polling).

Known holes of the engine:

- Linux: every disk is taken to tell two spellings of a name apart. On one that does not (a
  folder of a Mac or of Windows shared into the machine, a FAT or SMB disk, a directory with
  the casefold flag), a watched path that differs from the disk only in the case of its ASCII
  letters still gets its events: a hint for the one name marks the other (5.2). Below a
  watched directory the events can be wrong: a change made through another spelling of a
  name is reported under a name the disk does not have, or not at all, and a rename that
  changes only the case leaves the old name known (measured with the Linux build on a folder
  shared from macOS, with polling too).
- A hint with a plain name never finds a registered name that is not plain, and the other
  way round (5.2). APFS takes a few characters for plain ones (measured): `ß` for `ss`, the
  long s for `s`, the Kelvin sign for `k`, a ligature like `ﬁ` for its letters, and each of
  the 1,002 CJK compatibility ideographs for its ideograph. A watched path that is spelled
  with one of them, where the disk has the plain spelling, gets no events from an OS watch.
- Under polling, a young file above 16 MiB, and one that cannot be read, gets one `Update`
  when it is 2 s old, also when nothing changed, and also one that was written just before
  `watch` (5.8).
- A change of a link on the way to a watched path, or of a watched path that is a link, is
  found only by the poll (5.14).
- Equal fingerprints (5.8) where no `Written` hint comes (FSEvents, polling): a rewrite of the
  same size that puts the old modification time back; a file that is removed and, inside the
  delete delay, replaced by another one of the same size and time on a file system that uses
  the inode number again (ext4); a second write of the same size inside one step of a file
  system that counts time in steps between 20 ms and 1 s.
- Linux and Windows: a write through another hard link of a watched file, under a name in a
  directory that is not watched (measured on NTFS: the record names the entry that was
  written).
- FSEvents and Windows: after `rm -r` of the directories above a watched path, the engine
  learns that an upper directory is gone only at the next poll.
- Where the OS watches are put depends on what was watched and unwatched before: a stream stays
  where it is until the streams are replaced. Nothing is missed.

Known holes of the backends are in 7.1 to 7.3. The Windows backend has only run on Windows 11 with NTFS.
Android was never run: it is set to use the same code as Linux, and only type-checked.

## 10. Words

The code names things in plain words, and uses each word for one thing. This is the list, in
the order in which a change meets them.

- **Watched path.** A path given to `watch` and not taken back. Its node `is_path`.
- **Registered.** A node that is a watched path or leads to one (`is_ancestor`). A registered
  node stays in the tree while nothing is at its path.
- **Key.** A `DirKey`: the slot of a node in the arena and a generation. The backend knows a
  directory by its key and never by its path, so the two sides need not agree on a spelling.
- **Complete, partial.** A directory node that tracks every entry of its directory, or only its
  registered children. A watched directory is complete; a directory on the way to a watched path
  is partial.
- **Want.** What a directory asks the backend to hear about: `Names` (entries that come and go)
  or `Contents` (also writes). For one key the want only goes up, until `unwatch`.
- **Arm, narrow.** To ask the backend for the watch of a directory because it wants more than was
  asked, or because the watch may be gone; and to give a watch back because the directory wants
  less after an `unwatch`.
- **Cover.** A watch above covers a directory below it (`Watch::FromAbove`). In the FSEvents
  backend, the real directory that a key stands for.
- **Place.** Where the OS watches are put when there are more directories than the OS lets a
  process watch at once: the deepest directories that the paths have in common.
- **Hint.** What a backend says about a raw notification: where to look (`Changed`, `Written`),
  or that notifications were lost. A hint never says what happened.
- **Mark.** "Check this name at the next flush." A hint leaves one, and so does the engine when it
  has to look again. A mark says whether a directory is to be scanned deep, whether the file was
  written, and whether only a loss report asked for it.
- **Flush.** The pass that checks every mark and then runs the scans, round after round, until
  nothing is queued. `finish` reads the events off the tree when it is over.
- **Look.** One read of the disk for one path: an `lstat`, a `stat`, or a listing.
- **Check.** The look at one node and what the engine does with what it found: the table of 5.4.
- **Scan.** The check of every node of one directory, with a listing when the node is complete.
  `Names`, `Shallow` or `Deep`: 5.5.
- **Appear, still there, vanish.** A directory is found where the tree had none; is still the
  same directory; is gone, with everything below it.
- **Anchor.** In `register`: the deepest node on the way to a new watched path that has only
  directories above it on disk. The first look is made there.
- **Told.** The handler believes that a file is at the node. What was told and what the tree has
  decide the event at the end of a flush.
- **Baseline.** What a `watch` call finds: known to the handler without an event.
- **Young.** A file whose modification time is so recent that another write could leave it as it
  is. A hash of its content is kept to compare at the next look.
- **Fingerprint.** The modification time, the size and the id of a file: what tells one state of
  it from the next without reading it.
- **Held.** A look found nothing where the tree has a file or a directory. The node waits for a
  second look, one delete delay after the hint, and only that look decides.
- **Frozen.** A node below a held node. Nothing is done there before the second look above.
- **Uncover, let go.** A node is no watched path any more and no watched directory covers it:
  what the tree has there is forgotten. A node that is not registered any more is an entry like
  any other, or is freed.
- **Loose, strict.** A directory where two spellings of a name are one name (letter case,
  Unicode form), or two names. Every directory of a watcher is the one or the other
  (`Options::loose_spelling`).
- **Plain.** A name of ASCII characters and Han ideographs only, so that its other spellings are
  only the case of its ASCII letters, and the table of the tree finds all of them.
- **Twin.** Another node of the same directory that can be the same name as this one.
- **Unlisted.** A registered node whose entry the directory above lists under another spelling.
  It is the path of its caller, and no entry of that directory.
- **Rest.** After the scans that a loss report asked for took a time `t`, such scans wait
  `9 × t` before they run again, half a second at most. The pause after a poll is the poll
  interval, and at least the time the round took.
- **Settle** (tests). Deliver every notification and run every deadline until nothing is left.
