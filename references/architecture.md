# pathwatch architecture

This document describes how the code meets the rules of `design.md`. It covers the modules, the
data structures and algorithms of the engine, the backends, and the tests, with the measurements
behind each choice. "Rule N" refers to the rules for a backend (`design.md` §5.2), and S1 to S14
refer to the examples of the contract (`design.md` §2).

## 1. Overview

`pathwatch` is built for rolldown: `this.addWatchFile`, module files, and watch mode during
development and builds. That means many individual files, some directories, and paths that do not
exist yet. The model follows chokidar. It keeps a tree in memory, does not trust the OS's account of
what happened, and asks the disk.

```text
            signals                        events
 backend  ─────────►  watcher thread  ────────────►  handler
 (OS API)   queue     engine: tree + check
     ▲                    │
     └─ watch(key, dir) ──┘
```

| Module (`src/`) | Contents |
|---|---|
| `config`, `error`, `event`, `watcher` | The API. `watcher.rs` contains the single thread and its queue, and selects the backend. |
| `engine/`, `tree`, `fs`, `path` | The engine, the tree, the disk as the engine sees it (`Fs`), and path cleaning. No thread, no clock, no OS watch API. |
| `backend/` | The `Backend` trait and its types (`mod.rs`), and one backend per file below it: `inotify`, `fsevents`, `windows`, `poll`. Of the rest of the crate, the backends use only `backend` and the system calls in `fs` that the engine also makes. |

The modules other than the API are `pub` but hidden from the documentation, because the tests need
them. Code that only the tests call is compiled only with the feature `test-hooks`. This covers
`engine/test_hooks.rs`, `Tree::check_links` and `Tree::content_room`, what the inotify, FSEvents and
Windows backends expose about themselves, the names used only by the tests of the Windows backend,
and `Watcher::with_backend`. The tests enable the feature through a dev-dependency of the crate on
itself, so a user's build contains none of this code. The tests of the engine on a simulated disk,
and their fixtures, are in `tests/sim` (§6).

- A **backend** sets up OS watches when the engine requests them, and its thread copies raw OS
  notifications into the queue. On the watcher thread, the backend turns each raw notification
  into **hints**.
- A hint never says what happened. It says where to check: `Changed { dir, rel }`,
  `Written { dir, rel }` or `Lost`. `dir` is the key that the engine gave when it requested the
  watch, so the two sides never need to agree on how a path is spelled.
- The engine collects hints as **marks** until no hint has arrived for the quiet time. Then it
  **flushes**. It reads the disk at the marked places, compares the result with the tree, and
  delivers the events. A hint that arrives twice, or late, costs one `lstat` and changes nothing.
- **Watch, commit, scan.** The engine asks the backend to watch a directory, calls `commit`, and
  only then reads the directory. A change before the watch is found by the scan, and a change after
  the watch produces a hint. No change can fall between the two (`design.md`, principle 2).

**The API layer.** A path is cleaned by `clean` in `watcher.rs` and then by `path::normalize`.
`behavior.md`, promise 8, gives the rules. `path::windows_rules::is_name` rejects a name that
Windows does not accept as written. Such paths used to be watched and never received an event
(measured), because no OS record spells them that way. `Config::into_options` turns the settings
into the options of the engine:

- `loose_spelling` is on for macOS and Windows (`design.md` §4.7).
- There is no delete delay when the watcher polls everything (§2.9).
- Every duration setting is capped at one year, because `Duration::MAX` means "never" and cannot
  be added to a clock reading.

With a native backend, the poll of what the OS does not watch runs once per second. This is not
configurable.

## 2. The engine (`engine/`, `tree/`)

Each term denotes exactly one concept (`design.md` §6). The table names the code that holds it.

| Term | Meaning | In code |
|---|---|---|
| watched path | a path passed to `watch` and not undone by `unwatch` | `Node::is_path` |
| registered name | a watched path, or a name on the way to one | `Node::is_registered` |
| node | one name known to the tree; nothing, a file or a directory is at it | `Node`, `Kind` |
| key | the identifier of a node, also used by the backend | `NodeKey` |
| root | the single fixed node above the top directories (`/`, `C:\`) | `Tree::ROOT` |
| complete, partial | a directory node that tracks every entry, or only its registered names | `Dir::complete` |
| told | the handler believes that a file is at the path | `Node::told` |
| baseline | what a `watch` call found; it becomes told without an event | `Node::baseline` |
| hint | a place where a backend says to check | `Hint::{Changed, Written, Lost}` |
| loss report | a hint stating that notifications were lost, either everywhere (`Lost`) or at and below one directory (`Changed` with an empty `rel`) | `Mark::lost`, `Tree::lost` |
| mark | a request to check the node in the next flush | `Mark { deep, written, lost }` |
| check | the comparison of one node with the disk | `Engine::check`, `apply` |
| scan | a check of every node of one directory; a deep scan also checks everything below, and a names scan checks only the nodes whose names are not both in the tree and in a listing | `Engine::scan`, `Depth` |
| flush | checking the marks, then rounds of commit and scan, then delivering the events | `Engine::flush`, `check_marks`, `round`, `finish` |
| touched | a node changed by a flush; its event is read at the end of the flush | `Engine::touched` |
| want | what the engine asks a backend to report about a directory | `Want::{Names, Contents}` |
| coverage | the backend's answer | `Coverage::{Watched, WatchedTree, NotWatched, Failed}` |
| polled | no OS watch covers the directory, so the timer scans it | `Watch::Polled` |
| held | a look found nothing at the node; what the tree has there, and below, is kept until the second look | `Node::is_held`, `Tree::hold` |
| delete delay | the time from the hint to the second look; 10 ms, and none under polling | `Options::delete_delay` |
| young | a file whose modification time is too close to the check to be trusted | `Content` |
| quiet time | the debounce, that is, how long no hint may arrive before the disk is checked | `Options::debounce` |
| rest | the pause after work that no hint requested by name, such as a poll or the scans after loss reports | `REST`, `rest_until` |
| cover | FSEvents only: a directory that the engine watches and that a stream reports | `Covers` in `backend/fsevents.rs` |
| stream path | FSEvents only: a directory for which a stream was started; the stream reports the whole tree below it | `FsEventsBackend::paths` in `backend/fsevents.rs` |

`tree/` holds the data and `engine/` the logic. Each is split into one file per responsibility.

### 2.1 The tree

What is at a name (`design.md` §4.1) is one field of its node (`Kind`). A path that changes its
kind changes that field.

The tree has a node for each of the following:

- The **root**. It has no path, is never listed, and is never given to the backend. It always
  exists and is polled, so that every top directory has a parent that checks it.
- Every **registered name**. Such a node remains while nothing is on disk at its path.
- Every file and every directory found at or below a watched directory that the filter does not
  ignore.
- A name that waits for a check, because it has a mark.
- A name where a file has disappeared, until the end of the flush has told the handler.

A node that is none of these is freed (`Tree::prune`).

**Storage.** Only `tree/` knows how nodes are stored. All nodes live in one arena. A key consists of
an index and a generation, so an old key never names a new node. The arena and the names are
stored in blocks of 64 KiB, and a full block never moves, so they grow without copying what is
already there. When they grew as single blocks, they were briefly held twice at each growth, and
macOS keeps the freed pages. The heap of a watcher of 50,000 files then grew to 8.0 MiB for 4.3 MiB
in use, and its footprint grew with it. With blocks, the heap is 4.8 MiB, and 14 MiB is allocated
in total instead of 39 MiB. Each node has the list of its children, and one table for the whole
tree finds a child by its parent and name. Small side tables next to the arena hold what only a few
nodes have: the link of a registered name (§2.14), the content hash of a young file (§2.8), the
time of a hold (§2.9), the first look at a file whose time is ahead of the clock (§2.8), and the
registered names that are not plain, grouped by directory (§2.2).

A tree that shrinks returns its memory (`Tree::give_back`). A node never moves, because its key is
its position. Only the end of the arena can therefore be released, and only as far as no node
lives there. One node created late keeps the arena at its length, and then only the names and the
table shrink. In a measurement with one million nodes (94 bytes each, 91 MiB in total), nothing
remains when all of them go, and 71 MiB remain when the last node created remains. A tree that
disappears and comes back pays for its memory again, as it did the first time: 54 ns per node
instead of 12 ns.

Memory held by a watcher, measured on macOS as the heap of the process (`vmmap`):

| What is watched | Heap | Per file |
|---|---|---|
| nothing | 0.1 MiB | |
| one directory with 50,000 files in 5,000 directories | 4.3 MiB | 86 bytes |
| 20,000 files in 2,000 directories, each watched by its own path | 3.0 MiB | 150 bytes, because the backend keeps the real path of each directory twice |

While a `watch` call runs, the heap holds its paths a second time, for the watcher thread. The
lists of a flush keep their memory for the next flush only up to 16 KiB each (`emptied`). A longer
list is released when the flush ends. The tables and queues for what only a few nodes have at a
time, the young files and the holds, release their room when most of it is free
(`give_room_back`, `room_to_keep`). Previously, those of the young files did not. A burst of 10,000
new files kept room for 14,000 entries in their table and 16,000 in their queue for the lifetime of
the watcher, about 0.7 MB (`a_burst_of_young_files_gives_its_room_back`). The ordinary work of a
watcher that runs for days allocates no memory. A `watch` of paths that are already watched (in
their clean form), an `unwatch` of a path, and a poll allocate nothing, and a flush allocates only
for the paths of its events. `tests/sim/allocations.rs` enforces these budgets.

**`complete` is a rule** (`design.md` §4.1). Every check that finds the directory on disk evaluates
the rule. The tree may lag behind the rule for a while. A directory that becomes a watched path is
complete in the tree only after its next look. Where it matters, the rule itself is evaluated
(`Engine::tracks_every_entry`), namely in `unwatch` and in the check of an unregistered name in a
partial directory. The evaluation walks up only to the watched path and asks the filter nothing.
Every node with a file or a directory keeps the answer "not ignored" for that kind (invariant 8),
and so does every name on the way to a watched path (`design.md` §4.8).

**Invariants.** `Engine::check_invariants` and `Tree::check_links` verify them, and the simulator
runs these checks after every engine call.

1. Every node except the root has a parent that lists it, and the table finds it.
2. Only a node of kind "directory" has a file or a directory below it.
3. The told set is exactly the set of nodes of kind "file": the files on disk, and the files whose
   `Delete` is pending at or below a held node. `changed`, `baseline` and `added` are set only
   within one call.
4. A held node has a file or a directory, and something ensures its second look. This is either
   its entry in the queue, or a mark that waits for the disk to answer.
5. A node with nothing at its path is kept only while it is registered or has a mark. In a partial
   node, a file is known only if it is a watched path, and a directory only if a watched path lies
   below it. The single exception is a partial node that the rule makes complete and whose look is
   still pending.
6. A directory that was asked for names only holds no registered name with a file or nothing at it
   (§2.10), because no watch would report when something appears there. The exception is a name
   whose first check waits for the disk to answer. A mark then waits on the node or above it.
7. A directory that relies on a watch above (`FromAbove`) has one there. The exception is a look
   that waits for the disk to answer. A mark then waits on the node or above it.
8. Every node except the root that has a file or a directory keeps the filter's answer for that
   kind, and the answer is "not ignored". An ignored entry keeps no node, and an ignored watched
   path counts as missing (`design.md` §4.8). This relies on the filter's contract, which is the
   same answer for the same path and kind.

Invariants 6 and 7 say nothing about what lies below a held node, because nothing is known there.

### 2.2 From hint to mark

`Tree::mark_below` processes `Changed { dir, rel }` and `Written { dir, rel }` as follows:

1. Drop the hint if `dir` names no node.
2. If no directory is on disk at the node, go up to the nearest node above it that is a directory
   on disk, put a deep mark on its child that leads to `dir`, and stop.
3. An empty `rel` denotes the directory itself. Put a deep mark on its node and stop. This is a
   loss report for that place (the watch is gone, the directory was moved, or notifications were
   lost), so the mark records `lost` until a hint that names the path joins it. The directory also
   records that writes in it may have been missed (`Dir::away`, §2.5). This is needed because,
   under inotify, the parent reports a removed directory together with the end of its watch, and a
   directory created again at once can have the old identity.
4. Walk down from the node through the components of `rel`. At node `X` with the next component
   `c`:
   - If `X` is partial and has no registered child `c`, the hint is dropped, unless "another
     spelling" (below) applies. A flood from a place that no watched path can see therefore ends
     here, after one lookup per component.
   - If `c` is not the last component and the child `c` is a directory on disk, descend into it.
   - Otherwise, mark the child `c` and stop. If `X` has no such child yet, a node is created for
     it. The mark is `written` when the hint is `Written` and `c` is the last component. In every
     other case it is `deep`. A hint from deep inside a place that the tree does not know (a tree
     that was moved in, an ignored directory) therefore ends at the highest unknown name. A
     `Changed` hint for the exact name of a young file that was not read (`Content::Pending`,
     §2.8) is also treated as `written`. Every write produces a `Written`, so such a record is a
     rename or a removal, and the content may have changed under the other name. An example is a
     watched file that is renamed away, written, and renamed back under a Windows handle. Under
     inotify this also applies to a `chmod` of such a file, which costs one `Update`.
   - At each `X`, the other children that `c` can denote (below) are marked too, or descended into
     when `c` is not the last component. When every disk is loose (`Options::loose_spelling`), a
     child that differs from `c` only in the case of ASCII letters is the entry that was written.
     Its mark is `written` like the hint, so a rewrite that leaves the fingerprint unchanged is an
     `Update` under every watched spelling, also for a young file that was not read (§2.8). inotify
     also sends `Written` for a file that was created or moved in (`Hint::Written` with `new`). For
     such a hint the twin is the same file after a rename that changes only the case. It is
     therefore marked deep, like every other child that is only a candidate, and its look decides.

Two marks for one node merge into one. `Lost` sets a flag, and the next flush that the pacing
allows (§2.13) scans everything. A hint that left work postpones the flush to one quiet time later
(§2.13). A dropped hint postpones nothing.

**Another spelling.** The rules are those of `design.md` §4.7. The default disks of macOS and
Windows are loose. They find `App.js` when the entry is `app.js`, and likewise for two Unicode forms
of a name.

The alternatives to the rule of `design.md` were rejected:

- Events under the disk's name, as in notify, would force every caller to convert its own paths to
  the disk's names before it could match an event. rolldown matches by bytes and keeps the
  spelling of the import, so it would never rebuild.
- A path that counts as missing unless it is spelled as on the disk, as in Watchman, gets no events
  and no error for a drive letter or a command line in another case.

Whether an import may be spelled differently from the disk is a question for the resolver, not for
the watcher.

Three facts underlie the mechanism. The first two were measured with FSEvents on APFS.

- The OS names an entry that exists as the disk spells it, whatever spelling the writer used.
- The OS names an entry that is gone (removed, or renamed away) as the caller spelled it.
- `lstat` finds an entry under every spelling of its name. The disk can report how it spells the
  name of one entry. `Fs::name_on_disk` asks this with `getattrlist` on macOS, in 2 µs, and with
  `FindFirstFileW` on Windows. A listing names each entry in that one spelling.

**Which disk is loose.** `Options::loose_spelling` decides this for the whole watcher. It is on for
macOS and Windows, where every disk treats two spellings of a name as one. There every directory is
loose, and no disk is asked whether it is. A case-sensitive APFS volume still treats two Unicode
forms of a name as one (measured), so what `pathconf` reports about case proves nothing. The option
is off everywhere else. There every directory is strict, and no look asks how a name is spelled. A
Linux machine can have a loose disk alongside its own, such as a FAT or SMB disk or a directory with
the casefold attribute, and it shows no sign of it. On a folder of a Mac, measured with OrbStack,
`lstat` finds `APP.JS` for `app.js`, and no flag indicates this. §5 lists the wrong events that this
gives below a watched directory. A watched path in another case of its ASCII letters still receives
its events, because a hint for one name marks the other (below). A rule that determined the kind of
each directory from its looks was built and removed again. It tried an entry in the other case of
its letters, made a third look to guard against a change in between, and withdrew its proof when a
listing contradicted it. It took 250 lines for that one case and had gaps of its own: a disk that
gives each spelling a number of its own, such as vboxsf and sshfs, and a disk that folds Unicode
forms but not case.

A second name is evidence only while its node exists. When a look leaves nothing at a name (the
filter ignores it, or it is gone) and the node is removed, its marked twins are therefore looked at
first, as names that no listing produced (`Engine::check`). The twins are the names that differ from
it only in the case of ASCII letters. This keeps the order of two looks within one flush from
affecting the result.

In a loose directory the question is asked for a new name and for a name with a twin, not for an
edit of a file that the tree already knows. A name with a real twin (`Makefile` next to
`makefile`), and a name that is not plain, cost a question at every look.

**A hint applies to every name it can denote** (`Tree::others_meant`; `design.md` §4.7). The walk
of step 4 follows the child with the exact name in the hint, and also these children:

- Every registered child that can be the same name. On every system this applies when the two
  differ only in the case of ASCII letters. Where the disk distinguishes them, the look finds what
  is at its own name. An engine that treats a loose disk as strict, such as a folder that Linux
  shares with macOS, still follows its watched paths.
- In a complete directory, every child that differs only in the case of ASCII letters. In a loose
  directory an entry has one spelling, and the OS may name it in another one when it is gone. In a
  strict directory this costs one look at a file that did not change.

The walk continues below each of them when the hint has more components, and the marks at the end
are deep. If no child can be the name, the hint costs no look at the disk.

Two names can be the same name when they are equal ignoring the case of ASCII letters, or when
neither of them is plain. A **plain** name (`is_plain`) consists only of ASCII characters and Han
ideographs. An ideograph has no case and no second form, so a plain name has other spellings only
in its ASCII letters. How a disk folds any other letter (an accented letter, Greek, kana, Hangul)
is unknown here, so two names that are not plain can always be the same name.

A plain name is found through the table. Its hash ignores the case of ASCII letters
(`child_hash`), so the children that differ only in that respect share one hash. The same holds
for any name where every directory distinguishes two spellings, since only the case of ASCII
letters is followed there. Previously, a name that was not plain found nothing there. On a loose
disk shared into Linux, a watched `Café.js` missed the writes to `café.js`. Such a hint costs one
extra lookup, however many children the directory has: 18 ns among 20,000 registered children,
where comparing with each child took 53 µs. Only letters are folded. A name that differs in the
same bit of another byte (`@` and `` ` ``, or a byte of UTF-8) has a different hash, because names
that share a hash cost the square of their number to list. Inserting 65,536 such names took 270 ms,
against 4 ms for names with different hashes. The case variants of one name share a hash on
purpose, since only a disk that distinguishes them can hold many.

In a loose directory two more rules apply. The registered names that are not plain are kept on a
list per directory (`Tree::not_plain`), and a hint with such a name marks all of them, at one look
each. The other children of a complete directory are on no list, and there can be many. So when a
hint carries a name that is not plain and the tree does not have it as an entry (`Node::is_entry`),
the flush compares the names of the directory with one listing (`Depth::Names`). A node that the
listing names is not looked at, and a node that it does not name is. An edit of a file that the
tree knows costs no listing.

**A look asks for the spelling when it is unsure** (`Engine::spelling_is_unsure`). In a partial
directory a look asks nothing, because a registered name is whatever `lstat` finds. A name with
only one possible spelling, which is a plain name with no ASCII letter (`Tree::has_spellings`), is
never asked about. In a complete loose directory, a look that found an entry asks the disk for the
name of that entry in these cases:

- The node was not an entry of the directory at the last look (`Node::is_entry`). The tree had
  nothing there, the node is held, or the directory had the entry under another name. Such a name
  comes from a hint or from a caller, and both may spell it differently from every entry. The same
  applies to a name that the running `watch` registered (`Added::Registered`), even when the tree
  knew an entry at it, because the caller spelled it and the entry may have another spelling by
  now.
- Another node of the directory can be the same name (`Tree::has_other_spelling`), and the disk
  has only one of the two. A name that is not plain counts as such a name at every look.
- The look is part of a scan that listed the whole directory, and the listing did not name it.

A look is sure, and asks nothing, for a name that the listing of the running scan produced, and for
an entry that the tree knows and that has no other spelling in the tree. An edit of a known file
therefore costs what it costs on any disk. A symbolic link is never asked about, because the disk
cannot report how it spells a link: the real path of a link is the path of its target. The listing
is trusted for it.

When the disk spells the name differently, the directory has no entry of this name (`design.md`
§4.7). A name that is not registered is gone (`Found::OtherSpelling`). A registered name is
unlisted (`Node::is_unlisted`, read by `Engine::covered_by_parent`). A name that only leads to a
watched path loses what the directory reported there. A file there is gone at once, and a directory
becomes partial and keeps only what the watched paths below it cover (`uncover`). The same applies
when a partial directory is found under another name for the first time, because its entries were
covered through this name and no longer are.

In both cases the disk returned the name that the entry has. That name is looked at in the same
flush, with what the hint said about the entry (`written`), whether or not the tree has a node for
it, because the hint was for the entry under whatever spelling. This happens only once. A disk
whose answers contradict each other must not keep the flush busy, so a second step is marked for
the next flush.

The flag is an answer of the disk that the tree stores, so it can be stale. It is asked again at
every look at a node that is not a sure entry, and the tests compare it with the disk once the tree
has caught up (`Engine::check_spellings`). `unwatch` reads it. A path that the directory above does
not list is forgotten silently, like a path with no watched directory above it. If the flag was
stale, one `Create` follows when the look finds the entry.

**Cost.** A new file in a watched directory costs one `lstat` and one question for its name,
however many entries the directory has. Previously, each flush with a new name listed the
directory. With 20,000 files in it, 100 new files cost 1,617 ms of CPU time, against 18 ms for
notify. A file that goes through another spelling of its name costs one table lookup and one
`lstat`. Previously, such a change was missed: `rm App.js` for the file `app.js` gave no `Delete`.
A new name that is not plain costs one listing of the names of its directory per flush.

§5 lists what this does not cover.

### 2.3 Flush

```text
if notifications were lost and the pacing allows it: queue a deep scan of the root   # §2.13
mark_moved                                           # §2.10
for each marked node, from the top down:
    if a node above it is held: drop the mark
    if only a loss report requested the mark and the pacing does not allow it: keep the mark
    else: check(node, mark)
while a scan is queued:                              # one round
    backend.commit()                                 # on an error: §2.10
    for each node queued when the round started, if its directory is on disk:
        if a scan of this round queued it again: leave it for the next round
        else: scan(node, depth)
finish: read the event of each touched node (§2.9), deliver the batch
```

`check` and `scan` never descend into a directory. They ask the backend for its watch and queue a
scan (§2.5), which runs in the next round, after the commit. The engine therefore never reads a
directory before the watch on it is live, and every backend goes through the same code path. When
the disk gives no answer for a node, its mark is kept for the next flush.

### 2.4 `check(node, mark)`

The parent of `node` is a directory on disk, and `path` is the path of `node`.

**Reading the disk** (`read_entry`):

- An unregistered name in a partial directory is missing, without a call to the disk.
- If a listing provided the type of the entry and the name is not registered, the filter is asked
  first. An ignored entry is missing and costs no `lstat`.
- `lstat(path)` is called, unless the listing of the running scan provided everything (see
  below). For a registered name that is a symbolic link, `stat(path)` is also called, which follows
  the link (§2.14). An unregistered symbolic link is a file. Errors are handled as described in
  §2.12.

**A complete listing** (`fs::Entry::File`). On APFS one call (`getattrlistbulk`) returns the
entries of a directory together with what `lstat` would report for each. A scan takes every regular
file from it, so there is no `lstat` per file, neither at the first scan nor at later ones such as a
deep scan after a loss or a poll. Only what a look at a file reads is taken: its modification time,
its size and its inode, which form its fingerprint (§2.8). A scan therefore holds 40 bytes per entry
of the directory being scanned, rather than the 80 bytes of everything that `lstat` reports. A
registered name, and a name whose spelling is uncertain, are still looked at with `lstat`. Measured
on a tree of 5,000 directories with 50,000 files and one directory watched, the watcher is ready in
122 ms, and in 153 ms with an `lstat` per file. What remains is one `open` per directory (8 µs here,
a third of the time), the listing itself, and one `lstat` per directory.

What the listing reports is as old as the listing, and the look at a file can come later. A write
after the listing is reported by the OS watch, which was live before the scan (§2.3), just as a
write after an `lstat` is. A file that has disappeared since the listing is stored, and then found
gone by the look that its record requests.

Only what was measured to equal `lstat` is taken:

- APFS. The listing was identical to `lstat` for 160,000 entries of a system in use, and for a file
  that is open and being written, truncated, extended, hard-linked twice, compressed, or renamed.
  Other disks are listed as before. On HFS+ the listing keeps the old times of a file written
  through another hard link, on exFAT an empty file gets another inode number at each call, and
  FAT32 provides no change time.
- Regular files only. For a directory on which a disk is mounted, the listing reports what is under
  the mount, and the engine follows a symbolic link itself. A file whose content is only in the
  cloud (the `dataless` flag) is also left to `lstat`, because none with a size was available to
  measure.
- The disk is first asked by path (`statfs`), and an entry counts only if it is on that exact
  device, because the directory may be a different one by the time it is opened.

**What counts.** "Inside" means that the node is a watched path or that its parent is complete.

- A **file** counts if it is inside and the filter does not ignore `(path, File)`.
- A **directory** counts if it is inside and the filter does not ignore `(path, Dir)`. Its node is
  then complete. It also counts if a watched path lies below it, because `watch` asked about such a
  directory and found it not ignored (`design.md` §4.8). Its node is then partial, unless the first
  rule also holds.
- What does not count is missing.

`check` produces no event. It changes the node and adds it to the list `touched`, which the end of
the flush reads (§2.9). "Tell" in the table means exactly this.

| Disk | Tree has | Action |
|---|---|---|
| no answer | anything | Keep the mark for the next flush. |
| missing | file, directory | `missing(node)` holds the node. If its second look also finds nothing, it is gone (§2.9). |
| missing | nothing | Nothing. |
| file | nothing | Store it, and tell `Create`, or nothing if it is part of the baseline (§2.7). |
| file | file | Tell `Update` if the fingerprint differs, the mark is `written`, or the rule for young files requires it (§2.8). Store the new fingerprint. A hold ends. |
| file | directory | `vanish(node)`, then as for "file, nothing". |
| directory | file | Forget the file, which tells `Delete`, then `appear(node)`. |
| directory | nothing | `appear(node)`. |
| directory | directory | Store the identity, make the node complete if the rule requires it (§2.6), end a hold, and call `still_there(node)`. |

A change of kind does not wait for the delete delay (`design.md` §4.3).

Afterwards the node is freed if nothing needs it any more. If the node is a registered name with no
directory at it now, and its directory was asked for names only, the directory is asked for
`Contents` (§2.10).

### 2.5 `appear`, `still_there`, `scan`, `vanish`

**`appear(node)`.** The kind of the node becomes "directory", with its identity and `complete`. The
backend is asked for its watch (`arm`, §2.10), and a shallow scan is queued. With `two_step_links`
(Windows, not under polling), the node is also marked for one more look in the next flush, as is a
directory that differs from the previous one. A junction or a directory symbolic link is a plain
directory until its second step, which produces no record, so a look between the steps sees a
directory. Only a recently created directory is marked: one whose modification time, which its
creation sets, lies within the coarse youth of §2.8 (2 s) of the time of the flush, or ahead of it.
An older directory is past its second step, and the directories of a baseline are old. Before this
rule, every one of them was looked at once more in the first flush after `watch`. For 50,000 files
in 5,000 directories that was 5,011 `lstat` calls and 47 ms, which made the first edit after
`ready` take 58 ms instead of 12 ms (measured 2026-10-20). The listing provides the time where it
provides the identity (NTFS, APFS), so the rule costs no call. The extra look is spent by whichever
flush comes next, whether a poll or the hints of other paths. When that flush also falls between
the steps, nothing looks again. This was measured with the steps 300 ms apart, which no tool does.
The story keeps them 50 ms apart, clear of the poll.

**`still_there(node, deep, away)`.** `arm` the node. With `deep`, also queue a deep scan.
Otherwise, queue a shallow scan when a listing named an entry that was gone (`relist`, below).
`deep` is true for a `deep` mark, when the directory was held, when it is a different directory
than before, and when a symbolic link on the way changed (§2.14). Nothing below the node is
discarded. The deep scan compares each file with the fingerprint in the tree, so only what differs
is reported.

`away` (`Dir::away`) indicates that writes in the directory may have been missed. It is set in
these cases:

- The directory was held.
- It is a different directory.
- A symbolic link on the way changed.
- Its watch lost records (a `lost` mark).
- The tree watch it had was released. The records that this watch had not yet delivered are lost
  with it.
- A record named it (a hint with its time, not a `watch` call) while only a watch above covers it.
  Its name changed, so the records of its files arrived under the other name. A directory with a
  watch of its own does not need this, because that watch follows the directory, as inotify does.

A young file that was not read (`Content::Pending`, §2.8) counts as written in such a scan, even
when nothing was written to it (`behavior.md`, "Known limitations"). This does not apply to a file
whose doubt has ended. Once every record from the time it was young has arrived (`Hint::Delivered`,
§2.8), no write can hide behind its fingerprint. A rule once dropped the doubt of a file that was
already old when the record naming the directory arrived (`forget_pending_old_at`). It took the
arrival time of a record for the time of its change, and a late record made a file forget a write
made while it was away (Handle seed 2074719922). The rule was removed.

**`scan(node, depth)`.** `check` every child of the node. A **complete** node also lists the
directory. Each entry for which the tree has no node yet gets one, and is checked with the type
from the listing. A listing follows a symbolic link at the path, so it counts only if the same
directory is still at the path when the listing finishes, which costs one look. Otherwise a link
that replaced the directory since the look would be read as the directory, and the files behind it
reported. If the directory is not the same, the node is marked deep, and the next flush looks
again. A **partial** node lists nothing, because its children are its registered names. In a deep
scan each check gets a `deep` mark, so every directory below is watched again and also scanned
deep. Nothing is scanned below a held node. If the listing fails because the directory is gone, its
parent checks its name. When that look finds the same directory again, the node gets a deep mark
for the next flush. If the disk gives no answer, the node also gets a deep mark for the next flush.
An entry that the listing named and that is missing when it is looked at makes the directory list
again in the next flush (`relist`). The entry may have been renamed away and back while the
directory was read, and the Windows backend does not report that.

**`vanish(node)`.** The directory is gone for good, or a file has replaced it. The kind of the node
becomes "nothing", and `backend.unwatch` is called if the backend was asked about this node. Each
child that is a file is forgotten, and the end of the flush tells its `Delete`. Each child that is a
directory is vanished in turn. The marks of the children are dropped. The walk keeps a list of the
directories still to process instead of recursing, so a tree deeper than the stack is handled like
any other.

### 2.6 A partial node becomes complete

When `watch` makes a directory `R` a watched path and its node exists as a partial node, `register`
marks the node of `R`. The check finds the same directory, sees that the rule of §2.1 now makes it
complete, sets `complete`, asks the backend with the higher want, and queues a scan. That scan
checks the child directories in the same way, so the change propagates all the way down.

### 2.7 `watch` and the baseline, `unwatch`

The engine takes the paths of `watch` and `unwatch` as given, because the watcher has already
cleaned them (§1). The rules are those of `design.md` §4.4.

**`watch`** (`engine/watched_paths.rs`). A path below a directory that the filter ignores, or a path
that it ignores both as a file and as a directory, is skipped (`design.md` §4.8). For each other new
path, the engine walks down from the root, creates the missing nodes, and makes the last one a
watched path, which makes every node above it registered. It puts a deep mark on the deepest node on
the way that has only directories on disk above it, each listed by the directory above it. It does
not do so when a scan that checks every name of that node's directory is already queued, since a
scan of names alone skips what the listing names. The walk ends at a name that the disk spelled
differently at its last look. The tree can lag behind the disk when `watch` is called, with a
notification still on its way, and that answer decides the baseline of what lies below. The engine
then runs one flush at once, without the quiet time, and returns when it is done. A `Written` hint
that was on its way reports an `Update` for a file that the baseline has just seen (§2.8, guard 1).
NTFS produces such a record for the content of a new file, so a file written under a handle a
moment before `watch` can be reported twice, as `Create` and `Update` (the test
`a_scan_that_waited_runs_inside_a_watch_of_a_file_below`, Handle).

The whole batch is registered before the flush, and each node is checked once per flush. A batch
therefore costs one `watch` call per directory and one `lstat` per name (the test
`the_cost_of_a_batch`), plus one look for the kind of a watched path in each directory that holds
watched paths (§2.10).

**Baseline.** While a `watch` runs, each node records what the call made of it (`Node::added`), and
a directory records that it became complete in this call (`Node::baseline`). At the moment of the
look, `check` asks whether the node should have been reported before the call (`watched_before`).
That is the case if it was a watched path before, or if the directory above was complete before.
Where the directory above is still partial in the tree, the rule of §2.1 is evaluated for the
watched paths that existed before the call, since the look of an older `watch` can still be waiting
for the disk to answer. If neither holds, what the look finds is the baseline, and a file becomes
`told` without an event. The end of the call clears both fields, so a look that has to be retried
after the call reports what it finds.

**`unwatch`** (in the same file) is the inverse of registration:

- The nodes that nothing needs any more are freed, and the backend forgets each directory among
  them. A directory that remains, because a watched path lies below it, is partial.
- A directory that remains may want less than it was asked for (`narrow`). This is decided for the
  nearest one above what was removed, once at the end of the call, however many paths left it. A
  watch requested for `Contents` is released when the tree says that names suffice, that is, when
  every registered name in it is a directory on disk. The backend is then asked again, for names.
  After a `WatchedTree` watch, everything that remains below is scanned deep, so each node that
  relied on the watch requests one of its own. After a watch for a single directory (inotify), the
  scan that every request queues looks at its registered names. Previously, such a watch was left
  in place, and every write in a directory that no longer held a watched file still woke the
  watcher. 20,000 writes in half a second cost 6 ms of CPU time then, against 0.1 ms now. No event
  was wrong either way. While the tree has nothing at a registered name, the wide watch stays.
  When the `unwatch` of a tree makes many levels release such a watch, only the top one queues the
  deep scan. That scan reaches the others, and everything below is scanned once.
- A symbolic link that was registered becomes a file entry again, so it gets one check.

Then one flush runs, as for `watch`.

### 2.8 Fingerprint, identity, and young files

`design.md` §4.6 defines the fingerprint, the identity of a directory, and the two guards against
equal fingerprints. The file ID of a fingerprint is the inode on Unix and the file index on
Windows. The identity of a directory is its device, inode and birth time on Unix (a birth time of
zero where there is none), and its volume serial number, file index and creation time on Windows.
The creation time alone is no identity there, because a new entry inherits the creation time of the
entry that was removed under its name a moment earlier. A directory that was moved away and back
keeps its inode, so the identity never lets a check be skipped.

The guards are implemented as follows:

1. A backend that knows of a write sends `Written`, and the engine reports `Update` without
   consulting the fingerprint. Only a backend whose notifications correspond one to one to events
   does this (inotify, Windows). FSEvents never sends `Written`.
2. **Young files.** A file is young when its modification time is close to the time of the flush,
   or in the future. When a young regular file is stored, the engine also stores a hash of its
   content. At the next check with an equal fingerprint, the hash decides. When the file is no
   longer young, the hash is compared one last time and dropped. A symbolic link entry is never
   opened, and its hash is the hash of the text of its target. A watched path that is a symbolic
   link is followed for its fingerprint, and therefore for its content, so the file is read through
   its real path. Every other read opens the entry in a way that does not follow a symbolic link
   that replaced the file since the look, and does not wait on a FIFO or a device (`O_NOFOLLOW` and
   `O_NONBLOCK`). On Windows a reparse point that stands for another name is refused. A file larger
   than 16 MiB is not hashed, and the read stops at the limit when the file has grown since the
   look. Such a file, and a file that cannot be read, has nothing to compare:
   - Where an OS watch covers it, an equal fingerprint counts as a change at every look while the
     file is young, and at the first look that finds it old.
   - Where only the poll looks at it, an equal fingerprint says nothing while the file is young,
     and the first look that finds it old reports one `Update` for all of them. A later look is
     certain to come there, and a write that leaves the timestamp unchanged can only happen while
     the file is young.

   **Where every write produces a hint** (`Options::written_hints`), a young file is not read at
   all. This applies to inotify and Windows, except for a file that only the poll looks at. Guard 1
   reports a write that leaves the fingerprint unchanged, so the hash would only repeat the hint.
   The node keeps `Content::Pending` while the file is young. A look counts a `Pending` file as
   written when the hint may have been missed since its last look (`hint_may_be_lost`), which is
   the case in these situations:
   - A loss report requested the look, or one is waiting, because the report may have absorbed the
     hint.
   - The look belongs to the scan of a directory that was away (§2.5, `Dir::away`), or such a scan
     is queued above, because the file's own look can come first in the flush.
   - The file was held, because it was away itself, or because its hint went into the look that
     found it missing.

   A look that gets no answer (`Found::NoAnswer`), and a listing that cannot be finished, pass what
   they knew about lost hints to the retried look (`Engine::retry`). Previously, a listing left a
   plain deep mark, and its scan took a rewritten file for untouched (Inode seed 103232).

   **When the doubt ends.** A look that finds the file old and reports it ends the doubt, because
   the handler reads the file after the event and no later write can keep the fingerprint. A look
   that finds it old and reports nothing does not end the doubt. The hint of a write made while the
   file was young, or the loss report that absorbed it, may still be on its way behind the hints
   that this look acted on. This happens when the queue of the OS is read after a `watch` call
   looked, or when a reader lags behind (Inode seed 2074718981). The doubt ends once every record
   from the time the file was young has arrived:
   1. The engine asks the backend about the time at which the earliest `Pending` file becomes old
      (`Backend::ask_delivered`).
   2. The backend drains the queue of the OS and answers with `Hint::Delivered`, behind every
      record it read (rule 13, §4.1, §4.3).
   3. The engine drops the `Pending` state of every file that was old by then
      (`forget_delivered`).

   This does not happen while work from earlier hints is waiting (a mark, a scan, a loss report, a
   hold), because that work may count such a file as written. A directory that was away is such
   work, since its scan, or a mark or a hold above it, is waiting. A doubt lasts for the youth plus
   the time to the answer: about a millisecond under inotify, and up to one clock tick (15.6 ms)
   under Windows. A file far ahead of the clock keeps its doubt until a look reports it, and so does
   every file of a backend that never answers. FSEvents and the poll have no `written_hints`, and
   the default implementation of the trait never answers. Previously, the first look that found the
   file old ended the doubt, which missed the write of that seed. A file that no look found old kept
   its doubt forever, so a loss report, or a directory moved away and back long afterwards, gave it
   an `Update`. At a loss report an hour later, 1,000 untouched files got 1,000 `Update` events.
   The test `a_file_whose_records_all_came_is_not_counted_as_written_later` covers this, and the
   story `a_directory_is_moved_away_changed_and_moved_back` showed it on Linux since 4e1026c.

   The read that this saves is the one that waits for the virus scanner on Windows: 5 to 12 ms
   after a write (measured 2026-10-20), and 2 ms, or 14 ms for one write in ten, on 2026-10-13. It
   was the lower bound of an edit reported with no quiet time, and the tail of a burst. For a while,
   a number that NTFS keeps for each file, the position of its last record in the change journal,
   replaced the hash there. It cost 0.04 ms instead of the read. It does not change for a second
   write through a handle that stays open, which was a gap of its own, and it was one more thing to
   maintain. It was removed.

| Modification time | Young for |
|---|---|
| has a fractional second, and an OS watch covers the file | 20 ms |
| is a whole second (the file system counts in seconds) | 2 s |
| any, and only the poll looks at the file | 2 s |

The clock is the watcher's clock (`Fs::now`), not the disk's. A disk whose clock lags behind it,
such as a network disk, has no young files. A second write of the same size within one step of its
timestamps is then found only where the OS reports writes (`behavior.md`, "Known limitations"). A
disk whose clock is ahead is the next case.

**A time far ahead of the clock.** A file whose time is more than 2 s ahead of the clock never
becomes old by the clock. Such a time comes from the clock of another machine, or from a tool that
set it. Such a file used to be young forever. With polling, each round read every such file again
(measured: 5 files of 4 MiB, 420 MiB read in 20 rounds). A write after a look gives the file
another time, whatever the clock of its disk says. So the file is young for 2 s by the engine's
clock after the first look that found its current fingerprint (`Tree::ahead_since`), and old after
that.

"Only the poll looks at it" means that the directory of the file is polled, or that the node is a
symbolic link (§2.14). With polling, a young file larger than 16 MiB therefore gets one `Update`
when it is 2 s old, even when nothing changed (§5).

When the file disappears between the `lstat` and the read for the hash, the check reports nothing,
and the node is checked again in the next flush.

### 2.9 Events, and the delete delay

**Events are read from the tree.** A node has two bits for the handler: `told`, and `changed`, which
means that the file differs from the one the handler was told about. A check only changes the node
and adds it to the list `touched`. At the end of the flush, `finish` reads each touched node once
(`engine/events.rs`) and decides its event by the table of `design.md` §4.3, where "it changed" is
`changed`. Then `told` reflects what is there, and `changed` is cleared.

**The delete delay** (`design.md` §4.5). A held node keeps the time of its second look in a queue.
When no hint requested the first look, the delay counts from the time of the look, not from the
start of the flush, which can be much earlier. A first look that comes after a quiet time longer
than the delay, or under a flood, gets its second look at once, in the next flush. A path that is
back within the longer of the delete delay and the quiet time is therefore one `Update`. A directory
that vanishes with 50,000 files is one held node, and the backend releases the watch of a held
directory at once. At a second look that still finds nothing, a file is forgotten and a directory is
vanished (§2.5). A change of kind ends the hold at once (§2.4). With a delay of zero
(`with_delete_delay`), nothing is held.

**The default** is 10 ms. Editors save by writing in place (VS Code), by renaming a new file over
the old one (`sed -i`, an atomic save), or by renaming the file away and the new one in (a JetBrains
IDE), and `git checkout` removes and writes. In each case the path is missing for microseconds at
most. On Linux, none of them was wrong with 10 ms. This was measured in a VM on 2026-10-22, with 20
saves of each kind and 20 checkouts of 50 files per run, idle and with every core busy. On Windows,
none of 505 saves per round was a `Delete`, idle or under a load of 16 busy threads on 20 cores plus
a writer of files next to the tree (2026-10-20, with each quiet time of §2.13).

vim and Neovim on Linux are the exception, and not the case that the default is designed for. They
rename the file away and sync their swap file before writing the new one, so the path is missing
for 4 ms (median) when idle. On a busy machine it is missing for more than 10 ms in 31% of the
saves, for more than 20 ms in 18%, and for more than 50 ms in 6%. Such a save is a `Delete` and a
`Create` when the path stays missing longer than both the delete delay and the quiet time. With
10 ms, this happened in 1 of 60 saves when idle and in 7 to 23 of 60 when busy. With 50 ms and a
quiet time of 20 ms, it happened in none when idle and in 0 to 7 when busy (`behavior.md`, "Known
limitations").

On macOS FSEvents reports about 10 ms after the change, by which time a save is complete (§2.13).
The delay there is 10 ms, a cheap guard, and it is the only wait, since there is no quiet time. With
a quiet time of 10 ms or 20 ms and no delay, none of the saves of VS Code, vim, `sed -i`,
`git checkout` and a JetBrains-like save was wrong, idle or busy.

When the watcher polls everything, there is no delay, since there is no quiet time
(`Config::into_options`). The same holds in chokidar (`atomic` is off under `usePolling`), notify's
`PollWatcher`, watchdog's `PollingObserver` and watchpack. A round that falls inside a save that
leaves the path empty reports a `Delete`, and the next round a `Create`. With vim on Linux, polled
every 100 ms, this happened in 4 of 100 saves, and in none with a delay of 10 ms or more. The engine
still supports the second look under polling (`Options::delete_delay`), in case users ask for it.

### 2.10 Want, watch, poll

**The want rule** is that of `design.md` §4.12. On FSEvents, `watch("/home/me/project")` therefore
puts a stream on the project, not on the home directory. When directories above a watched path are
missing, the watch of the lowest existing directory reports the whole tree below it on FSEvents and
Windows, and the watch stays there afterwards.

The implementation (`Tree::want`, `Engine::want`, the end of `check`) is arranged so that no file is
looked at before its watch is live:

- The tree answers when it knows. A complete directory, or a directory with a registered name that
  has a file or a held node, wants `Contents`. A directory whose registered names are all
  directories wants `Names`.
- Where the tree has nothing at a **watched path**, the disk is asked before the backend, and
  **only for the kind** of what is there. Nothing is stored. In a batch of watched files this costs
  one extra `lstat` per directory that holds some of them.
- Where the tree has nothing at a **name on the way**, the disk is not asked, because a directory
  is expected there.
- When a check finds no directory at a registered name whose directory was asked for names only,
  that directory is asked for `Contents`. This queues a scan of it, so the name is looked at once
  more when the wider watch is live.
- For one key the want only increases. It decreases in one case only, through `unwatch` (`narrow`,
  §2.7).

`arm(node)` asks the backend for a watch when a directory appears, when a check finds it still
there, and when the node wants more. It calls `backend.watch` when the node wants more than was
requested, or when the caller says that the watch may be gone. Each call queues a scan of the node
for after the next commit. The watch state of a node is one of these:

| State | Meaning |
|---|---|
| `Watched` | The backend watches this directory. |
| `WatchedTree` | The backend watches this directory and every real directory below it. |
| `FromAbove` | A node above is `WatchedTree`, and the backend is not asked about this one. |
| `Polled` | No OS watch covers it. The answer was `NotWatched`, the node is held, or there is no backend because it did not start or it died. |
| `Failed` | The backend could not watch it, and the error is reported once. |
| `None` | The directory was just found and the backend has not been asked yet, or no directory is on disk. |

- A node that loses `WatchedTree` gets a deep scan, which requests the watch of every node below
  it.
- A node whose key `commit` reports as failed becomes `Failed`, with the reason that the backend
  gave. No other node does.
- If the backend reports that it cannot continue, the error is reported once, every node becomes
  `Polled`, and everything is scanned. From then on the settings are those of a watcher that polls
  everything (`Engine::fall_back_to`): the configured poll interval and no delete delay. The same
  holds when the backend cannot start, in which case the engine has no backend from the start
  (`Engine::without_backend`).

**The poll.** The tree keeps a list of the directories that the timer inspects (`Tree::polled`), so
a poll does not walk every node. A directory is added to the list when its watch state is set (the
backend's answer, or polled when there is no backend to ask), when a look in it fails, and when it
is held. A stale key stays until the list is tidied. Each poll tidies it, and so do an insertion
that finds the list longer than twice the arena, and the end of a poll when nothing is watched any
more. A sequence of `watch` and `unwatch` calls on ever new paths, each shorter than the poll
interval, therefore cannot grow the list without bound. On each tick of the poll timer, each node
on the list whose directory is on disk is handled by its state:

- `Failed`: the backend is asked again, because it may have room now.
- `Polled`, or a scan of it failed: a shallow scan is queued.
- Any other state: a plain mark is put on each registered child that is a symbolic link (§2.14).

The marks and scans go through the normal flush, and the next poll is paced (§2.13). With a native
backend the timer fires once per second, and the polled nodes are few: the root, the directories
for which the backend answered `NotWatched`, and what failed. The timer runs only while the list
holds more than the root, while a child of the root is not on disk (a drive that is not present
yet), while a registered name is a symbolic link (§2.14), or while a flush has left work behind
because the disk stopped answering. The next poll resumes that work. With inotify, which also
watches the directories above, and with FSEvents, whose guard does so (§4.2), the thread sleeps
until a notification arrives. With the poll backend every answer is `NotWatched`, so every node is
polled, and the timer runs at the poll interval. Polling is this timer with the poll backend.

**Asked again.** `NotWatched` is not permanent. A commit can place an OS watch over a polled
directory. On Windows this happens when a handle comes to stand at or above a directory that asked
for names. It does not happen on FSEvents, whose guard answers such a directory, and whose streams
never stand at or above a directory that received `NotWatched`. `commit` then returns the keys of
these directories, and the engine calls `watch` again for each of them (`ask_again`). A directory
that is now reported gets a shallow scan after the next commit (watch, commit, then scan), and no
poll inspects it any more.

**A watch below a watch.** A directory can have a watch of its own and also lie below a tree watch,
when its path was watched first and a path above it later. It keeps its own watch until the
directory disappears or the tree above it is unwatched. A record below it then produces one hint for
each of the two keys, and the tree merges them into one mark. Until 2026-10-19 the lower watch was
released after a deep scan, so that the OS watches did not depend on which path was watched first.
That took 40 lines and two rules of its own, to save one cover and one duplicate hint per directory
on FSEvents and Windows.

**A directory above a watch by path.** A watch by path (FSEvents) is bound to the text of a path.
When a directory above it is renamed, even only to another spelling of its name, the watch reports
nothing more. When the directory is renamed away and back, the watch reported nothing in the
meantime. In both cases the poll of the directory above finds the same directory at the watched
path. The engine does not try to detect such a rename. The kernel informs the backend, the backend
reports each of its watches below that directory as lost (§4.2), and the engine watches the
directory again and scans it deep, as after every loss report.

Timestamps do not reveal such a rename, and the engine keeps none for directories. Measurements
show that a rename changes the change time of the directory on APFS only (not on HFS+, exFAT or
FAT32), and the modification time of the directory above. A file or a directory moved from the
directory above into the renamed one, with one `rename` call, changes the same two timestamps. A
rule that took this for a rename rescanned a watched project of 50,000 files each time a directory
was moved into it from beside it, at a cost of 250 ms of CPU time.

**`mark_moved`.** A mark can arrive through a watch that moved away with its directory. If the
parent of that directory is only polled, nothing reports this in time, and the marks would be
checked against the wrong directory. So at the start of a flush, for each marked node, the nearest
directory at or above it whose parent is only polled, or that is reached through a symbolic link,
gets a plain mark, unless it already has one. Marks are checked from the top down, so the parent
checks it first.

### 2.11 The ignore filter

The rules are those of `design.md` §4.8. The engine asks the filter in three places. `read_entry`
(`engine/check.rs`) asks about an entry inside a watched directory. `is_ignored_above` and
`register` (`engine/watched_paths.rs`) ask about the directories above a path given to `watch` and
about the path itself. The answers are stored with the node, one per kind (`Engine::ignores`). A
node on the way to a watched path keeps the answer for the directories above, so `read_entry` does
not ask about it, and an ignored path gets no node. Hints from inside an ignored directory end at
its parent and cost one `lstat` per flush. The stories in `tests/sim/filter.rs` fix this behavior,
and the fuzzer uses a closed filter and a filter by name.

Previously, the filter was also asked about the "watch target" of a path, which was the directory
that holds a watched file, or the lowest existing directory above a missing path. A path whose
target the filter ignored was dropped with the code of `unwatch`, with a `Delete` for each of its
files that still existed, and nothing revived it. This produced events that did not match the disk,
and answers that depended on which directories existed. The second case of `design.md` §4.8
replaces it.

### 2.12 Errors from the disk

The two classes of `design.md` §4.10 are handled as follows:

| Error | Treatment |
|---|---|
| not found, not a directory | Missing. |
| an invalid name, such as a path longer than the OS allows | Missing. Reported once as `Io`. The node is polled, since nothing can ever be at the path. |
| an entry that is being deleted (on Windows the name disappears when the last handle on it is closed) | Missing. The node is looked at again in each flush until the name is gone or the file is there, because the holder can cancel the deletion and the OS then reports nothing. |
| permission denied, symbolic link loop | Missing for events. Reported once. The node is polled until a scan of it succeeds. |
| an unreachable server (`ERROR_BAD_NETPATH` on Windows, which std maps to "not found") | No answer. |
| another program holds the entry without sharing (on Windows a sharing or lock violation, `ResourceBusy`) | No answer. The mark stays for the next flush, and the node is polled until a scan of it succeeds. Not reported, because nothing is wrong with the disk and the holder usually releases the entry in a moment. |
| anything else | No answer. The tree stays as it is, no `Delete` is delivered, and the mark stays for the next flush. Reported once. The node is polled. |

A failed `watch` is reported as `WatchLimit`, `PermissionDenied` or `Io`. `WatchLimit` is reported
once for the lifetime of the engine. A tree beyond the limit finds no room for one new directory
after another, and each would be another error (`the_watch_limit_is_reported_once`).

**A disk that other machines also change** (NFS, SMB, a host folder in a VM or a container). The
user decides whether to poll it (`with_polling`). In measurements, a folder of the Mac in a Linux
VM (virtiofs, FUSE) produced no inotify event for a write made on the Mac, and NFS on macOS produced
no FSEvents record within 15 s for a write made in the VM. The first OS watch on each disk,
identified by its device, therefore asks for the type of the file system (`Fs::remote_disk`, one
`statfs` call of 0.2 to 3 µs). On macOS such a disk is one that is not `MNT_LOCAL`. On Linux it is
NFS, SMB, CIFS, 9P or FUSE, identified by the type number rather than by its name, because the same
folder was `fuseblk` and later `virtiofs`. `RemoteDisk` is reported for each such disk, even when
one flush finds two, because the rule of one error per kind per flush would drop the second for good
(`each_disk_that_other_machines_change_is_told`). A local disk mounted through FUSE is reported too,
which is one error too many. A disk that is polled anyway is not reported
(`a_disk_that_other_machines_change_is_told_once`). Windows asks nothing, because an SMB server
reports the changes of other machines to the handle.

### 2.13 Time and the queue

The engine has no thread and reads no clock. Its entry points receive the time from the caller
(`engine/mod.rs`):

- `watch` and `unwatch`.
- `signal`: one signal of the backend.
- `overflow`: the queue dropped signals.
- `deadline`: the next time at which there is work, such as a flush, the second look of a held
  node, a poll, or a loss report waiting for the end of a rest.
- `tick`: perform what is due.

It reads the disk through the `Fs` trait. `StdFs` is the real disk, and `SimFs` is the disk of the
tests.

**The watcher thread** (`watcher.rs`) is one loop around a queue. A backend thread never blocks. At
most 16 MiB of signals wait (`Backend::raw_bytes` reports the size of a raw notification). A signal
that would exceed this is dropped and a flag is set, which the engine treats as lost notifications
(`overflow`). The drop that sets the flag also sends one message that wakes the thread, so the flag
is taken at once, at the position of the drop in the queue, and only there. A delivery (rule 13)
that arrives behind the drop says nothing about the dropped signal. When such a delivery was
processed at the end of the turn, before the overflow, it ended the doubt of a young file whose
write that signal may have carried (§2.8,
`a_delivery_behind_a_dropped_signal_does_not_end_the_doubt`). A `watch` or `unwatch` before that
position needs no notice of the loss either, because a young file that its flush finds old keeps
its doubt (§2.8). Commands (`watch`, `unwatch`) and the notice that the backend died, which comes
once (rule 12), are never dropped and are not counted. The loop takes at most 512 messages and then
performs what is due, so a flood cannot delay a flush.

A command from inside the handler takes a separate path. The thread takes it when the handler has
returned, before the next message of the queue, so what the handler requested is in place before
anything that arrived meanwhile. Each such command delivers its own batch, because the disk can
change between two of them and a batch has at most one event per path
(`each_call_inside_the_handler_is_a_batch_of_its_own`).

On Windows the thread does not wait on the channel for a deadline. A timed wait on a channel ends at
a clock tick, which occurs every 15.6 ms unless a program requested a finer resolution. A wait of
20 ms took 31 ms, and one of 1 ms took 15.6 ms (measured), which accounted for all of the 11 ms
between the quiet time and the reported edit. The thread instead waits on an event that every
sender sets after its message, and on a high-resolution waitable timer, which fires within half a
millisecond (`watcher::windows`). On the other systems the timed wait of the channel is accurate.

**The debounce** (`design.md` §4.11) is computed by `Engine::schedule`. The bound on the wait is four
quiet times, and at most 500 ms unless the quiet time is longer. It is counted from the first hint
after the previous flush, just as lodash's `maxWait` counts from the first call. Measured on Linux
with a file in the watched tree written every 5 ms, an edit next to it waited 364 ms (p50) when the
longest wait was 500 ms, and 25 to 28 ms with four quiet times. On macOS FSEvents leaves gaps of
about 10 ms, and the edit waited 22 ms either way. A burst of 2,000 edits was delivered in 3 batches
on macOS instead of 1, all equally soon (2026-10-15). @parcel/watcher counts its 500 ms from the
previous batch instead. The first change after half a second of quiet is then delivered at once,
alone, and a disk check at that moment can catch a save that is half done (§2.9).

The default quiet time on Linux is 10 ms. It is long enough for the steps of one save (a temporary
file, a rename) to form one batch, even on a busy machine, and short enough not to be noticeable.
This was measured on Linux with the saves of §2.9 on a busy machine. With 5 ms, 4 of 60 saves and 2
of 1,000 files written by `git checkout` were reported twice, the first time half written. With
10 ms none were (2026-10-15), and again none in the runs of 2026-10-22, idle and busy, where an
edit was reported after 11 ms instead of 21 ms with 20 ms. vim and Neovim sync the new file before
closing it. A sync longer than the quiet time, which is common on a busy machine, makes such a save
reported twice, the second time after the close. With 10 ms this happened in 4 of 60 saves when
idle and in 10 to 14 of 60 when busy. With 20 ms it happened in none of 300.

On Windows a save produces two records, one when the open truncates the file and one at the close,
and nothing for the writes in between (measured). The quiet time can only join those two, and a
flush that falls between them reports the file twice, since the record of the close is a `Written`
hint (§2.8). This was measured with the saves of §2.9, 15 of each kind, and `git checkout` of 100
files 4 times, idle and under a load of 16 busy threads on 20 cores. With 20 ms nothing was wrong.
With 10 ms only `git checkout` was wrong, for 1 file of 1,600 when idle and for 2 or 3 of 400 under
load. With 5 ms, 3 to 5 of 400 were wrong, one of them half written. The default there is 10 ms, for
the idle machine on which editing happens (2026-10-20).

On macOS FSEvents reports about 10 ms after the change, by which time a save is complete. No value
was wrong there, whether idle, busy, or under a flood of 30,000 writes per second, when FSEvents
reports at once. A 1 MB save was reported 1 ms after it, still as one `Update`. The default there
is therefore zero, as in watchdog, notify and Node, and an edit is reported twice as soon (12 ms
instead of 24 ms). The cost is that a temporary file that lives longer than FSEvents waits is
reported, as a `Create` and a `Delete` for a file that nobody cares about. vim's swap file causes up
to one such pair per save. With 10 ms there are none when idle, and fewer under a flood. A flood
also arrives in more, smaller batches.

A check made by a scan takes the mark waiting on its node with it, `written` included.

**Pacing** (`design.md` §4.11):

- Loss reports. All scans requested by the loss reports of one flush are timed together. After
  they took some time, the next ones wait nine times as long (`REST`), and at most half a second
  (`LONGEST_LOSS_REST`). The rest guards against a storm of loss reports, and the bound keeps one
  long scan from postponing the next loss report by seconds. A directory of 20,000 entries takes
  about 200 ms to scan. Every mark that a hint requested is checked as usual.
- The poll. The pause between two rounds is the only control of polling, as in notify and
  watchdog. A round looks at every watched file once. For 20,000 files in 2,000 directories that is
  22,000 `lstat` calls and 50 ms on APFS (measured), and a user with many files raises the
  interval. For a while the rule was a rest of nine times the looks, at most a tenth of a core. It
  made the interval a floor that the user could not see, and 20,000 files were polled every half
  second.

The time counted is the time of the whole flush in which the poll or the scans ran. The checks
requested by hints in the same flush count toward it, as do the reads of young files and the time
that the backend takes to make watches live. A flush counts for at most one minute
(`LONGEST_FLUSH`). The clock keeps running while the machine sleeps, and a flush interrupted by
sleep would otherwise pause the work for hours.

### 2.14 Symbolic links

The rules are those of `design.md` §4.9. For a registered name that is a symbolic link, the engine
stores a hash of the real path behind it and compares it at each look. If the look behind the link
gets no answer, the stored hash stays as it was. A node reached through a link never becomes
`FromAbove`, so the backend is asked about it. Every poll puts a plain mark on each registered child
that is a link (§2.10), and `mark_moved` treats a node behind a link like a node with a polled
parent. An unregistered link is never opened (§2.8).

## 3. Backend interface

The interface is defined in `src/backend/mod.rs`. The rules that every backend follows are in
`design.md` §5. The file has no documentation comments.

**The `Backend` trait.** Every backend implements `watch`, `unwatch` and `translate`. An OS backend
has its own `new(sink: Sink<Self::Raw>)`. `PollBackend` is a unit struct and needs no sink.

| Item | Meaning |
|---|---|
| `Raw` | A raw OS notification. It is cheap to create, because the backend thread only copies it into the sink. |
| `watch(key, dir, want)` | Report changes in `dir` as hints that carry `key`. The answer is a `Coverage`. |
| `unwatch(key)` | Forget `key`. No hint should carry it afterwards, and a late one costs the engine one check. |
| `commit()` | Make every watch requested since the previous call live. It returns two lists of keys (`Committed`). The first holds the keys whose watch could not be made live, each with the reason. The engine reports the reason, polls the directory, and asks again later. A key that is not listed is live. The second list holds the keys that received `NotWatched` and whose directories an OS watch now reports. The engine calls `watch` again for each of them, with the previous `Want`, and does not poll a directory that is reported (§2.10, "Asked again"). Only a backend that places one OS watch over many directories has work to do here, because its answer for one directory depends on the OS watches of other directories. FSEvents lists the keys that received no stream or no guard, and the Windows backend lists the keys that a handle now reports. Every other backend omits the method, which then does nothing and returns no key. |
| `translate(raw, hint)` | Turn one raw notification into hints. |
| `ask_delivered(after)` | Answer with `Hint::Delivered` once every notification of a change made before some time at or after `after` has arrived (rule 13). A backend that omits it never answers. |
| `raw_bytes(raw)` | The number of bytes that `raw` keeps on the heap (§2.13). A backend that omits it reports 0. |

**The types.**

- `NodeKey` identifies one directory node of the engine. A backend stores it and puts it into
  every hint for that directory, so no path spelling has to match between the two sides.
- `Want`, `Coverage` and `Hint` are the vocabulary of `design.md` §5.1. The code adds the
  following:
  - `Failed(error)` carries the error that the engine reports.
  - The engine paces the scans that a `Changed` with an empty `rel` requests (§2.13), so a backend
    may send that hint often.
  - In `Written { dir, rel, new }`, `new` says that a file was created or moved there (inotify),
    with content different from what the name held before. A twin of the name is left to its look
    (§2.2).
  - `Mount { path }` comes from inotify (§4.1). The engine checks the node at that path, if it has
    one.
  - `Delivered { before }` is the answer to `ask_delivered`, and it ends the doubt of the young
    files that were old by then (§2.8).
- `Sink` is the channel from a backend thread into the engine. It never blocks. It carries a
  `Signal`, which is either `raw`, a raw notification for `translate` on the watcher thread, or
  `failed`, the notice that the backend cannot continue. From then on the engine polls everything,
  and it drops the backend at once, so that its kernel watches, streams, handles and threads do not
  wait for the end of the watcher. An inotify watch counts against a limit that all programs of the
  user share.
- `watch_limit(message)` creates the error for "the OS has no room for one more watch". It is an
  `io::Error` that carries a marker of its own, so that no error of the OS is mistaken for one.
  Previously, the marker was the kind `StorageFull`, which the OS also returns. The message names
  the limit and what to raise, so the text of the error names the exact limit that was hit.
  Previously, the kind named one limit per OS, even when another limit was hit. `is_watch_limit`
  tells whether an error is one, and the engine reports such an error as `ErrorKind::WatchLimit`.

**The rules in code.** For rule 12, the backend calls `Sink::failed` once, and the sink is not
called after the backend is dropped. For rule 13, the request is `ask_delivered`. inotify (§4.1)
and `ReadDirectoryChangesW` (§4.3) put a notification, and a loss report, into their queue before
the call that made the change returns.

## 4. Backends

### 4.1 inotify (`backend/inotify.rs`)

- One kernel watch covers one directory. It does not reach the directories below, so every answer
  is `Watched`, and the engine asks for every directory of a tree.
- The requested events for `Names` are `CREATE`, `DELETE`, `MOVED_FROM`, `MOVED_TO`, `MOVE_SELF`
  and `DELETE_SELF`. For `Contents` they are these and also `MODIFY`, `ATTRIB` and `CLOSE_WRITE`.
  The flags are `EXCL_UNLINK`, `ONLYDIR` and `MASK_ADD`. There is no `OPEN` or `ACCESS`, so the
  reads of the engine produce no events.
- A kernel watch belongs to the inode. A second `inotify_add_watch` for a directory (another
  spelling, or a renamed directory) returns the old `wd`. The new key is added to it, and every key
  of a watch receives every hint. The kernel watch is removed with its last key. A watch almost
  always has one key, so the first key is stored in the table itself, and only a second one
  requires memory. This costs 71 bytes of heap per watched directory and no lasting allocation,
  where it was 92 bytes and one allocation before. The kernel keeps about 1 KB per watch. The two
  tables shrink when most of their keys are gone. After 100,000 directories were unwatched, 67 KB
  remain instead of 6.1 MB.

Each event becomes a hint as follows:

| inotify event | Hint |
|---|---|
| `MODIFY`, and `CREATE` or `MOVED_TO` of a non-directory | `Written`, because the name now holds different content. |
| any other event of an entry | `Changed`. |
| `ATTRIB` of a directory | Dropped. |
| `Q_OVERFLOW` | `Lost`. |
| `MOVE_SELF` of the watched directory | `Changed` with an empty `rel` for each key. The watch and its keys remain, and the next `watch` of the engine moves the key to the directory that is now at the path. |
| `DELETE_SELF`, `IGNORED` or `UNMOUNT` of the watched directory | The watch is forgotten, and `Changed` with an empty `rel` is sent for each key. |

A full queue also drops `IGNORED` and `DELETE_SELF`, so a watch that the kernel abandoned can remain
in the table. The scans after `Lost` ask for every directory again, a new `wd` arrives for such a
directory, and the old entry is removed with it.

- A reader thread passes on the bytes of each read. It reads at most once per millisecond
  (`READ_INTERVAL`), and again at once after a read that filled more than half of its buffer. A
  first change is read when it arrives, and what follows within the millisecond comes with the next
  read. While events wait in the queue, the kernel merges an event that repeats the previous one,
  and the two threads wake up at most a thousand times per second (see the measurements below). The
  watcher thread adds and removes the kernel watches itself, so nothing needs a lock.
- **A volume mounted or unmounted** over a watched directory, or over a directory above one,
  produces no inotify event. This was measured with tmpfs over a watched directory, its parent and
  its grandparent, and the watches stay on the directories that the volume hides. The reader thread
  therefore also waits on `/proc/self/mountinfo`, where the kernel signals each mount and unmount
  in the namespace with a priority event (`POLLPRI`). In measurements the event came 19 to 27 ms
  after `mount`, with no event when idle and none for a mount in another namespace. The reader then
  reads the table again and compares it with the previous one by mount ID, since a volume mounted
  over another at the same point is also a change. It sends the mount points that appeared or
  disappeared together (`Raw::Mounts`), and each of them becomes a `Hint::Mount` with its path. The
  engine checks the node at that path, if it has one, and recognizes the different directory there
  by its identity (`a_mount_over_a_watched_directory_is_told`,
  `a_volume_mounted_over_a_watched_directory`). The path is a real path, so a mount that a watched
  path reaches through a symbolic link is not found this way. macOS reports mounts differently
  (§4.2).
- **A delivery** (rule 13, `forward`). The reader also waits on an eventfd for a request from the
  engine, and for the time that the request names. It then drains the queue and sends
  `Raw::Delivered` behind what it read, with the time at which the last read began. This becomes
  `Hint::Delivered`. While draining, the reader ignores `READ_INTERVAL` and reads once per turn of
  its loop, so that a flood that keeps the queue from emptying does not keep the drain from
  stopping. The kernel queues an event within the call that made the change, before the call
  returns (`fsnotify` from `vfs_write`, and from `__fput` for the close), and a full queue receives
  one overflow event at its end (`fsnotify_insert_event`). This was measured on Linux 6.6, ext4 and
  tmpfs, with 40,000 writes from the same thread and from another thread. The event of each write
  was in the queue when the write returned, and the overflow event came after the 16,384 events
  that the queue held (2026-10-22). A read of the empty queue costs 0.2 µs. A change of the mounts
  that arrives in the same wake-up is reported first, ahead of the answer. The wait that sees the
  change consumes the notice, which the kernel gives only once (`mounts_poll`), and a reader that
  only answered never reported the mount (`a_mount_in_the_wake_up_of_a_delivery_is_told`).
- `ENOSPC` becomes `Failed` with the watch limit error. Every other error becomes `Failed` with that
  error. `EMFILE` and `ENFILE` when the backend starts indicate the limit of inotify instances
  (`fs.inotify.max_user_instances`) or of open files, and also produce the watch limit error.

**Limits.** `MOVE_SELF` arrives only when the watched directory itself is renamed. A move of a
directory above it is reported by the watches on the way. One behavior is expected but not tested:
a removed directory that a process holds open sends `DELETE_SELF` only when that process releases
it.

**Measurements** on Linux, with 13 CPUs, the default queue limit of 16,384 events, and two events
per new file:

- With 20,000 directories, `watch` takes 0.6 to 0.7 µs per directory (0.3 to 0.4 µs for one that
  already has a watch), `unwatch` 0.6 to 0.7 µs, and `translate` 7 ns per event.
- New files on a disk (btrfs), 200,000 per second from 4 or from 8 threads. There is no overflow,
  and every event arrives.
- New files on tmpfs. At 800,000 per second from 4 threads there is no overflow. At 900,000 per
  second from 8 threads there are 15 to 20 overflows per run, and 36% to 47% of the events arrive. A
  reader that uses the bare API and only counts gets the same result, so the reader thread of the
  backend is not the cause. The writers hold the lock of the queue.
- Small writes into files of one watched directory, on a disk. Every `write` call is an event.
  With a read on every wake-up, one writer (a million writes per second for half a second) cost the
  reader thread 220 ms of CPU time and the thread that receives its reads 130 ms, for 85,000 events
  in 75,000 reads. With one read per millisecond, it cost 2 ms and 3 ms, for 190 events in 135
  reads. Eight writers for five seconds cost 3 s and 1 s before, and 0.6 s and 0.05 s now, for the
  same four million events. There was no overflow in either case. One new file is read 60 µs after
  the call, as before. The floods above overflow as often as before, and the writers of the flood
  on tmpfs are faster (1.3 million files per second, up from 0.8 million), because the reader takes
  the lock of the queue less often.
- A directory shared with another machine, seen from a virtual machine, with the file system
  `fuseblk`. `inotify_add_watch` works, changes made inside the machine arrive, and changes made by
  the other machine do not arrive at all. Nothing informs the backend of this, so the engine asks
  for the type of the file system (§2.12).
- The queue and its lock belong to the inotify instance. With the directories spread over two
  instances the overflow disappears or becomes rare, and with four it disappears, even when one
  thread reads all of them. This is not implemented. A user has 128 instances by default, an
  overflow requires more events than a disk produces, and the hints of one instance arrive in the
  order of the changes, which the tests of the backend rely on. After an overflow the engine reads
  everything again (`Lost`).

### 4.2 FSEvents (`backend/fsevents.rs`)

#### Measurements on macOS (APFS)

- A stream watches whole trees and matches records by path text. Records carry the **real** path,
  with symbolic links resolved and with the letter case and Unicode form of the disk.
- A renamed or moved directory produces one record for the directory and none for its contents.
- A stream is bound to the text of its path, which is resolved once, when the stream is created.
  After a directory above its path is renamed, even only to another letter case, the stream reports
  nothing more, and it reports nothing about that rename either. When the directory is renamed
  back, the stream reports again. A rename of its own directory to another case produces one record
  for it. A rename through another spelling (`mv X away` where the disk has `x`), and a removal
  through another spelling, produce no record for the directory.
- A directory renamed inside a stream produces two records, one for the old path and one for the
  new path, both with `ItemRenamed` and `ItemIsDir`, also when only the case changes.
- `kFSEventStreamCreateFlagWatchRoot` produces a `RootChanged` record when a directory at or above
  the path of a stream is renamed or removed. It is not used, for three reasons, all measured:
  - It looks at the path about 100 µs after the rename and sends the record only if the path is
    still different. "Rename away, create a file below, rename back" with no pause produced no
    record at all in 3 of 230 runs, and the file was not reported.
  - It opens one file for each name of each stream path, and paths do not share them. 64 paths of
    9 names are 769 open files.
  - A stream with it takes 0.09 to 0.16 ms per path to create (paths of 9 and of 11 names),
    compared with 0.002 ms without it.

  Other watchers that set it stop watching on that record, or report the watched directory as
  removed: Watchman, .NET, notify and watchdog.
- The guard (see the implementation) relies on these facts. The kernel reports a rename or a
  removal of a directory through a queue for vnode events (`kqueue`, `EVFILT_VNODE` with
  `NOTE_RENAME`, `NOTE_DELETE` and `NOTE_REVOKE`), on a descriptor of that very directory opened for
  events only (`O_EVTONLY`). Such a descriptor does not prevent a disk from being unmounted:
  `diskutil eject` works, and the descriptor receives `NOTE_REVOKE`. The measurements were:
  - The event is present when `rename` returns.
  - Two renames with no pause produce one event.
  - No event was missed in 4,000 runs each of "rename away, create a file below, rename back", of a
    rename that only changes the case, of a swap of two directories in one call, and of a directory
    that is replaced. The same holds on APFS, HFS+, exFAT and FAT32.
  - Nothing arrives on a descriptor of a directory below the renamed one, so each directory on the
    way needs its own descriptor.
  - Opening and registering a descriptor takes 10 µs, and the kernel keeps 248 bytes for it: a
    `fileproc`, a `fileglob` and a `knote`. 10,000 descriptors added 10,000 of each in `zprint`. The
    process itself keeps nothing more.
  - A waiting queue costs no time, 0.02 ms of CPU time in 5 s with 10,000 directories.
  - A dispatch source of type `READ` on the descriptor of the queue calls its handler 0.6 ms after
    a rename, and again while events wait. 200 events that waited at once arrived in 4 calls of at
    most 64.
  - `fcntl(F_GETPATH)` on such a descriptor returns the current path of the directory in 0.3 µs, in
    the short spelling of a path on the data volume (`/Users/...`).
- A volume mounted on a directory, such as a disk image. The descriptors of that directory and of
  one below it receive no event, and the descriptor of its parent receives only `NOTE_WRITE`. A
  stream below the mount point receives no record, and continues by the text of its path, into the
  volume. A stream at or above the mount point receives a record for it with `Mount` (`Unmount`
  when the volume disappears), and without `ItemIsDir`. The kernel queue reports each volume that
  appears or disappears (`EVFILT_FS` with `VQ_MOUNT | VQ_UNMOUNT`), but not where. This also works
  for a process that is not root, and only the requested kinds are reported.
- Flags are merged and stay set for many seconds, so they are never read as "what happened".
- Stopping a stream discards what it has not delivered.
- Records are lost silently when one stream takes over from another while files are written
  quickly (4 threads without pause, 300 files):
  - An old stream that stopped as soon as it had delivered the event ID of the moment of the start
    missed a change in 5 of 30 runs, also with a synchronous flush before the stop.
  - A new stream started from the event ID of that moment (`sinceWhen`), with a flush and no
    overlap, missed a change in 8 of 30 runs.
  - With 500 ms of overlap, nothing was missed in 40 runs.
  - With a loss report at the stop, nothing was missed in 40 runs, also with no overlap.
- Records are lost with notice when files arrive at one stream faster than it delivers them. One
  callback call of a stream carries at most 32 records. The flood of the probe was 20,000 new files
  from 4 threads, about 50,000 per second, distributed in turn over 8 or more directories:
  - A stream for the directory above them, or one stream with these directories as its paths,
    reports `UserDropped` and `MustScanSubDirs` for each of its paths in 4 of 14 floods, and names
    87% to 99% of the files. With 64 paths it does so in 12 of 12 floods, 1 to 15 times per flood.
  - With the same paths in 8 streams, there was a loss in none of 72 floods. With 2 streams, there
    was a loss in 1 of 10.
  - There was no loss either with 26,000 files per second, or with all files in one directory.
  - A program that uses the bare API with a callback that only counts receives what the backend
    receives. Both got a loss report in 7 of 15 floods, so the callback is not the cause.
  - The latency of the stream (0 to 0.2 s) makes no difference.

  The paths are therefore spread over several streams, and a loss report that still arrives (a
  flood below one path) must be cheap to act on.
- A stream takes longer to start the more files lie next to the executable of the process: under
  1 ms next to few files, 0.1 to 0.2 s next to 100,000, and 0.3 to 0.9 s next to 360,000 (a
  `target/debug/deps` that was never cleaned). The same program starts fast again from a copy in
  another directory.
- `FSEventStreamStart` waits for an answer from fseventsd, with no time limit. While fseventsd was
  busy, a start took 16 to 430 ms, and 20,000 `watch` calls of one file each took 13.8 s instead of
  0.47 s.
- The limits are 512 started streams per process and 4,096 paths per stream.
- A write to a file that stays open, and a write through `mmap`, is not reported before the close.
- Many stream paths lose nothing, but they are slow while other programs write a lot. With 50,000
  other files written, the time from the last write until all records had arrived was 0.1 s with 1
  or 100 paths, 0.9 s with 500, 1.8 s with 1,000, 3.5 s with 2,000, and 6.9 s with 4,000. A stream
  with one path in the same process was equally late. With no other writes, 2,000 paths take 0.1 s.

#### Implementation

**Streams.** The stream flags are `FileEvents | NoDefer`, with latency 0, starting from "now". One
serial dispatch queue serves all streams. The callback only copies the path and the flags of each
record into the sink.

**Cover.** A cover is a directory that the engine watches and that a stream reports. It is filed
under its real path with every key, in a table by path (`Dirs`). For `watch(key, dir, Contents)`, if
no stream reports the real path of `dir` yet, the path goes into the stream of the next `commit`,
and the answer is `WatchedTree`. For `watch(key, dir, Names)`, the answer is `WatchedTree` if a
stream reports the directory, and `Watched` otherwise. In a directory on the way, names appear and
disappear only when one of them is renamed or removed, and the guard of that name reports it
(below), as the directory's own guard does for the directory. A stream is never placed on `/`,
`/Users` or a home directory, where every program of the user writes. A cover there is answered
`NotWatched` and polled. The rule (`may_watch_a_tree`) is the one that places no Windows handle on
`C:\`, `C:\Users` or `C:\Users\<name>` (§4.3). Previously, a watched path whose lowest existing
directory was the home directory placed a stream on the home directory. 2,000 files written
elsewhere in it then produced 3,680 records, each dropped after one lookup.

**Stream paths.** A stream path is a directory for which a stream was started. Usually it is a cover
with no cover above it. With more than 64 stream paths (`MAX_STREAM_PATHS`), a commit places all
covers again by the rule of `place`, the same rule as for the Windows handles. The measurements
above showed that up to 100 paths cost nothing. Paths move up to directories that covers have in
common, the deepest first, until 64 remain. A path never moves up to a directory that is too high
(`is_too_high`). Such a directory is `/`, a directory in `/`, a directory one level further down (a
home directory, the top of a volume, `/private/tmp`), or the temporary directory of a user together
with the directories above it. Where only such a directory would serve, the paths stay separate. A
cover below a running stream path needs no new stream. `place` sorts the covers and walks them once.
It finds the depth of the last directory that has to be taken, takes every common directory below
that depth, and at that depth takes one directory after another while too many paths remain. For
20,000 covers in a deep tree this takes 13 ms, and for 100,000 it takes 69 ms. With a pass over all
paths for every directory taken, it took 38 ms and 724 ms.

**The real path** of a directory is composed of the real path of its parent, which is kept until the
next commit, and of its own name as the disk spells it (one `getattrlist` call). A symbolic link,
and a name that differs by more than the case of ASCII letters, are left to the OS (`realpath`).

**Streams are only added.** `commit` starts streams for the new paths. Old streams keep running, so
nothing ever falls into a gap. `commit` places all covers again when 64 streams are running
(`MAX_STREAMS`), when there are too many stream paths, or when a new path lies above a running one,
so that no stream path lies below another and no record arrives twice for long. The covers are then
placed on 56 paths, not 64. This leaves room for the paths of the next few commits, so that a few
new directories do not replace every stream again. A running stream whose paths are all placed
again, and are still trusted, continues; of two streams on one path, the newer one continues. New
streams start only for the other paths, and only the streams that do not fit are replaced. With 64
streams running, only the 24 with the most covers at and below their paths continue, so that the
next commits have room. Before 2026-10-19 every stream was replaced. After one `watch` call for each
of 20,000 files, the stops of the old streams then made the engine read 44,058 entries again. Now it
reads 11,990. If the streams cannot start, `commit` lists every key of the new paths as failed, with
the watch limit error, and the old streams keep running.

**Up to 8 streams per commit** (`MAX_NEW_STREAMS`). There are more only where a stream would receive
more than 4,096 paths. The paths of a commit are distributed over its streams in turn, so that
neighbors in the tree have different streams, and a flood over many watched directories is shared
by all of them. One stream for 64 directories reported a loss in every flood of the probe, and 8
streams in none. 40 directories of a second `watch` call reported a loss in 10 of 12 floods in one
stream, and in none in 8 streams. A loss report makes the engine read all covers of the stream
again. The 8 streams take 3 ms longer to start than one. Two streams keep no order between their
records, and the engine needs none. The limit of 64 running streams is high so that streams are
replaced rarely. One `watch` call for each of 20,000 files took 540 ms with a limit of 16, 420 ms
with 32, and 350 ms with 64. Before, with one stream for all covers and a limit of 8, it took
390 ms. One case remains. Covers that share one stream path have one stream, and a flood below it
can still end in a loss report. This applies to more than 64 directories with one directory above
them, and to a watched tree.

**An old stream that stops.** A replaced stream is stopped as soon as the new ones run, and the
backend reports each cover at and below its paths as lost. The engine reads them again, because a
stopping stream discards what it has not delivered, and the new stream may not have reported
everything from its first moments (see the measurements). What the old stream did not deliver is on
the disk, and the read after the stop finds it. A cover above its paths has a stream above it of
its own, since trusted stream paths never lie one below another. That stream either continues,
stops with a report of its own, or arrived with the replacement and started before the cover was
read, as for every new cover. Before 2026-10-19 the report was for `/`, so it was `Lost`, and the
engine read everything. Until 2026-10-19 a replaced stream also ran for 500 ms alongside the new
ones, with a timer on the guard's queue that triggered the stop. The report alone was measured to
lose nothing. A probe of 480 replacements, each under a burst of 300 files from 4 threads, lost
nothing with the report alone, and nothing with no report either. Streams that stop at the same
time share one report, and the scans of loss reports are paced (§2.13). A stream stops on the queue
of the streams, and the report is sent from there after the stop, but the watcher thread does not
wait for it. A callback can occupy the queue for a long time. Under a busy fseventsd, the waits for
1,161 stops in one `watch` call for each of 20,000 files added up to 21 to 27 s. The `Drop` of the
backend waits on the queue after these stops, so no report arrives after it.

**Hints.** A record becomes one `Changed` for each key of each cover that contains its path, and the
remainder of the path is `rel`. The covers are found with one table lookup for each directory above
the path that can be a cover. A record under no cover is dropped, because a stream path on a common
directory also reports what lies next to the covers. A directory record without `Created`, `Removed`
or `Renamed` is dropped. A record with `Mount` or `Unmount` is for a directory, although it does not
say so.

**Lost records.** A record with `MustScanSubDirs` for a path other than `/` becomes `Changed` with an
empty `rel` for each key of every cover that contains the path, and of every cover below the path.
If there is no such cover, there is no hint, because no key can see that place. A record for `/`,
and a record with only `UserDropped` or `KernelDropped`, becomes `Lost`. These hints come after the
other hints of the callback call. The covers are kept in the order of their paths (`Dirs`, an
ordered map), so the covers below a path are one search away. The search takes 0.2 to 0.3 µs for a
path with 50 to 100 covers below it, among 2,000 covers or 20,000. A flood of renamed directories
costs one search per record. A record finds the covers that contain it in the same map, in 75 ns
with 2,000 covers and 90 ns with 20,000, where a hash map took 65 to 75 ns. No second list of the
paths is kept. A look at every cover for every record took 5 to 10 s for 4,100 records and 4,100
covers in one call. A sorted copy of all cover paths, rebuilt after each change of the covers and
kept, was as fast as the map, and held every path a second time.

**`unwatch(key)`** forgets the key. The path stays in its stream until the streams are replaced, or
until no path of that stream has a cover at or below it any more. The stream is then stopped at the
next commit, with no loss report, since none of its records would reach a key. When no key is left
at all, the streams that a replacement left running are also stopped at the next commit, before
their time, so that nothing stays open.

**A cover is a text.** A stream is bound to the text of its path, and a record finds its cover by
the text of the cover. Both hold only while every name in them is the name that the disk has. Each
way in which a name can change has one place that detects it, and all of them respond in the same
way. The cover is reported lost, and the engine asks for it again, which resolves its real path
anew, and scans it deep. The four places are the following, and the paragraphs below describe
them:

- A directory at or above a stream path is detected by its guard.
- A directory inside a stream is detected by the record of its rename.
- A change between `watch` and the start of the stream is detected by the check after the start.
- A symbolic link that now leads elsewhere is detected by the poll (§2.14).

**The guard.** Each directory at and above a stream path, and each directory whose names a key asks
for and that no stream reports, is open for the vnode events of the kernel (`Guard`). The directory
is opened with `O_EVTONLY | O_SEARCH` and registered for `EVFILT_VNODE` with `NOTE_RENAME`,
`NOTE_DELETE` and `NOTE_REVOKE`, under an ID of its own, because a descriptor number can be reused
for another directory. One kernel queue serves the backend, and a dispatch source on the queue of
the streams reads it (`Notices`), so no thread waits for it. The source puts the IDs that fired into
a list, and its signal only wakes the engine. A full queue of the watcher drops the signal, never
the list. `translate` takes the list, and so does `commit`, which comes after the engine checked
everything for a dropped signal. Previously, the guards stayed on the old directories in that case
(`a_notice_whose_signal_was_dropped_is_taken_by_the_commit`). For each ID that fired:

- A key that asks for the names of a directory at or below the one that fired gets a hint for its
  directory.
- Each stream path at or below it is no longer trusted, and leaves the list of stream paths.
- Every cover at or below it is reported lost, even when the path is back by then, since the
  stream received nothing while its path was different.

The engine watches each cover again, which resolves its real path anew and starts a stream there,
and scans it deep. The stream that stood there is then idle, and the next commit stops it. The guard
that fired is closed, since another directory may now stand at its path. So is each guard below it,
because its directory left together with the one that fired, and no notice arrives on it. The next
commit opens a guard where one is still needed. When the guards below were kept open, they held the
old directories, and a rename of a new directory at their paths went unnoticed
(`a_directory_above_a_cover_is_replaced`).

- A volume mounted on a guarded directory, or above one, fires no guard, and no stream below it
  reports it. The kernel queue therefore also reports each volume that appears or disappears. Each
  guard whose open directory is then not the one at its path counts as fired
  (`a_volume_mounted_above_a_cover`). This is decided by device and inode, with one `stat` per
  guard. A volume inside a stream is reported by its record (below).
- At a commit in which the stream paths or the directories asked for names changed, the guards that
  the new streams need are opened before the streams start. A rename between the guard and the
  start is reported, and a rename before the guard is found by the check after the start. The
  guards that are no longer needed close at the end of the commit. The old stream paths keep their
  guards until new streams are in place, and permanently if none could start.
- The guards of all watchers of the process use at most half of the open files that the process may
  have (`getdtablesize`). That is 128 for a program started from a shell, which gets 256. Node
  requests far more when it starts. A directory that gets no guard, for this or another reason,
  gets no stream below it. The keys of the directories at and below it are listed as failed (the
  watch limit error, `ulimit -n`), and they are polled and asked again. The guards are opened from
  the top down, and none is opened below a directory that got none. The remaining room goes to
  complete paths downward, so the last guards to fit fail the fewest keys
  (`the_guards_that_fit_go_from_the_top_down`). A directory that is not present is not an error,
  but it gets no stream below it either. The keys at and below it are reported lost, since it may be
  back by then, with no guard to report its next rename. The engine checks them and watches them
  again, and the next commit opens the guard
  (`a_directory_away_when_its_guard_opens_is_checked_again`).
- As a result, nothing is polled on the way to a watched path, and an idle watcher has no timer
  (§2.10). The bench project with 20,000 watched files has 40 stream paths and about 50 guards.
- Before 2026-10-15 the real path of each stream path was requested at each commit (`realpath`). A
  poll of the directories above the covers triggered this once per second. It cost 1.6 ms per
  second for 40 stream paths, and 23 to 28 s for 20,000 `watch` calls of one file each
  (0.44 s now, measured side by side). A rename reverted within the second, with changes below in
  the meantime, went unnoticed. Before 2026-10-13 a guard ran alongside that poll, with a thread of
  its own, for the second it saved. It was removed.

**A stream that cannot start** fails every path of the commit, with the watch limit error. `commit`
lists the keys of their covers as failed, so that they are polled and asked again, and the old
streams remain.

**The check after the start.** The real path of a cover is requested at `watch`, and its stream
starts at `commit`. A directory above it that was renamed in between leaves the stream at a path
where nothing is. So after the start, the real path of each new stream path is requested again, in
the same way as before (`realpath`, 8 µs). A path that is now different is reported lost.

A directory between a new stream path and a cover below it can also receive another spelling of its
name in that interval, such as `src` to `Src`. The watched path stays valid, so no scan finds the
change, and no stream was there to report it. The cover would then stand at a path that no record
spells, and receive nothing. So after the start, each name between a new stream path and a cover
below it is read from the disk (`getattrlist`, 1 µs per directory, each directory once). A cover
with a name that is spelled differently, or that is now a symbolic link, is reported lost. Measured
with 20,000 watched files in 2,000 directories, this takes 2 ms of a setup of 50 ms. A watched tree
has nothing below its stream path and pays nothing.

**A path through the data volume.** `realpath` keeps a path spelled through `/System/Volumes/Data`,
where `/Users` and `/private` actually reside, but FSEvents reports the directory under its short
path, so such a cover received nothing. A real path with that prefix is therefore used without it,
when the short path is the same directory (device and inode) and is itself a real path. `/home` is
the same directory as `/System/Volumes/Data/home` too, but it is a symbolic link, and a stream at a
path that is not real is never trusted (the check after the start). That path therefore keeps its
long form. The directory itself is checked, not only its parent, because
`/System/Volumes/Data/Users` has a parent with no short path, and is `/Users`.

**A cover at another real path.** A key that is watched again and whose directory has a different
real path than before is reported lost if its old path still has a trusted stream. In that case a
directory on the old path was renamed and the notice of its guard has not been read yet, or a
symbolic link leads elsewhere. Without this, a key that asks for more at that moment would take the
new path with a scan of its own directory only. A key below a stream path that is no longer trusted
was already reported together with that path. The old cover of the key is removed as by `unwatch`,
so a stream that was to start for it alone does not start.

**Covers below a renamed directory.** A record for a directory renamed inside a stream, or for a
volume that appeared or disappeared there (`a_volume_mounted_inside_a_stream`), is also a loss
report for every cover below the path of the record. The engine finds such a cover by itself when
the renamed directory is a node of its tree. It is not a node when a symbolic link leads to the
cover from elsewhere.

#### Open issues

- A symbolic link on the way to a watched path that now points elsewhere is found by the poll
  (§2.14), within a second.
- A stream path that is no longer trusted keeps its stream until the streams are replaced, unless
  no path of that stream is trusted any more. In that case the stream is stopped at the next
  commit. While it keeps running and its directory is back, both that stream and the new one
  report the directory. A record then arrives twice, which costs one more table lookup and nothing
  else.
- When a directory inside a stream gets another spelling of its name, each cover below it is
  scanned deep twice: once for the record of the rename, and once because its real path has
  changed. The scans are paced, and only what differs is reported. The second loss report cannot be
  removed. A cover whose path changes has missed the records that spelled the new path, whatever
  caused the change, and the backend does not know that a scan is on its way.

### 4.3 ReadDirectoryChangesW (`backend/windows.rs`)

The tests of this backend pass on Windows 11 with NTFS. They are `tests/windows/os.rs`, and
`tests/windows/parts.rs` for the parts that make no Windows call (`place`, `Handles`, `Records`,
`translate`).

**The port.** One thread owns one I/O completion port and every directory handle (`port`). A handle
reports its directory and everything below it. A request that needs a new handle is answered at
once, and the handle is opened by the next `commit`, together with all the requests of a flush.
`place` is therefore called once per flush, not once per directory. For 2,000 directories this
took 10 ms instead of 350 ms (measured).

**Where the handles go** (`place`). Each directory that wants `Contents`, with no other such
directory above it, gets a handle. The handle serves every key at or below its directory. With more
than 64 handles, handles move up to a directory that they have in common and merge into one, the
deepest common directory first, until at most 64 remain. No handle is placed on `C:\`, `C:\Users`
or `C:\Users\<name>`. A handle reports nothing behind a junction or a directory link below it. A
directory with a link on the way down from the common directory is therefore not merged there, and
neither is any directory it was merged with deeper down. It keeps a handle of its own, which can
stand below the shared one, and the others share without it, including the ones on either side of
it in path order. A key goes to the deepest handle at or above its directory. The links are found
where a key would come to lie behind one (`System::is_link`, one look for each name on the way,
once per settle), and the placement is computed again with them known, before any handle moves.

**Answers.**

- A key that wants the contents and is served by a handle gets `WatchedTree`.
- A key that wants names only and is served by a handle gets `Watched`. `WatchedTree` would make
  the engine rely on it for the directories below, and only a key that wants the contents holds a
  handle.
- A `Names` request that no handle covers, and a directory that could only be watched from one of
  those three directories, get `NotWatched`. When a handle later comes to stand at or above such a
  directory, `commit` returns its key.
- A key to which the commit can give no handle (no room and no permitted common directory, or the
  open fails) gets `Changed` with an empty `rel`, and the engine asks again. That request is
  answered `Failed` with the reason, or `NotWatched` when the handle that came to serve it stands
  above a link.

**Moving the handles.** `Handles` keeps every served key. When a key is added or removed, the
handles move to where `place` puts them, once at the next `commit` for all the keys of a flush.
`unwatch` only closes a handle that no key uses any more (rule 6, which removes the OS watch with
its last key), and the engine commits after every flush that released a watch, also one that queued
no scan. New handles are opened first, and then old ones are closed. A key that received another
handle is not notified. The new handle was reporting before the old one stopped. The old one, whose
records wait in the kernel between two reads, gets one read queued after the close, which collects
them, and is then closed. Whatever comes later is reported by the new handle, so a close completes
even during a flood. The read that the close cancels fails with `ERROR_OPERATION_ABORTED`, so it is
not mistaken for lost records. A loss in the kernel (`ERROR_NOTIFY_ENUM_DIR`) on that read counts
as one, as on any other read. The call of the backend returns when the closes are done, so a
directory above a closed handle can then be renamed. If one open fails, nothing changes. A handle
that is still used only by keys wanting names closes with the last of them.

A `watch` for the very directory of a handle places a new handle there, because the old one may
stand on a directory that was renamed away. This does not happen when the key was only waiting for
a handle, or received none at the last commit. A handle whose read fails is abandoned, and its keys
are notified. When the next handle of a key also dies before it delivered anything, the request
after that is answered `Failed` with the error of the read. The directory is then polled, and tried
again at each poll. The deaths are counted per key, since a shared handle stands on a directory of
no key. The tables of the keys shrink when most of their keys are gone, as the tables of inotify do
(§4.1).

**Reads.** The filter is `FILE_NAME | DIR_NAME | SIZE | LAST_WRITE`, and the buffer is 64 KiB. The
bytes of a completed read are parsed on the watcher thread (`Records`, `translate`). The raw
notification holds the bytes and shares the path of the handle. The path of each record, with its
lower-case spelling, is built in a buffer that `translate` keeps from one record to the next, so a
flood of records allocates no memory. What the records of a read have in common is computed once
(`Handles::route_below`): the path of the handle in lower case, and the keys at and above it. Each
record then costs one lookup for each of its own names, by text. This took 0.53 µs per record in a
flood of 63,000, where a walk up the whole path of each record in lower case took 2.1 µs
(2026-10-22). The reads request the extended records (`ReadDirectoryChangesExW`, Windows 10 1709
and later) where Windows and the disk support them, because these records indicate whether an entry
is a directory.

**Hints.** A record goes to every key whose directory contains its path, the deepest first, with
the names compared in lower case (rule 7). Two watched paths that are two spellings of one
directory both receive it, and so does each key above. The lower case is computed per character
(`char::to_lowercase`), as measured against NTFS. NTFS treats `οδοσ` as `ΟΔΟΣ`. It does not treat
`οδος` (with the final sigma that `str::to_lowercase` produces for a word), `STRASSE` or `STRAẞE`
as `straße`, the Kelvin sign as `k`, or `I` as `ı`. The fold treats `STRAẞE` and the Kelvin sign as
equal to their counterparts, which costs one check too many and misses no record. A key above
receives a hint that it may not need, which costs one walk. Without that hint the path would be
missed where the tree reaches it only from above, such as a second spelling of a name below, or the
other letter case of the drive, which is another top node of the tree.

A record that no key contains is dropped, and so is a record whose name is not the name of an entry
below the directory. Such a name is `.` or `..`, a root, a drive, or a name with a NUL or a `:`,
which starts a stream of a file. NTFS produces none of these, and a write to a stream of a file is
a `MODIFIED` record for the file (measured). A disk that another system filled, or a server, could
produce such a name, and a `..` or a drive would reach another directory.

Records become hints as follows:

| Record | Hint |
|---|---|
| `MODIFIED` | `Written`. It is dropped when it names a directory (the extended records indicate this) or the directory of a key, because Windows sends it whenever an entry in a directory changes, and that entry has a record of its own. |
| any other record | `Changed`. |
| a read with no bytes, or a record with no name | `Changed` with an empty `rel` for each key at or below the directory of the handle. |
| bytes that cannot be parsed | `Lost`. |
| a read that fails | The handle is closed, and `Changed` with an empty `rel` is sent for each of its keys. |

A short (8.3) name in a record is treated like any other name. Below a watched directory the engine
asks the disk how it spells a name that it does not know, and finds the entry under its long name. A
record that no key contains is dropped (`behavior.md`, "Known limitations").

**Pacing the reads.** The kernel completes a read as soon as one record is in it. After a read that
returned with less than half of the buffer, the next read is queued a millisecond later. While no
read is queued, the kernel keeps the records in a buffer of its own, as large as the backend's, and
the next read collects them all. Without this pause a flood costs one read, one packet and one
wake-up of the worker and of the watcher thread per record. In a measurement, the 63,000 records of
30,000 new files in an ignored directory took 93,000 reads and 2 s of CPU time. The first record of
a quiet handle is not delayed, since the read that waits for it returns at once. A read that
returned fuller is queued again at once, since the flood is heavy. A save producing 20,000 records
per second fills 2 KiB of the kernel's 64 KiB during the pause.

**A delivery** (rule 13, `read_empty`). Once the requested time has come, the worker queues a read
on every resting handle and takes packets until the port is empty, with no handle resting in the
meantime. It then sends `Raw::Delivered` with the time at which the last look at the port began,
and this becomes `Hint::Delivered`. A queued read completes as soon as one record is in it, within
the call that made the change. `FsRtlNotifyFullReportChange` completes the pending requests, and on
a handle bound to a port the packet goes straight into the port. A handle with no read queued keeps
its records for the next read. This was measured on Windows 11 with NTFS, with 60,000 writes from
the same thread, from another thread and with no read queued. The record of each write was in the
port, or came with the next read, when the write returned (2026-10-22). The worker waits for the
time on the port and not with a sleep, which would leave the records of the interval unattended.
The first attempt used a sleep, and an edit reported with no quiet time took 4.6 ms instead of
0.4 ms. A share on another machine receives its records from the server, so an answer can arrive
before them.

**Links.** Directories are compared by path text, and a handle reports nothing for what lies behind
a junction or a directory link below it. Each name between a handle and a key that it would serve
is therefore inspected (`link_between`). A key behind a link, below a directory that has a handle
anyway, is answered `NotWatched`, and the engine polls it. A directory behind a link that wants the
contents gets a handle of its own (above), so such a key never makes the handles move back and
forth.

### 4.4 Poll (`backend/poll.rs`)

`watch` answers `NotWatched`, and the other methods do nothing. See §2.10.

## 5. Known limitations

What a user observes of these is in `behavior.md`, "Known limitations". What the design leaves out
on purpose is in `design.md` §1. Version 1 also has no option for the one-second poll, and no
kqueue backend (FreeBSD uses polling).

The known limitations of the engine:

- Linux: every disk is assumed to distinguish two spellings of a name. On a disk that does not,
  such as a folder of a Mac or of Windows shared into the machine, a FAT or SMB disk, or a directory
  with the casefold flag, a watched path that differs from the disk only in the case of its ASCII
  letters still receives its events, because a hint for one name marks the other (§2.2). Below a
  watched directory the events can be wrong. A change made through another spelling of a name is
  reported under a name that the disk does not have, or not at all, and a rename that changes only
  the case leaves the old name known. This was measured with the Linux build on a folder shared
  from macOS, also with polling.
- A hint with a plain name never finds a registered name that is not plain, and vice versa (§2.2).
  APFS treats a few characters as equal to plain ones (measured): `ß` as `ss`, the long s as `s`,
  the Kelvin sign as `k`, a ligature such as `ﬁ` as its letters, and each of the 1,002 CJK
  compatibility ideographs as its ideograph. A watched path spelled with one of them, where the
  disk has the plain spelling, receives no events from an OS watch.
- Under polling, a young file larger than 16 MiB, and one that cannot be read, receives one
  `Update` when it is 2 s old, even when nothing changed, including a file written just before
  `watch` (§2.8).
- A change of a symbolic link on the way to a watched path, or of a watched path that is a symbolic
  link, is found only by the poll (§2.14). A link that is changed and changed back before that poll
  is found as it was, and what the path led to in between is never reported.
- Equal fingerprints (§2.8) hide a change where no `Written` hint arrives (FSEvents, polling). This
  affects a rewrite of the same size that restores the old modification time; a file that is
  removed and, within the delete delay, replaced by another of the same size and time on a file
  system that reuses inode numbers (ext4); and a second write of the same size within one step of a
  file system that counts time in steps between 20 ms and 1 s.
- Linux and Windows: a write through another hard link of a watched file, under a name in a
  directory that is not watched, is not reported. On NTFS the record names the entry that was
  written (measured).
- FSEvents and Windows: after `rm -r` of the directories above a watched path, the engine learns
  that an upper directory is gone only at the next poll.
- Where the OS watches are placed depends on what was watched and unwatched before, because a
  stream stays where it is until the streams are replaced. Nothing is missed.

The known limitations of the backends are described in §4.1 to §4.3. The Windows backend has only
been run on Windows 11 with NTFS. Android has never been run. It is configured to use the same code
as Linux, and is only type-checked.

## 6. Tests

**The simulator** (`tests/sim`, Unix hosts only). The fixtures are in `fixtures/`, and the tests
beside them. `SimFs` is an in-memory file system with inodes, a clock, and a mode that ignores
letter case. `SimBackend` emulates an OS watch API on it in three modes: by inode (inotify), by path
prefix with real paths (FSEvents), and by handle for a tree (Windows). A stream in the second mode
ends when a directory at or above its path is renamed or removed, and reports this at once, whereas
the real backend detects it at its next commit (§4.2). The test decides when each notification is
delivered: now, late, twice, or never, with or without the notice that it was lost. The disk can
change in the middle of an engine call. The backend keeps a log of every `watch` and `unwatch`
call, and the disk tells where an OS watch currently stands. `MOVE_SELF` of inotify and the shared
handles of Windows are not emulated.

**The two checks at every settle** (`fixtures/caught_up.rs`). Each time a test lets everything
settle (every notification delivered, every delay expired, and more than two polls), the fixtures
check two things, by rules that they implement independently:

1. Each fingerprint that the engine stored equals a fresh look at the disk.
2. The told set is the set of files that a walk of the disk finds from the watched paths.

Every story ends with this check (`caught_up`), or states why it cannot (`not_caught_up`). A batch
is sorted before it is compared, so no test depends on the order of the engine. An engine whose
deadline never passes fails the test rather than hanging it. **The invariants** of §2.1 are checked
after every engine call.

The test files of the simulator:

- The rows of the contract (`design.md` §2) are the `contract` stories of `tests/stories`, which
  run here in each mode and also with the poll backend.
- `check_table` has one test for each row of the table in §2.4. `baseline`, `young` and
  `delete_delay` cover §2.7 to §2.9. `unwatch` covers §2.7, `want` covers §2.10 and where the OS
  watches are placed, `filter` covers §2.11, and `spelling` covers §2.2.
- `links`, `moved_directories`, `failures`, `timers` and `regressions` hold every case that was once
  a way to miss a change, or to report one that did not happen, grouped by topic. `regressions`
  holds the cases that fit no topic.
- `fuzz` runs random file system operations mixed with deliveries, clock steps, `watch` and
  `unwatch` calls and failures, including a disk that stops answering in the middle of an engine
  call. A run emulates the handler by its own rules. When everything has settled, the handler must
  know exactly the files that the watched paths stand for, each with its current content, and the
  two checks must hold.
- `sim` tests the fixtures themselves, and that the checks above fail when they should.
- `allocations` sets budgets for how often the engine allocates memory. A test fails when a budget
  is exceeded.
- `cost` is not a test but a measurement. It prints what the engine costs for large cases (20,000
  watched files, a directory of 50,000 files, 100,000 hints, a polling round, `unwatch`), in time,
  allocations and bytes. Run it before and after a change of the engine and compare.

**The other tests** (`tests/`, and the `mod tests` of `src/`):

- Private functions, in a `mod tests` at the end of the file: `tree/mod.rs`, `path.rs`,
  `fs/apple.rs`, `fs/windows.rs`, `engine/events.rs`, `watcher.rs`, and helpers of the backends.
- The tree alone (`tests/tree.rs`): the arena, the table, and the path from a hint to a mark.
- The engine on a real directory with the poll backend and a clock that the test advances
  (`tests/poll.rs`). It covers a file whose time is ahead of the clock (§2.8; the test counts the
  reads), the rest after a poll (§2.13), the engine after its backend stopped (§2.10), and the hash
  of a file, that is, the limit and that nothing but a regular file is opened (§2.8).
- Each backend alone (`tests/inotify.rs`, `fsevents.rs`, `windows/os.rs`, each on its own OS).
  `windows/parts.rs` runs on Windows only. In `fsevents.rs` a test that makes many changes runs
  alone, and after the others where possible (`Turn`), because a flood makes FSEvents report
  losses, and a test running beside it was observed to receive one too. No test there starts when
  more than 50,000 files lie next to the test program (see the measurements in §4.2).
- The watcher thread (`tests/watcher.rs`): `watch`, `unwatch`, `close` and `drop` from every side,
  relative paths, an invalid path, the queue bound, and a backend that cannot start.
- The stories (`tests/stories`):
  - `contract`: the rows of the contract.
  - `projects`: what editors and large projects do.
  - `promises`: the promises of `behavior.md`. Its first lines say which test proves which.
  - `engine`: what the engine must get right beneath the promises, such as a batch of paths, a
    directory that becomes a watched path above others, files and symbolic links on the way, and
    directories that are renamed or replaced.
  - `operations`: every way in which a user writes, renames and links a file, a hard link, and two
    watchers in one process.

  Each story runs on the real disk, once with the backend of the OS and once with polling, both at
  the same time. A story that needs nothing beyond the vocabulary of `World` (files, directories,
  links, renames, `watch`, `unwatch`, `expect`) runs first on each simulated backend of
  `tests/sim`, where it takes no time and its end state is checked against the disk. A story that
  sets a delete delay skips polling, which has none (§2.9), and says so. On the real disk these
  tests mostly wait rather than compute. Measured on macOS with each test run alone, 34 s of the
  56 s are the quiet time of 300 ms after each `expect`. 17 s are the wait for the events (the
  debounce, the delete delay, the poll of a symbolic link once per second, and about 10 ms of the
  OS), 4 s are the changes that the tests make, and 0.2 s are the `watch` calls.

**The repository.**

```text
src/
  lib.rs                 the crate docs; re-exports the API
  config.rs  error.rs  event.rs
  watcher.rs             the thread and the queue; selects the backend
  backend/               mod.rs: the `Backend` trait, what the engine requires of an OS watch API;
                         inotify.rs  fsevents.rs  windows.rs  poll.rs: one backend each
  engine/                the engine: watched_paths, check, scan, os_watch, events,
                         and test_hooks, what the tests see of it
  tree/  fs/  path.rs
tests/
  stories/               the promises of `behavior.md`: each story on the real disk, and on the
                         simulated disk of `sim/` where its vocabulary allows
  sim/                   the engine on an in-memory disk (Unix hosts): what the real disk
                         cannot reproduce on demand, with a fuzzer and a cost program; the
                         disk and the backends are in `fixtures/`
  watcher.rs             the thread
  tree.rs                the tree of the engine, alone
  poll.rs                the poll backend on a real directory, with a clock the test advances,
                         and the hash of a file
  inotify.rs  fsevents.rs  windows/     each backend alone, on its OS; `windows/parts.rs`
                         holds the parts of that backend that make no Windows call
  common/                gate.rs: the gate in the sink that inotify.rs and windows/os.rs share
examples/                watch.rs, dev_server.rs
references/
  behavior.md                what it does and promises
  design.md                  the rules it follows
  architecture.md            how the code follows them
bench/                       the comparison with other watchers: a separate workspace,
                             with the tables of the last full runs in BENCHMARK.md
```

`pathwatch` exposes seven names: `Watcher`, `Config`, `Event`, `EventKind`, `EntryKind`, `Error`
and `ErrorKind`. Its other public modules are hidden from the documentation. They exist for the
tests and can change in any release. What only the tests call is built with the feature
`test-hooks`, which the tests enable themselves.

**How to run them.** The Rust toolchain is the one in `rust-toolchain.toml`.

```sh
cargo test --workspace
```

This runs the following on the machine where it is invoked:

- The engine on a simulated disk, with a fuzzer (`tests/sim`, on Unix hosts).
- The backend of this OS against the real watch API (`tests/inotify.rs`, `fsevents.rs` or
  `windows/os.rs`).
- The watcher thread (`tests/watcher.rs`).
- The tree alone (`tests/tree.rs`), and, on a real directory, the poll backend with a clock that
  the test advances and the hash of a file (`tests/poll.rs`).
- On Windows, also the parts of its backend that make no Windows call (`tests/windows/parts.rs`).
  Elsewhere the backend is only compiled.
- The stories (`tests/stories`). Each runs once with the backend of this OS and once with polling
  on the real disk. A story that needs nothing beyond files, directories, links and renames also
  runs on each simulated backend of `tests/sim`. The note above the promises in `behavior.md` says
  which of them run where. A story about the delete delay skips polling, which has none.

| Platform | How |
|---|---|
| macOS | `cargo test --workspace` |
| Linux | `cargo test --workspace`. From macOS with OrbStack: `orb bash -c 'source $HOME/.cargo/env; cd <this directory> && CARGO_TARGET_DIR=$HOME/pathwatch-target-inotify cargo test --workspace'`. The tests use the temporary directory of the VM, because inotify does not see changes that macOS makes on the shared folder. |
| Windows | `cargo test --workspace` on a Windows machine. Tested on Windows 11 with NTFS. From another OS only a type check is possible: `cargo check --workspace --all-targets --target x86_64-pc-windows-msvc`. |
| Android, WASI, FreeBSD, musl, 32-bit Linux | Only a lint of the crate and its tests, as on CI: `cargo clippy -p pathwatch --all-targets --target aarch64-linux-android`, and the same with `wasm32-wasip1-threads`, `x86_64-unknown-freebsd`, `x86_64-unknown-linux-musl` or `i686-unknown-linux-gnu`. The engine with the poll backend was run once under wasmtime on a preopened directory, as a probe, not as a test. |

The fuzzer (`tests/sim/fuzz.rs`):

```sh
# Quick run: 250 seeds per backend. Part of `cargo test --workspace`.
cargo test -p pathwatch --test sim fuzz::

# Long run, over a range of seeds of your choice. CI runs it on the seeds of the day.
PATHWATCH_FUZZ_FIRST=100000 PATHWATCH_FUZZ_SEEDS=20000 \
  cargo test --release -p pathwatch --test sim fuzz::big -- --ignored

# Replay one seed on every backend, with a short account of each failure. A failure of the
# `long` run also needs `PATHWATCH_FUZZ_STEPS=5000`.
PATHWATCH_FUZZ_SEED=1234 cargo test -p pathwatch --test sim fuzz::replay -- --ignored --nocapture
```

Other commands:

```sh
# The cost of the engine for large cases. Run it before and after a change and compare.
cargo test --release -p pathwatch --test sim cost:: -- --ignored --nocapture

# macOS: the cost of 20,000 files watched one by one in the FSEvents backend. Runs alone.
cargo test --release -p pathwatch --test fsevents the_cost_of_single_files -- --ignored --nocapture

# The FSEvents test that fills the stream limit of the process. Runs alone.
cargo test -p pathwatch --test fsevents -- --ignored failed_start

# The inotify tests of the watch limit and of a mount. They need a lowered limit and mounts of
# their own, and run sequentially, because they share the limit and see each other's mounts.
unshare -Urm sh -c 'echo 8 > /proc/sys/user/max_inotify_watches &&
  cargo test -p pathwatch --test inotify -- --ignored --test-threads=1'

# Lints and documentation.
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p pathwatch --no-deps
```

## 7. Glossary

The terms of the rules are defined in `design.md` §6. The code also uses the words below, each for
one thing only, in the order in which a change meets them.

- **Watched path.** A path given to `watch` and not yet unwatched. Its node `is_path`.
- **Registered.** A node that is a watched path or leads to one (`is_ancestor`). A registered node
  stays in the tree while nothing is at its path.
- **Key.** A `NodeKey`: the slot of a node in the arena, plus a generation. The backend knows a
  directory by its key and never by its path, so the two sides need not agree on a spelling.
- **Arm, narrow.** To arm is to ask the backend for the watch of a directory, because it wants more
  than was requested or because the watch may be gone. To narrow is to release a watch, because the
  directory wants less after an `unwatch`.
- **Cover.** A watch above covers a directory below it (`Watch::FromAbove`). In the FSEvents
  backend, a cover is the real directory that a key stands for.
- **Place.** Where the OS watches are put when there are more directories than the OS lets a
  process watch at once: the deepest directories that the paths have in common.
- **Mark.** A request to check a name at the next flush. A hint leaves one, and so does the engine
  when it has to look again. A mark records whether a directory is to be scanned deep, whether the
  file was written, and whether only a loss report requested it.
- **Flush.** The pass that checks every mark and then runs the scans, round after round, until
  nothing is queued. When it is over, `finish` reads the events off the tree.
- **Look.** One read of the disk for one path: an `lstat`, a `stat`, or a listing.
- **Check.** The look at one node, and what the engine does with the result (the table of §2.4).
- **Scan.** The check of every node of one directory, with a listing when the node is complete. The
  depths are `Names`, `Shallow` and `Deep` (§2.5).
- **Appear, still there, vanish.** A directory is found where the tree had none; a directory is
  still the same directory; a directory is gone, with everything below it.
- **Anchor.** In `register`, the deepest node on the way to a new watched path that has only
  directories above it on disk. The first look is made there.
- **Plain.** A name consisting only of ASCII characters and Han ideographs, so that its other
  spellings differ only in the case of its ASCII letters, and the table of the tree finds all of
  them (§2.2).
- **Twin.** Another node of the same directory whose name differs from this one only in the case of
  ASCII letters.
- **Frozen.** A node below a held node. Nothing is done there before the second look above.
- **Uncover, let go.** A node is no longer a watched path and no watched directory covers it, so
  what the tree holds there is forgotten. A node that is no longer registered becomes an entry like
  any other, or is freed.
- **Settle** (tests). Deliver every notification and run every deadline until nothing is left.
