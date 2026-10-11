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
watcher.watch_all(["/project/src/index.js", "/project/content", "/project/not-there-yet.json"])?;
watcher.unwatch("/project/content")?;
watcher.close();
```

| Name | What it is |
|---|---|
| `Watcher::new(handler)`, `with_config(config, handler)` | Start the watcher thread and the backend. They fail only when the thread cannot start. When the watch API of the OS cannot start, the handler is told once (`WatchLimit` or `Io`) and the watcher polls everything. |
| `watch(path)`, `watch_all(paths)` | Register a batch and return when every path is watched and its state is known. What exists then is the **baseline** and is not reported. A path that is watched already is skipped. |
| `unwatch(path)`, `unwatch_all(paths)` | Take back a `watch` of the very same path, and report nothing (5.7). |
| `close()` | Stops the backend and waits for the thread. After it the handler is not called again, and `watch` and `unwatch` return `Closed`. Dropping the watcher calls it. |
| the handler | `FnMut(Result<Vec<Event>, Error>) + Send + 'static`. It runs on the watcher thread. A call of the watcher from inside it does not wait: it takes effect when the handler has returned. |
| `Event { path, kind }` | `EventKind`: `Create`, `Update`, `Delete`. |
| `Error` | `kind()` (what the caller can do about it), `path()` (one path that shows the problem; none when the problem is the watcher itself), `source()` (what the OS or the backend said). `ErrorKind`: `InvalidPath`, `WatchLimit`, `PermissionDenied`, `Io`, `Closed`. One error of a kind is handed over for a flush. Its text is these three in a row (see below). |
| `Config` | `with_ignored(f)` (5.11); `with_debounce(d)`: the quiet time, 50 ms (5.13); `with_delete_delay(d)`: the delete delay, 100 ms, zero turns it off (5.9); `with_polling(bool)`; `with_poll_interval(d)`: 100 ms, at least 1 ms, used when the watcher polls everything. A time of the settings is a year at most (`Engine::new`): `Duration::MAX` says "never", and is no time that can be added to a clock. |

`Watcher` is `Send + Sync`.

Paths (`clean` in `watcher.rs`, then `path::normalize`): a relative path is joined to the
current directory. `.`, repeated separators and a trailing separator are dropped, and `..` takes
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
what to raise. (Before, the kind named one limit for each OS, also when another one was met:
on macOS the limit of streams, when the limit of open files had stopped the guard.) An error
without a path is about the watcher: its backend did not start or has stopped, and the source
says which, and that every path is polled now.

Not an option: with a native backend the poll of what the OS does not watch runs once a
second. `Config` turns `loose_spelling` on for macOS and Windows: there every disk takes two
spellings of a name for one. Elsewhere each directory is asked (5.2).

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

| Module (`crates/pathwatch/src`) | Holds |
|---|---|
| `config`, `error`, `event`, `watcher` | The API. `watcher.rs` has the one thread and its queue, and picks the backend. |
| `engine/`, `tree`, `fs`, `path` | The engine, the tree, the disk as the engine sees it (`Fs`), path cleaning. No thread, no clock, no OS watch API. |
| `backend` | The `Backend` trait and its types. |
| `inotify`, `fsevents`, `windows`, `poll` | One backend each, in one file. Of the crate they use `backend`, and the calls of the system in `fs` that the engine makes too. |

The modules that are not the API are `pub` but hidden from the docs: the tests need them. A
second crate, `pathwatch_sim`, is not published: the fixtures and the tests of the engine
(section 8).

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
| delete delay | the time from the hint to the second look: 100 ms | `Options::delete_delay` |
| young | a file whose modification time is too close to the check to trust | `Content` |
| quiet time | the debounce: how long no hint must come before the disk is checked | `Options::debounce` |
| rest | the pause after work that no hint asked for by name: a poll, the scans after loss reports | `REST`, `rest_until` |
| cover | FSEvents only: a directory that the engine watches and a stream reports | `Covers` in `fsevents.rs` |
| stream path | FSEvents only: a directory that a stream was started for; it reports the whole tree below | `Streams::paths` in `fsevents.rs` |

`tree/mod.rs` holds the data, and `engine/` has one file for each job (`mod.rs` says which).

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

How it is stored (only `tree/mod.rs` knows it): all nodes live in one arena. A key is an index and
a generation, so an old key never names a new node. A node has the list of its children, and
one table for the whole tree finds a child by (parent, name). Small tables next to the arena
hold what only a few nodes have: the link of a registered name (5.14), the content hash of a
young file (5.8), and the time of a hold (5.9).

A tree that gets smaller gives its memory back (`Tree::give_back`). A node never moves,
because its key is its place. So only the end of the arena can go, as far as no node lives
there: one node that was made late keeps the arena as long as it is, and then only the names
and the table get smaller. Measured with a million nodes (94 bytes each, 91 MiB): when all of
them go, nothing stays; when the node that was made last stays, 71 MiB stay. A tree that goes
and comes back pays for its memory again, as the first time (54 ns for a node, not 12).

What a watcher holds, measured on macOS (the heap of the process, by `vmmap`): 0.1 MiB with
nothing watched; 4.3 MiB for one watched directory with 50,000 files in 5,000 directories (86
bytes for a file); 3.0 MiB for 20,000 files in 2,000 directories that are each watched by
their own path (150 bytes for a file: the backend keeps the real path of each directory
twice). While a `watch` call runs, the heap holds its paths once more, for the thread of the
watcher, and the arena twice for a moment each time it grows. The lists of a flush keep their
memory for the next flush only up to 16 KiB each (`emptied`): a longer one is given back when
the flush ends.

**`complete` is a rule.** A directory node is complete when its path is a watched path that the
filter does not ignore, or when its parent is complete and the filter does not ignore it. Every
check that finds the directory on disk works it out. The tree can be behind the rule for a
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
  Windows; the listing of the directory on Linux, which has no such call), and a listing
  names each entry in that one way.

**Which directory is loose** (`Tree::is_loose`). That is a matter of the disk under the
directory, not of the system: a Linux machine has its own strict disk and, next to it, a
folder of a Mac (measured with OrbStack: `lstat` finds `APP.JS` for `app.js`, and no flag
says so), a FAT or SMB disk, or a directory with the casefold flag. The two mistakes are not
alike. A strict disk that is taken for a loose one costs looks, and the events are right. A
loose disk that is taken for a strict one gives wrong events. Measured with the Linux build
on that folder, before this rule: a write through another spelling gave a `Create` for a
name that the disk does not have, a removal through another spelling gave nothing, and a
rename that changed only the case left the old name behind, with polling too. So:

- With `Options::loose_spelling` (macOS, Windows) every directory is loose, and no disk is
  asked. A case-sensitive APFS volume still takes two Unicode forms of a name for one
  (measured), so what `pathconf` says about the case proves nothing.
- Without it, each directory has an answer of its own (`Dir::spelling`): nothing is known
  at first, and the directory counts as loose; then the looks find out that it is strict, or
  that it is loose.
- The proof (`Engine::tells_spellings_apart`): an entry is tried in the other case of its
  ASCII letters. The same entry is there: loose. Nothing is there, or another inode, and a
  third look finds the entry itself as it was: strict. The third look is there so that a
  change of the disk between the first two does not pass for a proof.
- The entry that is tried must be there, and `lstat` alone does not say that. Measured on
  the shared folder: after a rename away through another spelling, Linux still finds the
  old name with `lstat` (its kernel keeps what it knew about each spelling), and nothing in
  the other case, as on a strict disk. So two things can make the proof. A look at a new
  name with no second spelling in the tree (`Engine::other_spelling`): such a name is there
  because something made it. That is the first new file of a watched directory, and it
  costs two looks and no listing. And a listing: each listing of a directory whose nodes
  the tree knows tries one of the names that it gave (`Engine::ask_for_the_rule`). A look
  that is not sure, cannot tell, and finds nothing known asks for such a listing, once.
  The first listing of a directory tries nothing: watching a tree costs no call more than
  before.
- A proof is taken back. By such a listing, when the other spelling finds the very entry:
  the directory can be another one by now, which the tree cannot tell from the old one (the
  same inode number, no birth time). A listing of a strict directory keeps no names, so the
  node that is tried can be gone with the old directory, which says nothing: then the next
  one is tried. (Found by the fuzz test: the first node was gone, the second one was there
  under both spellings, and the directory stayed strict.) And by a look: in a strict directory a look asks the
  disk for the name when the tree has a second name in another case of its ASCII letters
  (`Tree::has_case_twin`). That is how a loose disk shows to an engine that takes it for a
  strict one (a write through another spelling, a rename of the case). If the disk gives
  another name, the directory is loose, and its names are compared with one listing: the
  looks before took it for a strict one.
- A directory that is replaced by another one (another id) starts again. So does one that
  tracks its entries no more (a partial node): nothing is asked in it, so nothing would take a
  wrong proof back. (Found by the fuzz test: a directory that was proved strict was replaced by
  one with the same id from a loose disk, under another spelling of its name. The tree kept
  it as a partial node, and the proof with it.)

A second name is evidence only while its node is there. So when a look leaves nothing at a
name (the filter ignores it, or it is gone) and the node goes, the marked nodes that can be
the same name are looked at first, as names that no listing gave (`Engine::check`). The order
of two looks in one flush must not decide.

On a strict disk this costs two looks, once for each watched directory, at its first new
file; or one listing and three looks, when the first look that is not sure is at a known name
that is not plain. Each later listing costs two looks. A name with a real twin (`Makefile`
next to `makefile`) costs a question at each of its looks. A directory in which no name has
an ASCII letter cannot be proved: a name of it that is not plain costs a question at each
look. On Linux a question for a name is a listing (1.8 ms for 2,000 entries of the shared
folder). In a loose directory it is asked for a new name and for a name with a twin, not for
the edit of a file that the tree knows.

**A hint is for every name that it can mean** (`Tree::others_meant`). The walk of step 4
follows the child with the very name of the hint, and also

- each registered child that can be the same name. On every system, when the two differ only
  in the case of ASCII letters: where the disk tells the two apart, the look finds what is at
  its own name, and an engine that takes a loose disk for a strict one (a folder that Linux
  shares with macOS) still follows its watched paths;
- in a complete directory, each child that differs only in the case of ASCII letters. In a
  loose directory an entry has one spelling, and the OS can name it in another one when it
  is gone. In a strict one this is one look at a file that did not change, and it is where
  a wrong proof shows.

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
  name comes from a hint or from a caller, and both can spell it as no entry is spelled;
- another node of the directory can be the same name (`Tree::has_other_spelling`): the disk
  has only one of the two. A name that is not plain counts as such a name at every look;
- the look is part of a scan that listed the whole directory, and the listing did not name it.

A look is sure, and asks nothing, for a name that the listing of the running scan gave, and
for an entry that the tree knows and that has no other spelling in the tree: the edit of a
file that the tree knows costs what it costs on any disk.

When the disk spells the name in another way, the directory has no entry of this name.

- A name that is not registered is gone (`Found::OtherSpelling`): at once and with no hold,
  because a second look would find the entry again.
- A registered name is still the path of its caller, but the directory above does not cover
  it: the node says so (`Node::is_unlisted`, read by `Engine::is_inside`). A watched path
  reports what is at it as before. A name that only leads to a watched path has nothing of
  its own: a file that the directory reported there is gone at once, and a directory there
  becomes partial and keeps only what the watched paths below it cover (`uncover`).

In both cases the disk gave the name that the entry has. If the tree has no node for it, one
is made and looked at in the same flush.

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
  node is complete. It also counts if a watched path lies below it: such a directory is never
  filtered, and its node is partial unless the first rule holds too.
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

One thing comes before the table: the directory above a registered name with a file or nothing
at it is the watch target of that name. If the filter ignores that directory, the table is
not used: the name is dropped when the flush ends (5.11).

The identity of a directory is never a reason to skip the check when a hint names the
directory: a directory that was moved away and back keeps its inode.

### 5.5 `appear`, `still_there`, `scan`, `vanish`

`appear(node)`: the kind of the node becomes "directory", with its identity and `complete`; ask
the backend for its watch (`arm`, 5.10); queue a shallow scan.

`still_there(node, deep)`: `arm` the node, and with `deep` also queue a deep scan. `deep` is
true for a `deep` mark, when the directory was held, when it is another directory than before,
and when a link on the way changed (5.14).
Nothing below the node is thrown away: the deep scan compares each file with the fingerprint
that the tree has, so only what differs is reported.

`scan(node, depth)`: `check` every child of the node. A **complete** node also lists the
directory: each entry that the tree has no node for yet gets one, and is checked with the type
from the listing. A **partial** node lists nothing: its children are its registered names. In
a deep scan each check gets a `deep` mark, so every directory below is watched again and
scanned deep too. Nothing is scanned below a held node. If the listing fails because the
directory is gone, its parent checks its name. If the disk does not answer, the node gets a
deep mark for the next flush.

`vanish(node)`: the directory is gone for good, or a file took its place. The kind of the node
becomes "nothing"; `backend.unwatch` if the backend was asked for this node. For each child:
forget it if it is a file (the end of the flush tells its `Delete`), `vanish` if it is a
directory; its mark is dropped.

### 5.6 A partial node becomes complete

When `watch` makes a directory `R` a watched path and its node is there as a partial node,
`register` marks the node of `R`. The check finds the same directory, sees that the rule of 5.1
makes it complete now, sets `complete`, asks the backend with the higher want, and queues a scan.
That scan checks the child directories in the same way, so this goes all the way down.

### 5.7 `watch` and the baseline, `unwatch`

**`watch`** (`engine/watched_paths.rs`). A path that the filter ignores as a file and as a directory
is left out (5.11). For each other new path: walk down from the root; make the missing
nodes; make the last one a watched path, which makes every node above it registered; put a
deep mark on the deepest node on the way that has only directories on disk above it. Then run
one flush right away, without the quiet time, and return when it is done.

The whole batch is registered before the flush, and each node is checked once in a flush. So
a batch costs one `watch` call for a directory and one `lstat` for a name (the test
`the_cost_of_a_batch`), plus one look for the kind of a watched path in each directory
that holds some (5.10).

**Baseline.** What `watch` finds is not reported. But there is no silence for a file that had
to be reported before this `watch`: an older watched path or an older complete node covered it
already. So silence is only for

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
- A directory that stays may want less than it was asked for (`narrow`). A watch that was
  asked for `Contents` is given back when the tree says that names are enough (every
  registered name in it is a directory on disk), and the backend is asked again, for names.
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
   young, the hash is compared one last time and dropped. A link is never opened: the hash of
   a link is the hash of its target. A file above 16 MiB is not hashed, and a file may not be
   readable. Such a young file has nothing to compare:
   - where an OS watch covers it, an equal fingerprint counts as a change at every look while
     it is young, and at the first look that finds it old;
   - where only the poll looks at it, an equal fingerprint says nothing while it is young, and
     the first look that finds it old reports one `Update` for all of them. A later look is
     sure to come there, and a write that leaves the time stamp as it is can only come while
     the file is young.

   **A version in place of the hash** (`Fs::version`, Windows). NTFS keeps a number for each
   file that moves when it is changed (the place of its last record in the change journal).
   Not with every write: while a writer keeps the file open, only its first write moves the
   number, and the close does (measured: three writes through one handle, one number). So a
   second write of the same size inside one step of the clock, by a writer that keeps the
   file open, is not found by a look (section 9). Where an OS watch covers a young file, the engine stores that number and not a
   hash, and the same number at the next look says that nothing was written. The file is not
   opened for reading: on Windows that waits for the virus scanner when the file was just
   written (2 ms, and 14 ms for one in ten, measured; the number takes 0.04 ms). The number
   also moves when the file is renamed, gets other attributes, or is closed by its writer,
   and each of these counts as a change at that look: more `Update` events, never fewer. So
   it is not asked where only the poll looks at a file: there nothing else tells a write from
   a `chmod`, and the hash decides as before. A disk without a journal has no number, and the
   hash is taken.

| The modification time | Young for |
|---|---|
| has a part of a second in it, and an OS watch covers the file | 20 ms |
| is a whole second (the file system counts in seconds) | 2 s |
| any, and only the poll looks at the file | 2 s |

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
  node, with the time of its second look: 100 ms after the hint that asked for the first look,
  so the quiet time before the look does not add to the delay; 100 ms after the look where no
  hint asked for it (a poll, a scan). A first look that comes later than that, as under a
  flood, gets its second look at once, in the next flush. Nothing else changes: the
  fingerprint stays, and under a directory every node stays as it is. A directory that vanishes
  with 50,000 files is one held node. The backend forgets the watch of a held directory at once.
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

### 5.10 Want, watch, poll

**The want rule.** What a directory node asks from the backend:

- `Contents` if the node is complete, or if one of its registered names has a file or nothing
  at it. A file there is written, and where nothing is, a file can come, or a directory with
  everything below it at once. Not for a registered name when the filter ignores the
  directory (5.11).
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
- When `commit` fails, every node in the scan queue with a watch becomes `Failed`.
- If the backend says that it cannot go on, the error is reported once, every node becomes
  `Polled`, and everything is scanned.

**The poll.** The tree keeps a list of the directories that the timer looks at
(`Tree::polled`), so a poll does not walk every node. On the poll timer, for each node on the
list whose directory is on disk:

- `Failed`: ask the backend again (it may have room now),
- `Polled`, or a scan of it had an error: queue a shallow scan,
- else: put a plain mark on each registered child that is a symbolic link (5.14).

The marks and scans go through the normal flush, and the next poll is paced (5.13). With a
native backend the timer is one second and the polled nodes are few: the root, the directories
that the backend answered `NotWatched` for, and what failed. With the poll backend every
answer is `NotWatched`, so every node is polled, and the timer is the poll interval. **Polling
is this timer with the poll backend.**

**Asked again.** `NotWatched` is not for ever. A commit can start an OS watch for a tree that
holds a polled directory (FSEvents: a stream path that went up to a common directory, or a
watched path higher up). Then `commit` returns the keys of these directories, and the engine
calls `watch` again for each of them (`ask_again`). One that is reported now gets a shallow
scan after the next commit (watch, commit, then scan), and no poll looks at it any more.

**Given back.** A directory can have a watch of its own and lie below a watch for a tree: its
path was watched first, and a path above it later. When such a directory is scanned deep, its
own watch is given back after the next commit, and it relies on the watch above (`still_there`,
`give_back_covered`). So it does not matter which of the two paths was watched first. It
happens only with a deep scan: a watch that goes takes with it what it has not delivered yet,
and the scan after it finds what that watch did not tell.

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

**The contract of a filter.** It gives the same answer for the same path and kind. And if it
ignores a directory, it ignores everything below it. Such a filter is called **closed**.

**The kind is always known.** It comes from `lstat`, from a listing, or from rule 2. An entry
that is gone is never asked about: what the tree stored decides. The path has the registered
spelling.

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
2. **A path given to `watch`** (`register`), for both kinds, whatever is on disk. If both are
   ignored, the path can never be reported: it gets no node and costs no watch, no look and
   no poll, and `unwatch` of it does nothing. Every other path becomes a watched path. When it
   exists, it is asked with its kind like every entry. While the answer is "ignored" it counts
   as missing: nothing is listed or watched for it, and it is asked again when its kind
   changes.
3. **The watch target of a registered name** (`is_ignored_target`). A registered name with a
   file or nothing at it is served by the watch of the directory above it: that directory is
   asked for `Contents` because of the name (5.10). It is the directory that holds a watched
   file, or the lowest directory that exists above a missing path. Before the engine asks the
   backend for that, it asks the filter about the directory, as a directory. If the filter
   ignores it:
   - the directory is not asked for `Contents` because of its registered names. It keeps the
     `Names` ask of a directory that leads to a watched path;
   - a registered name in it with a file or nothing at it is **dropped**: the look that finds
     it puts it on a list (`Engine::dropped`), and the end of the flush forgets every watched
     path at the name and below it with the code of `unwatch` (`drop_ignored`). The look asks
     the disk first whether the ignored directory is still the one that the tree has there:
     a look at a name can come before the look that finds its directory gone, and then the
     watch target is a directory further up, which the filter may not ignore. The nodes
     that nothing else needs are freed, and their watches are given back. One thing differs
     from `unwatch`: each file there that the handler was told about, and that no other
     watched path still covers, gets its `Delete` in the same batch, also one whose `Delete`
     was waiting for the delete delay. Such a file is gone from the disk (a name with a
     file below it has a directory at it, and is not dropped), and without the `Delete` the
     handler would keep it for ever. No delete delay applies;
   - nothing wakes a dropped path up. `unwatch` of it does nothing, and `watch` of it
     registers it anew and looks again.

   When a chain of missing directories appears step by step, each new directory is the watch
   target in turn, and it is asked when it appears. The chain stops at an ignored one. A
   directory that only leads to a watched path (asked for `Names`) is not asked: nothing walks
   up to the root.

A directory on the way to a watched path is never filtered as an entry. If the filter ignores
it, its node is partial: it tracks only its registered names.

**Closed or not.** With a closed filter, rule 3 never changes an event: a path below an ignored
directory is ignored for both kinds, so rule 2 leaves it out, and an ignored directory has no
registered name in it. With a filter that only looks at the last name, the result of rule 3
depends on which directories exist when a name is looked at: `/p/node_modules/x.js` is never
reported, and `/p/node_modules/pkg/x.js` is reported only if `pkg` is there at that look. If it
is not, or if `pkg` goes away later, the path is dropped. That is allowed. The stories in
`pathwatch_sim/tests/filter.rs` pin it; the fuzzer uses a closed filter.

### 5.12 Errors from the disk

| Error | Meaning |
|---|---|
| not found, not a directory, invalid name | missing |
| permission denied, link loop | missing for events; reported once; the node is polled until a scan of it has no error |
| anything else | no answer: the tree stays as it is, no `Delete` is told, the mark stays for the next flush; reported once; the node is polled |

A failed `watch` is reported in the same way (`WatchLimit`, `PermissionDenied` or `Io`), but not
when the directory is gone: the scan that follows finds it missing.

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
hold back a flush.

**The debounce.** The disk is checked when no hint came for the quiet time: every hint that
leaves work moves the flush to one quiet time after it. So a save in several steps is one
batch. Hints that keep coming do not hold a batch back for ever: counted from the first hint
after the last flush, the flush comes after 500 ms at the latest, or after four quiet times if
that is longer. `watch` and `unwatch` flush at once. The delete delay is not touched by this:
it counts from the look that found nothing.

**Pacing.** Two kinds of work are asked for by no hint that names a path, and can come again
and again: a poll, and the scans after a loss report. After such work that took `t`, the same
kind of work rests for `9 × t`. So about a tenth of the time goes into it at most.

- Loss reports: all scans that the loss reports of one flush asked for are timed together, and
  the next ones wait for the rest after them. A mark that only a loss report asked for stays on
  its node until then. Every other mark is checked as always, and a hint that names a path
  with a waiting mark takes it out of the wait.
- The poll: the next round starts after the poll interval, or after the rest, whichever is
  longer.

The time is the time of the looks at the disk. What the backend takes to make watches live
(`commit`) is left out: it is paid once, not at every look.

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

`crates/pathwatch/src/backend.rs`. The engine asks a backend to watch directories, and the
backend tells the engine where to check. A backend never says what happened: the engine reads
the disk itself. The file has no comments: this section is the one place for what its items
mean, and for the rules.

**The trait `Backend`.** Every backend writes `watch`, `unwatch` and `translate`. A backend
type has its own `new(sink: Sink<Self::Raw>)`.

| Item | Meaning |
|---|---|
| `Raw` | A raw OS notification. Cheap to make: the backend thread only copies it into the sink. |
| `watch(key, dir, want)` | Report changes in `dir` as hints that carry `key`. The answer is a `Coverage`. |
| `unwatch(key)` | Forget `key`. No hint should carry it after this; a late one costs the engine one check. |
| `commit()` | Make every watch asked for since the last call live. On an error none of them is. It returns the keys that got `NotWatched` and whose directories an OS watch reports now: the engine calls `watch` again for each of them, with the `Want` of before, and does not poll one that is reported (5.10, "Asked again"). Only a backend that starts one OS watch for many directories (FSEvents) has work here, and only such a backend returns a key: its answer for a directory depends on the OS watches of other directories. Every other backend leaves the method out: then it does nothing and returns no key. |
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
   So a watch is live when `commit` returns, and `commit` opens no gap in a watch that is live.
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
  changes that the other machine makes do not come at all. Nothing tells the backend so.
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
- The kernel tells about a rename or a removal of a directory through a queue for vnode
  events (`kqueue`, `EVFILT_VNODE` with `NOTE_RENAME`, `NOTE_DELETE`, `NOTE_REVOKE`), on a
  descriptor of that very directory that was opened for events only (`O_EVTONLY`; it does
  not keep a disk from being unmounted). Measured: the event is there when `rename` returns;
  two renames with no pause give one event; none was missed in 4,000 runs of "rename away,
  make a file below, rename back", of a rename that only changes the case, of a swap of two
  directories in one call, and of a directory that is replaced; the same on APFS, HFS+,
  exFAT and FAT32. Nothing comes on a descriptor of a directory below the renamed one, so
  each directory on the way needs its own. To open and register one takes 10 µs.
  `fcntl(F_GETPATH)` on such a descriptor gives the path of the directory as it is now, in
  0.3 µs, in the short spelling of a path on the data volume (`/Users/...`).
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
  directory, else `NotWatched`.
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
- **The real path** of a directory that is asked for its contents is made of the real path of
  its parent, which is kept until the next commit, and of its own name as the disk spells it
  (one `getattrlist` call). A link, and a name that differs by more than the case of ASCII
  letters, are left to the OS (`realpath`).
- **Streams are only added.** `commit` starts streams for the new paths. Old streams keep
  running, so nothing is ever in a gap. With 64 streams running (`MAX_STREAMS`), with too many
  stream paths, or when a new path lies above a running one, `commit` starts new streams for
  all covers in their place: so no stream path lies below another, and no record comes twice
  for long. If the streams cannot start, `commit` returns the watch limit error and the old
  streams keep running.
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
- **An old stream that stops.** A replaced stream runs on for 500 ms next to the new ones. When
  it is stopped, the backend puts one loss report into the queue (`MustScanSubDirs` for `/`,
  so `Lost`), and the engine reads everything again: a stream that stops takes with it what it
  has not delivered, and the new one may not have told everything from its first moments (see
  the probe). Streams that stop at the same time share one report, and the scans of loss
  reports are paced (5.13). The stop is made by the first `commit` or `translate` after the
  500 ms: the poll of the directories above the covers brings a `commit` every second, so an
  old stream does not wait for the next change. At most 72 old streams run on
  (`MAX_RETIRING_STREAMS`: as many as can run at a time, so the streams of one replacement
  always keep their 500 ms). With more, the oldest are stopped at once, in the same way, so
  that commits that come fast do not use up the 512 streams.
- **Hints.** A record becomes one `Changed` for each key of each cover that holds its path;
  the rest of the path is `rel`. The covers are found by one table lookup for each directory
  above the path that can be a cover. A record under no cover is dropped: a stream path on a
  common directory also reports what lies next to the covers. A directory record without
  `Created`, `Removed` or `Renamed` is dropped.
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
  stopped at the next commit and its guards are given back, with no loss report, since none of
  its records would reach a key.
- **A cover is a text.** A stream stands at the text of its path, and a record finds its
  cover by the text of the cover. Both hold only while every name in them is the one that the
  disk has. Each way for a name to change has one place that finds it, and all of them answer
  in the same way: the cover is reported lost, and the engine asks for it again (which reads
  its real path anew) and scans it deep. A directory above a stream path: the guard. A
  directory inside a stream: the record of its rename. A change between `watch` and the
  start of the stream: the check after the start. A link that leads somewhere else: the
  poll (5.14). The notes that follow are these four.
- **The guard.** Each directory at and above a stream path, from the top of the disk down,
  is kept open for events, once for all stream paths below it (`Guard`). It is opened for
  search only (`O_SEARCH`), so a directory that the user may pass through but not read is
  guarded too, and a disk can still be unmounted (measured on APFS and HFS+). A project with 64
  stream paths has some 70 of them. One thread waits for the queue of the kernel. When a
  guarded directory is renamed or removed, every stream path at or below it is trusted no
  more (it leaves the list of stream paths, and its directories are closed), and every cover
  at or below the directory is reported lost: the engine watches each one again, which
  resolves its real path anew and starts a stream at it, and scans it deep. A notice counts
  only for the registration that made it: one that comes late, for a descriptor number that
  has been given to another directory since, is dropped. The scans that loss reports ask for
  are paced (5.13), so a directory that is renamed again and again costs one scan in a
  while, not one for each rename.
- **The room for the guard.** All watchers of a process together keep at most half of the
  files that the process may open (`getdtablesize`). When a commit would need more, it fails
  with the watch limit error like a stream that cannot start: the new covers are polled, and
  the old streams stay. The same for every other error of the guard, with that error. Only a
  directory that is gone is no error: its stream path is reported lost right after the
  start, and the engine finds out what is there. `/` has nothing above it and needs no
  guard. (It is a cover when a watched path right below it has a file or nothing at it.)
- **The check after the start.** The real path of a cover is asked at `watch`, and its stream
  and its guard start at `commit`. A directory above that was renamed in between leaves the
  stream at a path where nothing is. So after the start, the real path of each stream path
  that is new is asked again, in the way it was asked before (`realpath`, 8 µs; the guard is
  registered by then). One that is another path now, or that has no guard, is reported lost.
  The same question is asked both times on purpose: `F_GETPATH` on the descriptor of the
  guard is cheaper, but it can spell a path in another way than `realpath` does, and a
  difference that is no rename would make the engine watch and scan again without end.

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
  is trusted: then a directory on the old path was renamed and the notice
  of the guard is still on its way, or a link leads somewhere else. Without this, a key that
  asks for more in that moment would take the new path with a scan of its own directory
  only. (A key below a stream path that is trusted no more was reported with it.)
- **Covers below a renamed directory.** A record for a renamed directory inside a stream is
  also a loss report for every cover below the path of the record. The engine finds such a
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

The tests of this backend (`tests/windows.rs`, and `tests/windows_parts.rs` for the parts that
make no Windows call: `place`, `Handles`, `Records`, `translate`) pass on Windows 11 with NTFS.
What was not checked, and what is known to be missing, is in `docs/windows-open-points.md`.

- One thread owns one I/O completion port and every directory handle (`port`). A handle
  reports its directory and everything below it. A request that needs a new handle is
  answered at once, and the handle is opened by the next `commit`, for all the requests of a
  flush together: `place` is asked once, not once for each directory (2,000 directories:
  350 ms the one way, 10 ms the other, measured).
- **Where the handles go** (`place`): each directory that wants `Contents`, with no other such
  directory above it, gets a handle. It serves every key at or below its directory. With more
  than 64 handles, handles go up to a directory that they have in common and become one: the
  deepest common directory first, until at most 64 are left. No handle goes on `C:\`,
  `C:\Users` or `C:\Users\<name>`.
- **Answers.** A key that wants the contents and that a handle serves: `WatchedTree`. A key
  that wants names only and that a handle serves: `Watched` (after `WatchedTree` the engine
  would take back the watches of the directories below, and only a key that wants the
  contents holds a handle). A `Names` request that no handle covers, and a directory that
  could only be watched from one of those three directories: `NotWatched`; when a handle
  comes to stand at or above such a directory later, `commit` returns its key. A key that the
  commit can give no handle (no room and no allowed directory in common, or the open fails)
  gets `Changed` with an empty `rel`, the engine asks again, and that request is answered
  `Failed` with the reason.
- `Handles` keeps every served key. When a key comes or goes, the handles move to where `place`
  puts them: new handles are opened first, then old ones are closed. A key that got another
  handle is not told: the new handle reported before the old one stopped, the last read of
  the old one is still translated, and the read that the close cancels fails
  (`ERROR_OPERATION_ABORTED`), so it is not taken for lost records. If one open fails,
  nothing changes. A `watch` for the very directory of a handle puts a new handle there: the
  old one may be on a directory that was renamed away.
- Filter: `FILE_NAME | DIR_NAME | SIZE | LAST_WRITE`. 64 KiB buffer. The bytes of a
  completed read are read on the watcher thread (`Records`, `translate`). The reads ask for
  the longer records (`ReadDirectoryChangesExW`, Windows 10 1709 and later) where Windows
  and the disk have them: they tell whether an entry is a directory.
- **Hints.** A record goes to the deepest key whose directory holds its path, with ASCII
  letters compared without case, and to each key above it that spells the way in another
  manner: two watched paths that are two spellings of one directory both get it. A key that
  is spelled like the record always gets it: in a directory that tells the case of letters
  apart, a key below in another spelling is another directory. A record that no key holds
  is dropped. `MODIFIED` → `Written`; it is dropped when it names a directory (the longer
  records tell) or the directory of the key itself: Windows sends it whenever an entry in a
  directory changes, and that entry has a record of its own. Every other record → `Changed`. A read with no bytes, and a record with no name →
  `Changed` with an empty `rel` for each key at or below the directory of the handle. Bytes
  that cannot be read → `Lost`. A read that fails → the handle is closed, `Changed` with an
  empty `rel` for each of its keys. A name that looks like a short (8.3) name → `Changed` for
  the directory that holds it.
- Directories are compared by path text, and a handle reports nothing for what is behind a
  junction or a directory link below it. So each name between a handle and a key that it
  would serve is looked at (`leads_through_a_link`), and a key behind a link is answered
  `NotWatched`: the engine polls it.

### 7.4 Poll (`poll.rs`)

`watch` answers `NotWatched`, and nothing else does anything. See 5.10.

## 8. Tests

**The simulator** (`crates/pathwatch_sim`: the fixtures in `src/`, the tests in `tests/`; on
Unix hosts). `SimFs` is a file system in memory with inodes, a clock, a change time for each
entry, and a mode that ignores the case of letters. `SimBackend` plays an OS watch API on it in
three modes: by inode (inotify), by path prefix with real paths (FSEvents), by handle for a tree
(Windows). A stream of the second mode ends when a directory at or above its path is renamed or
removed, and says so, as the guard of the real backend does (7.2). The test decides when each
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
- `cost`: no test, a measure. It prints what the engine costs for big cases (20,000 watched
  files, a directory of 50,000 files, 100,000 hints, a polling round, `unwatch`), in time,
  allocations and bytes. Run it before and after a change of the engine and compare.

**The other tests** (`crates/pathwatch`):

- Private functions (a `mod tests` at the end of the file): `tree/mod.rs`, `path.rs`, `fs/mod.rs`,
  `fs/windows.rs`, `engine/events.rs`, and helpers of the backends.
- The tree alone (`tests/tree.rs`): the arena, the table, and the way from a hint to a mark.
- The engine on a real directory with the poll backend and a clock that the test moves
  (`tests/ahead.rs`): a file whose time is ahead of the clock (5.8). It counts the reads.
- Each backend alone (`tests/inotify.rs`, `fsevents.rs`, `windows.rs`, each on its OS). `windows_parts.rs` runs on Windows only. In `fsevents.rs` a test
  that makes many changes runs alone, and after the others where it can (`Turn`): a flood
  makes FSEvents report losses, and a test next to it was seen to get one too. No test there
  starts when more than 50,000 files lie next to the test program (the probe in 7.2).
- The watcher thread (`tests/watcher.rs`): `watch`, `unwatch`, `close` and `drop` from every
  side, relative paths, a bad path, the queue bound, a backend that cannot start.
- The real disk (`tests/disk`), each test once with the backend of the OS and once with
  polling, both at the same time: `contract` (the rows of section 3), `projects` (what editors
  and big projects do), `promises` (the promises of the README; its first lines say which test
  proves which). These tests wait, they do not compute. Measured on macOS, each test alone: of
  56 s, 34 s are the quiet time of 300 ms after each `expect`, 17 s the wait for the events
  (the debounce, the delete delay, the poll of a link once a second, and about 10 ms of the
  OS), 4 s the changes that the tests make, and 0.2 s the `watch` calls.

How to run them (the README has the commands for the other platforms and the lints):

```sh
cargo test --workspace                # everything for this OS; 250 fuzz seeds for each backend
PATHWATCH_FUZZ_FIRST=100000 PATHWATCH_FUZZ_SEEDS=20000 \
  cargo test --release -p pathwatch_sim --test fuzz big -- --ignored          # the long fuzz
PATHWATCH_FUZZ_SEED=1234 \
  cargo test -p pathwatch_sim --test fuzz replay -- --ignored --nocapture     # one seed again
cargo test --release -p pathwatch_sim --test cost -- --ignored --nocapture    # the cost program
```

## 9. Not in version 1

Left out on purpose: events for directories; rename pairs; a report of what exists at `watch`
time, and changes from before the call; globs; a depth option; following symbolic links inside
a watched directory; an option for the one-second poll; kqueue (FreeBSD
uses polling).

Known holes of the engine:

- Linux: a disk that takes two Unicode forms of a name for one and tells the case of letters
  apart (a case-sensitive volume of a Mac in a Linux machine) is proved strict. And a
  casefold flag that is changed on a watched directory while it is empty is noticed only at
  the next listing of that directory.
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
- Windows: a second write of the same size, inside one step of the clock of the time stamps,
  by a writer that keeps the file open: the version of the disk does not move (5.8).
- A write through another hard link of a watched file. A mount over a watched directory.
- FSEvents and Windows: after `rm -r` of the directories above a watched path, the engine
  learns that an upper directory is gone only at the next poll.
- Where the OS watches are put depends on what was watched and unwatched before: a stream stays
  where it is until the streams are replaced. Nothing is missed.

Known holes of the backends are in 7.1 to 7.3. The Windows backend has only run on Windows 11 with NTFS.
Android was never built.

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
- **Guard.** FSEvents: one open file for each directory above a stream path, so that a rename or
  a removal above the path is heard at once.
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
  is. Its content, or the version number of the disk, is kept to compare at the next look.
- **Fingerprint.** The modification time, the size and the id of a file: what tells one state of
  it from the next without reading it.
- **Held.** A look found nothing where the tree has a file or a directory. The node waits for a
  second look, one delete delay after the hint, and only that look decides.
- **Frozen.** A node below a held node. Nothing is done there before the second look above.
- **Uncover, let go.** A node is no watched path any more and no watched directory covers it:
  what the tree has there is forgotten. A node that is not registered any more is an entry like
  any other, or is freed.
- **Loose, strict.** A directory where two spellings of a name are one name (letter case,
  Unicode form), or two names. A directory counts as loose until a look proves it strict.
- **Plain.** A name of ASCII characters and Han ideographs only, so that its other spellings are
  only the case of its ASCII letters, and the table of the tree finds all of them.
- **Twin.** Another node of the same directory that can be the same name as this one.
- **Unlisted.** A registered node whose entry the directory above lists under another spelling.
  It is the path of its caller, and no entry of that directory.
- **Rest.** After a poll, or after the scans that a loss report asked for, took a time `t`, such
  work waits `9 × t` before it runs again.
- **Settle** (tests). Deliver every notification and run every deadline until nothing is left.
