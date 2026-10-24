# pathwatch design

This document is the foundation of `pathwatch`: the smallest set of rules that every
implementation must follow. `behavior.md`, which describes what a user observes, and
`architecture.md`, which describes how the code works, must agree with it. A change that breaks a
rule is a bug; a change of a rule is a change of the design.

## 1. Purpose

`pathwatch` watches **paths** and reports **file events**: `Create`, `Update` and `Delete`. It
never reports a directory. A watched path can name a file, a directory, or nothing yet.

Out of scope: events for directories; rename pairs; a report of what exists at `watch` time, and of
changes made before the call; globs; a depth limit; following symbolic links inside a watched
directory.

## 2. The contract

Three definitions specify the whole contract.

1. **A watched path stands for a set of files.** If a file is at the path: that file. If a
   directory is there: every file below it, at any depth, except what the ignore filter excludes.
   If nothing is there: no file. A file is anything that is not a directory. A watched path that
   is a symbolic link, or passes through one, is followed; a symbolic link below a watched
   directory is a file.
2. **Events are the difference between the told set and the disk.** The **told set** is what the
   handler believes exists: the baseline of each `watch`, plus every `Create`, minus every
   `Delete`. Once the watcher has caught up, the told set equals the set of files that the watched
   paths stand for.
3. **A batch has at most one event per path.** `Create`: not told, and a file is there. `Delete`:
   told, and no file is there. `Update`: told, a file is there, and it is a different file or may
   have different content. Hence the last event for a path gives its state.

Extra `Update` events are allowed; a missed change is not.

The table applies these rules to examples. `R` is a watched path.

| # | What happens | Events |
|---|---|---|
| S1 | in-place write to the file `R` | `Update R` |
| S2 | atomic save of `R` (write a temporary file, rename it over `R`) | `Update R` |
| S3 | `R` deleted and created again | `Delete R`, `Create R`. Within the delete delay: `Update R`, or nothing when the same file is put back by a rename |
| S4 | `R` missing; a file is created there, then written | `Create R`, `Update R` |
| S5 | `R` missing; a directory is created there, then a file inside it | `Create R/f` |
| S6 | `R` and several directories above it missing; `mkdir -p` and a file at `R` | `Create R` |
| S7 | the file `R` deleted; a directory with files appears at `R` | `Delete R`, `Create R/x …` |
| S8 | the directory `R` removed; a file created at `R` | `Delete R/x …`, `Create R` |
| S9 | a new file in the directory `R`; `mkdir` and a file written at once | `Create R/f`; `Create R/sub/f` |
| S10 | a tree moved into the directory `R` | `Create` for each file |
| S11 | a subdirectory moved out of `R`, or removed with `rm -r` | `Delete` for each told file |
| S12 | the directory `R` removed or renamed away, then restored | `Delete` for each told file, then `Create` for each file. Within the delete delay: only what differs |
| S13 | the file `R = proj/lib/a.js`; `mv proj/lib proj/lib2`, and back | `Delete R`; `Create R` |
| S14 | an ignored directory below `R` | nothing from inside it; renamed from an ignored to a visible name: `Create` for each file; from a visible to an ignored name: `Delete` for each told file |

## 3. Principles

The rules of sections 4 and 5 follow from seven principles.

1. **The disk is the truth.** An OS notification says where something may have changed, never
   what changed. Every event comes from comparing a look at the disk with the told set. A
   notification that arrives twice, or late, costs one look and changes nothing.
2. **Watch, then read.** A directory is read only after a watch over it is live. A change before
   the watch is found by the read; a change after it produces a notification. No change can fall
   between the two.
3. **Absence must be confirmed.** A look that gets no answer from the disk changes nothing and is
   repeated. A told file is reported gone only when a second look, one delete delay after the
   first, still finds nothing, so a file that a tool removes and writes again is not reported as
   deleted.
4. **When in doubt, report `Update`.** Equal metadata does not prove equal content: two writes of
   the same size can share one timestamp. Where a change cannot be ruled out, the file is reported
   as updated.
5. **The backend decides when, not what.** The OS watch decides how soon a change is found, never
   which events are reported: every backend, and polling, must produce the same events. The
   ignore filter and the spelling rules are therefore applied by the engine, and a backend never
   sees them.
6. **Nothing is left unobserved.** A directory that no OS watch covers is polled: when the OS does
   not watch it, when its watch failed, and when the backend cannot start or has stopped. An error
   is reported, and the watcher keeps running.
7. **Cost is bounded.** Work that no notification requested by name, a poll or the rescans after
   lost notifications, is paced by the time it took. A continuous stream of notifications cannot
   postpone a batch indefinitely.

## 4. Rules for the engine

The **engine** keeps the tree, receives the hints of a backend, reads the disk, and delivers the
events.

### 4.1 The tree

- The tree is a sparse copy of the directory tree. **One node is one name**, and the node records
  what is at that name: nothing, a file, or a directory. A path that changes its kind changes its
  node; nothing moves to another place.
- A **registered name** is a watched path or a name on the way to one. Its node remains while
  nothing is on disk at its path.
- A directory is **complete**, and the engine tracks every entry of it, when its path is a watched
  path or its parent is complete, provided in both cases that the filter does not ignore it as a
  directory. Every other directory on the way to a watched path is **partial**: the engine tracks
  only its registered names. Nothing next to a watched path is tracked or reported.

### 4.2 From hint to event

- A backend turns each OS notification into **hints**. A hint names a place to check: a directory
  that the engine asked the backend to watch, identified by its key, and a path relative to it. A
  hint never says what happened. A hint for a directory requests a check of everything below it.
- A **loss report** says that notifications were lost, at and below one directory or everywhere.
  The engine checks that place, watches it again, and scans everything below it.
- Hints leave **marks** in the tree. Once no hint has arrived for the quiet time (4.11), the engine
  **flushes**: it looks at the disk at each marked place, from the top down, compares the result
  with the tree, and delivers the events as one batch.
- A **look** is one read of the disk for one path. A **check** is the look at one node and the
  update of that node. A **scan** checks every node of one directory, with a listing when the
  directory is complete. A check never descends into a directory: it requests the watch of the
  directory and queues a scan, which runs after the watch is live (principle 2).
- A hint for a place that the tree does not track is dropped. A hint from deep inside a place the
  tree does not know (a tree that was moved in, an ignored directory) ends at the highest unknown
  name.

### 4.3 Events

The event of a node is decided at the end of a flush, from two facts: whether the handler was told
about a file at the node, and whether a file is there now.

| Told | A file is there now | Event |
|---|---|---|
| yes | no | `Delete` |
| no | yes | `Create`; none if the file is part of the baseline of the running `watch` (4.4) |
| yes | yes, and it changed (4.6) | `Update` |
| otherwise | | none |

- Whatever happened within one flush needs no special rule: a file that appeared and disappeared
  produces nothing, and a file that disappeared and reappeared produces one `Update`.
- A change of kind is never an `Update`, and it does not wait: the `Delete` of the file, or of
  every file below the directory, is in the same batch as what replaced it.
- A directory that appears produces a `Create` for each file below it, and a directory that is
  gone a `Delete` for each told file below it, in one batch.

### 4.4 `watch`, the baseline, and `unwatch`

- `watch` registers a batch of paths, looks at them, and returns once the state of every path is
  known. A path that is already watched is skipped. The whole batch is registered before the look,
  and each node is checked once per flush.
- **The baseline.** What `watch` finds is not reported. But a file that should have been reported
  before this `watch` is not silenced: an older watched path or an older complete directory
  already covered it. A complete directory whose listing failed (no permission, or another program
  held the directory) covered nothing, so a path watched below it later reports what it finds as
  new. Silence therefore applies only to:
  - a file at a path that this `watch` made a watched path, in a directory that was not complete
    before;
  - every entry of a directory that became complete during this `watch`.
- A reported write that was on its way when `watch` looked produces an `Update` for a file that the
  baseline has just seen: a reported write counts as a change (4.6), and nothing shows whether it
  preceded the look.
- `unwatch` undoes a `watch` of exactly the same paths, and reports nothing. What only this path
  covered is forgotten: the file at it, or every file below it down to the next watched path. The
  handler is not told, even when a `Delete` of the file was pending. Whether a file is covered is
  decided by the rule of 4.1, from the paths alone, without a look at the disk. What another
  watched path still covers stays as it is.
- The one exception: a symbolic link inside a watched directory is followed only while it is
  registered. After `unwatch` it is a file entry again, and its check can report a change (4.9).
- A path that is watched again later gets a new baseline.

### 4.5 Absence and the delete delay

Tools replace a file by removing it and writing it again, so a `Delete` is not reported at once.

- The first look that finds nothing where the tree has a file or a directory **holds** the node.
  Nothing else changes: the fingerprint stays, and below a directory every node stays as it is.
  No hint goes into a held directory, and nothing below it is checked or scanned.
- **The second look** comes one delete delay after the hint that requested the first look, so that
  the quiet time before the first look does not add to the delay; when no hint requested the first
  look (a poll, a scan), one delete delay after that look. A first look that comes later than that
  gets its second look in the next flush. The second look happens even when no hint arrives, and
  also under polling.
- If the second look still finds nothing, a file is reported as a `Delete`, and a directory as a
  `Delete` for each told file below it, in one batch. If the disk gives no answer, the hold stays
  until it does: an error that does not establish whether the path exists is never a `Delete`.
- If the path is back, a file is compared with the stored fingerprint: `Update` if it differs,
  nothing if it is the same file. A directory is watched again and scanned deep, and each file below
  it is compared in the same way.
- A change of kind ends the hold at once. A file that was not yet told is not held: when it is
  gone, it is forgotten. If a node above a held node is also found missing, the upper node decides
  for both.
- With a delete delay of zero nothing is held: a `Delete` comes at the first look.

### 4.6 Fingerprints and young files

- The **fingerprint** of a file is its modification time at full resolution, its size, and its file
  id. Metadata that changes without the content is not part of it: the change time, which `chmod`,
  a rename, a new hard link and an xattr write all change.
- The **identity** of a directory tells it apart from another directory created later at the same
  path: its device, its file id, and its creation time. The identity is never a reason to skip a
  check that a hint requests: a directory that was moved away and back keeps its identity.
- A told file has changed when its fingerprint differs, when the backend reported a write to it,
  or when it is young and its content may differ.

Two writes of the same size can share one timestamp, and equal fingerprints then hide a change.
Two guards close this gap:

1. **Reported writes.** A backend whose notifications correspond one to one to writes reports each
   write, and the engine reports `Update` without consulting the fingerprint.
2. **Young files.** A file is **young** while its modification time is so recent that another
   write could leave its fingerprint unchanged, or while that time lies in the future. For a young
   file an equal fingerprint is no proof. Either its content is compared, or, where every write
   produces a hint, the absence of a hint is the proof; but only once every notification from the
   time the file was young has arrived. Until then, a look that may have missed a hint counts the
   file as written. A file whose content cannot be compared (too large, or unreadable) is counted
   as changed at the latest by the first look that finds it old.

Youth is measured by the watcher's clock, not by the disk's. A file whose modification time lies
far ahead of the clock is young for a bounded time after the first look that found its current
fingerprint, and old after that. A symbolic link entry is never opened: its content is the text
of its target.

### 4.7 Spelling

Some disks find an entry under more than one spelling of its name: `App.js` finds the entry
`app.js`, and two Unicode forms of a name find one entry. A directory on such a disk is **loose**;
a directory that distinguishes the spellings is **strict**. One rule decides every case:

> `pathwatch` reports what a program would see if it asked the OS about each watched path on its
> own, again and again: `lstat` for a watched file, the listing for a watched directory.

Hence there are two kinds of names. A **registered name** is the text that its caller gave. What
is at it is whatever `lstat` finds there under any spelling, and its events carry the caller's
spelling. A **listed name** (found below a watched directory) is the name as the disk spells it,
and each entry has exactly one such name. Each watched path is an independent question: two
watched paths that are two spellings of one file both receive its events, and a watched path in
another spelling inside a watched directory receives its events alongside those that the
directory reports under the disk's name. A symbolic link is treated the same way: a registered
link is followed, a listed one is a file. Polling behaves this way by construction, and the OS
watches must produce the same events.

- **Loose or strict** is a property of the watcher, not a question asked of each directory: a
  watcher treats every directory as loose, or every directory as strict.
- **A hint applies to every name it can denote**: to every registered name that may be another
  spelling of the name in the hint, and, in a complete directory, to every name that differs from
  it only in letter case. The look at each of them decides.
- **When the disk spells a name differently**, the directory has no entry of that name. A name
  that is not registered is gone at once, without a hold, because a second look would find the
  entry again. A registered name remains the caller's path, but the directory above does not cover
  it (the name is **unlisted**): a watched path reports what is at it as before, and a name that
  only leads to a watched path has nothing of its own. The name that the disk gave is looked at in
  the same flush.
- The order of two looks within one flush must not affect the result.

### 4.8 The ignore filter

`ignored(path, kind)`, with `kind` either `File` or `Dir`. Only the engine asks it; a backend never
sees the filter, so the result is the same on every backend.

- **The filter's contract.** It returns the same answer for the same path and kind. It should
  ignore everything below a directory that it ignores (a **closed** filter). A filter that
  inspects only the last component is allowed, and gives the same answers as a closed one for
  everything below an ignored directory: `/p/node_modules/pkg/x.js` is never reported, whether `/p`
  or the file itself is watched, because `node_modules` lies on its path.
- **The kind is always known.** It comes from `lstat` or from a listing. An entry that is gone is
  never asked about: what the tree stored decides. The path has the registered spelling.
- **The answer is kept with the node**, one per kind. An entry is therefore asked about once when
  it is found and again when its kind changes, not at every check. An ignored entry inside a
  watched directory keeps no node, so each listing of its directory asks about it again.

Three things are asked about:

1. **An entry inside a watched directory.** An ignored entry is missing. An ignored directory is
   never listed or watched, and nothing below it is asked about. A directory that disappears is
   decided at the directory: one `Delete` for each told file, with no question. A directory that
   appears is asked about once, and then each entry in it once.
2. **The directories above a path given to `watch`**, as directories, from the top down and whether
   or not they exist, so that nothing is asked below an ignored one. If one is ignored, the path can
   never be reported: it costs no watch, no look and no poll, and an `unwatch` of it does nothing.
   The spelling of a watched path never changes, so this is asked once. A directory on the way to a
   watched path is therefore never ignored.
3. **The path itself**, for both kinds, whatever is on disk. If both kinds are ignored, the path is
   skipped in the same way. Every other path becomes a watched path. When it exists, it is asked
   about with its kind like every entry. While the answer is "ignored", it counts as missing:
   nothing is listed or watched for it, and it is asked again when its kind changes.

**No event comes from the filter alone.** A watched path that counts as missing was never reported,
so nothing is deleted when it disappears. A file that replaces an ignored directory at a watched
path has appeared on the disk: a `Create`. The files of a watched directory that an ignored file
replaces are gone from the disk: one `Delete` each.

### 4.9 Symbolic links

- A symbolic link **inside a watched directory** that is not registered is a file entry. It is
  never followed and never opened.
- A **registered name** that is a symbolic link is followed. Event paths keep the registered
  spelling.
- When what lies behind a registered link changes (link ↔ no link, or another real path) and the
  directory is still the same one, the directory is watched again and scanned deep: otherwise a
  watch bound to a path would be dead. If the look behind the link gets no answer, nothing changes.
- A tree watch does not report what lies behind a symbolic link, so a directory reached through a
  link is watched on its own.
- Nothing watches a symbolic link itself: every poll checks each registered name that is a link.
- When `watch` places a watched path at or behind a link that was not followed until then, what
  lies behind the link becomes the baseline of that `watch`. After `unwatch` the link is a file
  entry again (4.4).

### 4.10 Errors from the disk

Every failed look falls into one of two classes.

- **Missing**: the disk establishes that no entry is there (not found, not a directory). An entry
  that cannot be read at all is also missing for events, and its error is reported once: a path
  that the OS cannot represent, an entry the watcher may not read, a symbolic link loop. Its node
  is polled, so a later change is found. An entry that is being deleted counts as missing, and is
  looked at again in each flush until the name is gone or the file is there, since the deletion can
  be cancelled without a notification.
- **No answer**: anything that does not establish whether the entry exists. The tree stays as it
  is, no `Delete` is told, the mark stays for the next flush, and the node is polled until a scan
  of it succeeds. The error is reported once, unless another program merely holds the entry for a
  moment: nothing is wrong with the disk then.

Further rules:

- A failed watch is reported in the same way, except when the directory is gone: the scan that
  follows finds it missing. A lack of room for one more watch is reported once for the lifetime of
  the engine, not once per directory.
- At most one error of each kind is delivered per flush.
- **A disk that other machines also change.** Where an OS watch cannot see the changes that other
  machines make, the watcher reports this once per disk, also for two such disks found by one
  flush, and does not poll on its own: polling a large tree on a slow disk can cost more than the
  changes are worth, and only the user can judge that. A disk that is polled anyway is not
  reported.

### 4.11 Time

- **The quiet time.** The disk is checked when no hint has arrived for the quiet time: every hint
  that leaves work postpones the flush to one quiet time after it, so a save in several steps
  produces one batch. A continuous stream of hints does not delay a batch forever: counted from the
  first hint after the previous flush, the flush comes within a fixed bound.
- The marks of hints wait for the quiet time, and nothing else takes them: the quiet time can be
  longer than the poll interval, and a poll or a second look that runs before it checks its own
  work and leaves those marks. There are two exceptions, each because a look is made anyway: a
  `watch` or `unwatch` call ends the quiet time, since its baseline must be decided by a tree that
  is in line with the hints; and a hint for a directory above a held node is checked with the
  second look of that node, since the directory decides for it. A check made by a scan takes the
  mark waiting on its node with it: the look is made now, and what the hint said about the node
  still applies. None of this affects the delete delay: it counts from the hint, or from the look
  that found nothing.
- **Pacing.** Two kinds of work are requested by no hint that names a path, and can recur
  indefinitely: a poll, and the scans after a loss report. After the scans of the loss reports of
  one flush, the next such scans rest for a time that grows with how long they took, up to a bound;
  a mark requested only by a loss report waits until then, and a hint that names its path releases
  it. The next poll starts one poll interval after the end of the current one, and at least one
  round's duration later, so that polling uses at most half of the time; a `watch` call in between
  does not move it. The time counted is that of the whole flush in which the poll or the scans ran,
  up to a bound, since the clock keeps running while the machine sleeps.
- Notifications that the watcher cannot hold are lost notifications: a loss report for everything.

### 4.12 What a directory asks of the backend

- **The want rule.** A directory node asks the backend for `Contents` if the node is complete, or
  if one of its registered names has a file or nothing at it: a file there can be written, and
  where nothing is, a file can appear, or a directory with everything below it at once. Otherwise
  it asks for `Names`: every registered name in it is a directory, and such a directory requests a
  watch of its own. Hence `watch("/home/me/project")` puts a watch on the project, not on the home
  directory with everything else in it.
- When directories above a watched path are missing, the lowest existing directory is therefore
  asked for `Contents`. A backend that watches whole trees then finds `mkdir -p` together with a
  file without a poll.
- No file is looked at before its watch is live (principle 2).
- For one directory the want only increases. It decreases in one way only: when an `unwatch` leaves
  a directory whose registered names are all directories on disk, its watch is reduced to names.
- A directory that no OS watch covers is **polled**: a timer scans it. With an OS backend the timer
  covers few directories; with polling it covers every directory, at the poll interval. **Polling
  is this timer with a backend that watches nothing.** The timer runs only while there is
  something to poll, so an idle watcher with an OS backend sleeps until a notification arrives.
- A backend's refusal to watch a directory is not permanent: when an OS watch later comes to cover
  it, the directory is asked for again and is no longer polled.
- If the backend reports that it cannot continue, the error is reported once, every directory is
  polled, and everything is scanned. From then on the watcher behaves as one that polls everything.

## 5. Rules for a backend

A backend sets up OS watches when the engine asks, and turns raw OS notifications into hints.

**Vocabulary.**

- A **key** identifies one directory of the engine. The backend puts it into every hint for that
  directory, so no path spelling has to match between the two sides.
- A **want** is what the engine asks about a directory (4.12). `Names`: report when a name in the
  directory appears, disappears or is replaced. `Contents`: report that, and also writes to the
  files in the directory.
- A **coverage** is the answer to a watch request. `Watched`: changes in this directory produce
  hints. `WatchedTree`: changes in this directory and in every real directory below it produce
  hints, and the engine does not ask for the directories below. `NotWatched`: the backend does not
  watch this directory, and the engine polls it. `Failed`: the backend tried and failed; the engine
  reports the error, polls, and asks again later.
- A **commit** makes every watch requested since the previous commit live. It names the keys whose
  watch could not be made live, and the keys that received `NotWatched` and that an OS watch now
  covers.
- The **hints**:
  - `Changed { dir, rel }`: the entry at `rel` below `dir` appeared, disappeared or was replaced.
    If it is a directory, anything below it may also have changed. An empty `rel` is a loss report
    for `dir` itself: the watch is gone, the directory was moved away, or notifications were lost.
  - `Written { dir, rel }`: the content of the file at `rel` was written. For a directory it means
    nothing.
  - `Lost`: notifications were lost, and the backend cannot say where. Check everything.
  - `Mount { path }`: a volume was mounted or unmounted at the real path `path`, and the backend
    knows no key for it.
  - `Delivered { before }`: every notification of a change made before `before`, by the system
    clock, has arrived ahead of this hint. It is the answer to a delivery request (rule 13).

**The rules.** Other sections refer to a rule by its number.

1. **One sequence.** Every request of the engine, and the translation of every raw notification,
   happens in one sequence, on the engine's side. A backend's own threads only queue raw
   notifications.
2. **Watch, commit, scan.** The engine reads a directory only after `watch` and then `commit`. A
   watch is therefore live when `commit` returns, unless `commit` listed its key, and `commit`
   opens no gap in a watch that is live.
3. **`watch` again** for a key means that the watch must stand on the directory that is at `dir`
   now. If that requires a new OS watch, the new one is live before the old one is removed.
4. **The answer is the truth.** The coverage states which changes produce hints from now on. After
   `NotWatched` or `Failed` nothing is watched for the key. A `commit` can make an OS watch report a
   directory that previously received `NotWatched`: `commit` then returns its key, and the engine
   asks for it again.
5. **Want.** For one key the want only increases, until `unwatch(key)`. More hints than requested
   are acceptable; fewer are not.
6. **Keys.** One OS watch can serve several keys: two spellings of one directory, or a renamed
   directory under its old and its new key. The OS watch is removed with its last key.
7. **One hint for each key** that can see the change, with `rel` relative to the `dir` given with
   that key. A backend whose OS reports real paths maps them back.
8. **Where, not what.** `Changed` for a directory makes the engine scan everything below it. A
   notice that only says "this directory was touched" is therefore dropped, or sent as `Written`.
9. **`Written` only when the OS reports each event separately.** An OS that merges or repeats its
   flags never sends it.
10. **A dead watch.** If the OS dropped the watch, forget it. If the directory was moved, keep the
    watch and its keys. In both cases send `Changed` with an empty `rel` for each of its keys. A
    watch bound to the text of a path is dead as soon as a directory at or above that path is
    renamed or removed, and it does not report this itself: the backend has to detect it, at once
    and also when the directory is back a moment later. A key that is watched again and whose
    directory has a different real path than before gets the same hint: its old watch was dead for
    a while.
11. **Lost notifications.** If the OS names the place, send `Changed` for each key of it, with an
    empty `rel` or the `rel` of a place below; the watch remains. If it names no place, or its data
    cannot be read, send `Lost`.
12. **The end.** A backend that cannot continue says so once. Dropping the backend stops and joins
    its threads, and nothing is sent after that.
13. **A delivery.** When asked about a time, the backend drains the queue of the OS once that time
    has come, and sends `Delivered` with the time at which the last read began, behind everything
    it read. Every notification of a change before that time is therefore ahead of it, and so is
    the loss report of such a change. This holds only for an OS that puts a notification, and a
    loss report, into its queue before the call that made the change returns. A later request
    replaces one that has not been answered yet.

## 6. Terms

- **Watched path.** A path given to `watch` and not yet unwatched.
- **Registered name.** A watched path, or a name on the way to one (4.1).
- **Listed name.** A name found below a watched directory, as the disk spells it (4.7).
- **Complete, partial.** A directory that the engine tracks entry by entry, or only for its
  registered names (4.1).
- **Told set.** The files that the handler believes exist (section 2). A node is **told** when the
  handler believes that a file is at it.
- **Baseline.** What a `watch` call finds: known to the handler without an event (4.4).
- **Hint.** What a backend derives from a raw notification: where to look, or that notifications
  were lost. A hint never says what happened.
- **Loss report.** A hint that notifications were lost, at and below one directory or everywhere.
- **Mark.** "Check this name at the next flush." A hint leaves one, and so does the engine when it
  has to look again.
- **Flush.** The pass that checks every mark, then scans what the checks queued, and delivers the
  events as one batch.
- **Look, check, scan.** One read of the disk for one path; the look at one node and the update of
  that node; the check of every node of one directory (4.2).
- **Held.** A look found nothing where the tree has a file or a directory. The node waits for a
  second look, one delete delay later, and only that look decides (4.5).
- **Fingerprint.** The modification time, the size and the id of a file: what distinguishes one
  state of it from the next without reading it (4.6).
- **Young.** A file whose modification time is so recent that another write could leave its
  fingerprint unchanged (4.6).
- **Quiet time.** How long no hint may arrive before the disk is checked (4.11).
- **Delete delay.** The time from the hint to the second look at a path found missing (4.5).
- **Polled.** No OS watch covers the directory, so a timer scans it (4.12).
- **Want, coverage.** What a directory asks the backend to report, and the backend's answer
  (section 5).
- **Loose, strict.** A directory where two spellings of a name (letter case, Unicode form) are one
  name, or two names (4.7).
- **Unlisted.** A registered name whose entry the directory above lists under another spelling. It
  is the path of its caller, and not an entry of that directory (4.7).
- **Rest.** The pause after work that no hint requested by name: the scans after a loss report, or
  a poll (4.11).
