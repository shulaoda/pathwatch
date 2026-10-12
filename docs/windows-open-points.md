# The Windows backend: open points

The Windows backend (`crates/pathwatch/src/windows.rs`) was written without a Windows machine,
from the Windows documents and from the source of other watchers.

It first ran on 2026-10-10, on Windows 11 Pro (build 26200) with NTFS on a local disk:

- `crates/pathwatch/tests/windows.rs` (the backend with real handles): all tests pass, 20
  runs in a row. So each point below that has a test there holds on that system.
- `crates/pathwatch/tests/windows_parts.rs` (the parts that make no Windows call: `place`,
  `Handles`, `Records`, `translate`): all 60 tests pass (72 by now).
- `crates/pathwatch/tests/disk` with this backend: every test passes, but two cannot run with
  it: `a_directory_above_a_watched_path_is_renamed_to_another_letter_case` and
  `a_directory_above_a_watched_path_is_moved_away_changed_and_moved_back` rename a directory
  above an open handle, which Windows refuses; they run with polling only. The test with
  sockets (`a_file_is_anything_that_is_not_a_directory`) is Unix only, and the tests with
  symbolic links stop early when the user may not make one.

That first run also found what the engine got wrong about an NTFS disk when it polls. All of
it is in `fs/windows.rs` now (`told` and `stat`):

- **An entry that is being deleted answers "access denied"** to every call, until the one who
  deletes it closes its handle. So does a directory that is opened as a file. The engine took
  that for a file that cannot be read: it reported `PermissionDenied`, or an `Update` right
  before the `Delete`. Only the status of the kernel (`RtlGetLastNtStatus`) tells the two
  apart, and such an answer is "not found" now, with a mark on the error: the node is looked
  at again at every flush while the deletion waits, since the holder can take it back
  (`FileDispositionInfo` with `FALSE`), which the OS reports as little as the deletion
  itself; the file gets its `Create` then. That call is an export of `ntdll.dll` that
  Microsoft does not document: should a Windows drop it, such an entry looks unreadable
  again, as before.
- **The creation time is no identity.** A new entry gets the creation time of the one that
  was removed under its name less than 15 seconds ago, and the clock of the time stamps
  moves in steps of 1 to 16 ms. A file that was removed and written again with the same
  content had every time and the size of the old one, and the poll saw no change. The
  identity of a file and of a directory is the file index now, with the volume serial number
  for a directory.

The first run of the benchmark on Windows (`bench/BENCHMARK.md`) found two more, in the
backend:

- **A directory that got no handle was polled for ever.** A directory on the way to a watched
  path asks for names only and gets no handle. When the handles moved up later and one stood
  at or above it, nobody told the engine, and it went on polling: with 20,000 watched files in
  2,000 directories, 2,000 `lstat` calls each second while nothing changed. `commit` returns
  such a key now (rule 4), and the engine asks again.
- **A key that wants names only is answered `Watched`.** After `WatchedTree` the engine takes
  back the watches of the directories below, but only a key that wants the contents holds a
  handle: the handle went away with them, and all of them asked again, without end.

Then the two numbers in which the backend was far behind (`bench/BENCHMARK.md`):

- **Setup.** One `lstat` of std opens the entry, asks, and closes it: 10 us. Two faster ways,
  both in `fs/windows.rs`:
  - `GetFileInformationByName` (Windows 11 24H2 and later; looked up at run time) tells the
    same in one call that opens nothing: 3.5 us. It takes the path as it is, so a path of
    260 units or more goes the old way.
  - A listing with `FileIdBothDirectoryInfo` gives name, kind, time, size and file index of
    some hundred entries in one call: 0.35 us for each. It is taken for a regular file on
    NTFS only, where it was measured to tell what `lstat` tells. It is behind for a file that
    a writer has open, until the writer closes it, and for a hard link whose file was written
    through another name. So the engine takes it only in a directory that an OS watch covers
    (`Fs::listing_lags`): there the close of the writer makes a record. A directory that is
    polled asks each file by itself, as before.
- **The time from a write to its event.** A young file was opened and hashed, and on Windows
  an open for reading waits for the virus scanner when the file was just written. The engine
  takes the number that NTFS keeps for the file in its place (`Fs::version`, design 5.8).

And three more, from a look at where the time of a setup goes:

- **The handles are opened by `commit`.** A request that needs a new handle asked `place`
  for every directory again: with 20,000 watched files in 2,000 directories that was 390 of
  the 520 ms of the setup. Now `watch` only notes the key, and the next `commit` asks `place`
  once (rule 2: the engine reads no directory before it). A key that gets no handle then is
  told and asks again.
- **A key that only got another handle is not told.** It was told, the engine asked again
  and scanned its directory again: 2,000 of these after such a setup. Nothing is lost when
  the handles move, so nothing is said (design 7.3). A key behind a link below its new
  handle is the exception: it is told, and the next `watch` gives it up.
- **`MODIFIED` for a directory is dropped in the backend.** After a scan of 5,000 directories
  Windows sent one such record for each of them (it brings the entry of the directory in its
  parent up to date when a handle on it is closed), and the engine looked at each one: the
  first change after a `watch` took 25 ms. The longer records of `ReadDirectoryChangesExW`
  say what is a directory.

What is left of the difference: a watcher that keeps no list of the files (notify, parcel,
`fs.watch`) is ready as soon as its one handle is open, and pathwatch reads the tree first.
That is what lets it tell a new file from a changed one and report a tree that is moved in
or out.

What was not tried: a network share, ReFS, a FAT disk, an older Windows, Wine, and a rename
made from WSL. A point below that has no test is still only known from documents.

## How to check

On macOS or Linux, with a toolchain that has the Windows target:

```
cargo check  --workspace --all-targets --target x86_64-pc-windows-msvc
cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
```

On Windows (the GitHub runner `windows-latest` is enough):

```
cargo test -p pathwatch --test windows
cargo test -p pathwatch --test disk
```

The first line runs the backend alone. The second line runs the contract table of the design on
the real disk, with this backend and with polling.

## Not verified on Windows

- **The spelling of a name** (`Fs::name_on_disk`, `fs/windows.rs`). On a disk that takes two
  spellings of a name for one, a look asks how the disk spells the name that `lstat` found
  (design 5.2). Here that is `FindFirstFileExW` for the path, and the name that it gives back.
  Checked: `fs::windows::tests::the_name_on_disk` (the name as the directory holds it, also at
  the end of a path of more than 260 units; the call takes the `\\?\` form where std would),
  and the disk tests of promise 13, which run on Windows too.
- **How a name that is gone is spelled in a record.** FSEvents names a removed entry as the
  caller spelled it, and the engine is right for both spellings. Which one
  `ReadDirectoryChangesW` gives is not known.
- **Which names the disk takes for one.** The rule for plain names (design 5.2) was measured
  on APFS. NTFS compares names through its table of upper-case letters and knows no Unicode
  forms. Measured: it takes the two cases of a letter for one name, also outside ASCII
  (`é`/`É`, `σ`/`Σ`, `ф`/`Ф`, fullwidth letters), and for two names the NFC and NFD forms,
  `ß`/`SS`, `ſ`/`s`, the Kelvin sign/`k`, `ﬁ`/`fi`, `ı`/`I`; no character of the BMP is
  taken for an ASCII one or for an ideograph, so `is_plain` holds on NTFS, and the only alias
  of a plain name is its short name.
- **How lost records show up.** Expected: the read completes with success and zero bytes. The
  error `ERROR_NOTIFY_ENUM_DIR` is taken the same way. Zero bytes mean that the buffer of this
  one handle was too small. So both become `Changed` with an empty `rel` for each key that the
  handle serves, and the engine scans below these directories, not everything.
- **How many `MODIFIED` records one save makes.** Expected: one to three, some of them only when
  the writer closes the file. Each one becomes a `Written` hint, so one save can make more than
  one `Update` when the records are further apart than the quiet time.
- **A directory with a handle that is deleted.** Expected: the pending read fails with
  `ERROR_ACCESS_DENIED`. A record with an empty name is taken the same way. Both become `Changed`
  with an empty `rel` for each key of the handle: the engine checks the directory and calls
  `watch` again. If Windows sends neither, the handle stays open on a directory that has no name
  any more, and only a new `watch` call for that directory repairs it.
- **A directory with a handle that is deleted and made again at once.** As above. The new
  directory is only reported after the engine has called `watch` again.
- **A renamed ancestor.** Expected: Windows refuses to rename a directory above an open handle,
  so this cannot happen while a handle is open. Not checked on ReFS, on network shares and for
  renames made from WSL.
- **The directory of a handle that is renamed.** Windows allows that (the handle is opened with
  `FILE_SHARE_DELETE`). The handle follows its directory, not its path: it goes on reporting
  under the old key, and no record says that the directory moved. The backend cannot tell, so
  rule 10 of the backends is not kept here: the engine finds out by the poll of the directory
  above, within a second, or by a shared handle above. Until then the hints make the engine
  look at paths where nothing is, and a new directory made at the old path gets a handle only
  with the next `watch` of it.
- **How the directory is opened.** `port` opens it with `std::fs::OpenOptions`: access mode
  `FILE_LIST_DIRECTORY`, share mode `READ | WRITE | DELETE`, custom flags `BACKUP_SEMANTICS |
  OVERLAPPED`. Expected: std passes these to `CreateFileW` as they are, and `File::metadata`
  works on such a handle. The engine reads the disk through std too, so both sides open a path in
  the same way.
- **Long paths.** std puts a long path (about 248 units or more) into `\\?\` form by itself.
  Names in records are relative to the handle and have no limit.
- **Names that end with a dot or a space.** Windows drops the dot or the space unless the path is
  given in `\\?\` form, and `path::normalize` takes that form as the plain one. So such a name
  cannot be watched: the OS finds the name without the dot or the space. `watch` returns
  `InvalidPath` for it, and for a name with `:`, a control character or one of `< > " | ? *`.
  A directory can still hold such a name, and the name of a device (`nul`, `aux.c`): git, WSL
  and an unpacked archive make them. For a path with one, every call of the engine and the
  backend takes the `\\?\` form (`fs::literal`), so the entry is read under this very name.
  Before that, such a file below a watched directory got no event at all (measured).
- **Short (8.3) names.** Windows may report a change under the short name of a file. A name that
  looks like one (`LONGFI~1.TXT`) becomes a `Changed` hint for the directory that holds it. A
  name with a lower-case letter (`v1~1.txt`) is never taken for one: Windows makes short names
  in upper case only. Not checked: which parts of a reported path can be short names, and how
  often real long names look like one. The other way round: a watched path that holds the short
  name of an entry (`d\LONGFI~1.JSO`, or `d\LONGDI~1\file.js`) gets records that carry the
  long name, which match no registered name. The tree keeps the registered names that look like
  short names on a list for each directory, and a hint with a long name marks all of them, as a
  guess that the look decides (`design.md` 5.2). Measured: a watched file with a short name, and
  a watched file below a directory with one, get `Update`, `Delete` and `Create` through the
  long names (`disk::promises::a_watched_path_with_a_short_name`). Still open: a key whose own
  directory holds a short name and shares a handle above it is found by the text of its path,
  which the long name in the record does not match (README, "more than 64 directories").
- **Names that no path can hold.** A disk that another system filled (an NTFS volume written
  from Linux, a share without name mangling) can list a name with `\` or `/` in it, or one that
  reads as a drive (`C:`, `Q:x`). `PathBuf::push` would start the path anew for the second kind,
  so a name is appended to a path by its text (`tree::push_name`), and `pop` takes it off again.
  A name with a separator in it is left out of every listing (`fs::has_separator`): no Win32
  path names such an entry, and nothing about it can be reported. Not run: this host cannot
  make such a name, so only the joining is tested (`windows::a_listed_name_that_reads_as_a_drive_stays_in_its_directory`).
- **Junctions and directory symlinks.** A handle opened through a link watches the target. A
  handle does not report changes that happen behind a link below it: they are reported under the
  real path, and only if that path is below a handle too. So with more directories than handles,
  a directory behind a link is not joined with its neighbours above the link: it keeps a handle
  of its own below the shared one (measured: 64 directories and a junction next to them come to
  rest with two handles, `directories_above_the_limit_and_a_junction_come_to_rest`; before, the
  commit joined the junction with the others, found it behind the link, gave it up and moved the
  handles back, and the engine asked again: two to four thousand handle opens in ten idle
  seconds). A link is made in two steps: a plain
  directory (`ADDED`, in the filter), then the reparse point on it, which is reported under
  `FILE_NOTIFY_CHANGE_ATTRIBUTES` only, a bit the filter leaves out (an attribute change of a
  file would be `MODIFIED`, and a `Written` that is no write). So the engine looks once more at
  every new directory, at the next flush (`Options::two_step_links`).
- **Closing a handle.** Expected: after `CancelIoEx` and `CloseHandle` the pending read completes
  and its packet reaches the port. Until then the buffer is kept. If the packet does not come
  within two seconds at shutdown, the buffer is leaked.
- **A read that cannot be queued.** When `ReadDirectoryChangesW` returns `FALSE`, no request
  was started: the kernel is done with the buffer, and no completion packet comes for it. A
  handle still takes no further read after such a failure: the fall back from the longer
  records opens a new handle with a key of its own (`Worker::open`).
- **Network shares.** A server that cannot notify makes the first read fail. The answer is
  `Failed`, and the engine polls the directory. A read that fails after the open gives the
  handle up and tells its keys; when the next handle on the directory dies the same way
  before it delivered anything, the ask after that is `Failed` too, and the directory is
  polled. A server that cannot be reached answers
  `ERROR_BAD_NETPATH`, which std calls "not found": `told` makes it no answer, so the files
  on the share get no `Delete` while the server is away.

### More directories than handles

When more than 64 directories want a handle, directories share a handle on a directory above
them (`place` in `windows.rs`). The move up and down runs with real handles in
`more_directories_than_handles_share_a_handle` and
`directories_above_the_limit_and_a_junction_come_to_rest`; the rest has run as plain code
(`windows_parts.rs`):

- **The names in the records of a shared handle.** Expected: paths relative to the directory of
  the handle, with `\` between the parts, spelled as on the disk. The backend joins such a name
  to the path of the handle and looks for every key whose directory holds it. The directory
  of a key is spelled as the engine got it. If the two differ in the case of letters, the key
  is still found. Not checked: how often a watched path holds a short name on a real machine.
  Such a key gets no hints while it shares a handle.
- **A record for the directory of a key itself.** A handle above reports when a directory below
  it is added, removed or renamed. Expected actions: `ADDED`, `REMOVED`, `RENAMED_OLD_NAME`,
  `RENAMED_NEW_NAME` with the path of that directory. They become `Changed` with an empty `rel`
  for its key. A `MODIFIED` record for that path is expected whenever an entry in the directory
  changes, and is dropped. If Windows sends `MODIFIED` and nothing else for a directory that
  was replaced, the replacement is only found when the engine looks.
- **Moving the handles.** When a directory comes or goes, handles can move up to a common
  directory or down again. The new handles are opened first, then the old ones are closed. No
  change is missed in between, and a change that both handles report only costs the engine a
  second look. One open takes about 40 us (measured).
- **A shared handle that Windows does not let us open.** A common directory can refuse
  `FILE_LIST_DIRECTORY` where the directories below it allow it. Then the request that needed
  the shared handle is answered `Failed` with that error, the handles stay where they were, and
  the same happens at each later request until the count is under the limit again.
- **How much a shared handle reports.** A handle on `node_modules` reports every change of every
  package, and the backend drops what no key holds. Not measured: how often the 64 KiB buffer of
  such a handle is too small while packages are installed. Each time, every key of the handle
  gets a `Changed` with an empty `rel`, and the engine paces the scans.
- **The directories that never get a handle.** `C:\`, `C:\Users` and `C:\Users\<name>`, on
  every drive. Not checked: whether other places need the same rule (a home directory that was
  moved to another path, a folder that a sync tool fills).
- **A stale shared handle.** A handle follows its directory when that is renamed. The backend
  puts a new handle on a directory when the engine asks for that very directory again. For a
  shared handle this needs the engine to have a node for the directory of the handle and to ask
  for it when it finds another directory there. Expected: it does, because that directory is on
  the way to the watched paths.

Numbers from macOS (plain code only, release build, 2,000 directories that share one or two
handles): all `watch` calls together take 12 to 28 ms, and one record takes 0.7 us to find its
key. An `unwatch` moves no handle: the next commit works out the placement once for all of
them.

## The tests that settle these points

`crates/pathwatch/tests/windows.rs` compiles only on Windows. These tests settle the points
above. When one fails on another system, the point and the code behind it have to be read
again:

- `open_point_lost_records_become_a_hint_for_each_key_of_the_handle`
- `open_point_a_save_becomes_a_written_hint`
- `open_point_a_deleted_directory_becomes_a_hint_for_the_directory_itself`
- `open_point_a_directory_that_is_deleted_and_made_again`
- `open_point_a_renamed_directory_reports_under_its_key`
- `open_point_only_the_directories_above_a_handle_cannot_be_renamed`
- `open_point_a_handle_opened_through_a_junction_watches_the_target`
- `open_point_a_change_behind_a_junction_is_reported_under_the_real_path`
- `open_point_a_directory_below_a_handle_that_is_renamed_is_told_itself`
- `more_directories_than_handles_share_a_handle` (65 directories in the temp directory: the
  move to one handle, a hint for each key, nothing for what no key holds, and the move back)
- `open_point_a_change_made_through_a_short_name`
- `open_point_a_directory_name_that_ends_with_a_dot`
- `a_path_longer_than_260_units` and `both_spellings_of_the_temp_directory_work`
- `what_is_no_directory_fails` (it checks the error kinds that std gives)

The other tests check the backend itself: a watch is live when `commit` returns, each kind of
change makes its hint, the rules of `Handles` hold with real handles, and `drop` ends the thread.

## Known gaps

These are known from reading. `windows.rs` has few comments: what the backend does, and where
its limits are, is in `docs/design.md` 7.3.

1. **A key behind a link below a directory that has a handle anyway is polled.** A handle does
   not report what is behind a junction or a directory link below it, so the backend looks at
   each name between the handle and the directory (`leads_through_a_link`) and answers
   `NotWatched` when one is a link. A directory behind a link that wants the contents gets a
   handle of its own below the shared one (`place` knows the links); what is left to the poll
   is a key that wants names only, or lies below a directory that has a handle of its own.

2. **Hints after `unwatch`.** A record finds its key among the keys of now, so a read that was
   made before `unwatch` gives no hint with that key. One case is left: the notice "another
   handle serves this key now" can be on its way when the key is unwatched. The row of `unwatch`
   in the table of design 6 allows a late hint, and the engine drops a hint whose key names no
   node.

3. **A short name in the directory of a key itself** becomes `Changed` with an empty `rel`.
   For the engine that means "check this directory and watch it again", so everything below is
   scanned, and a handle on that directory is opened again. It is correct, but more work than
   needed. A hint that says "scan this one directory" does not exist. With a shared handle it
   can be worse: when no key is at or above the directory that holds the short name, the short
   name can be a directory on the way to any key below, so each of these keys gets that hint.

4. **Lost records in the last read of a closed handle.** Settled: the read that a close
   cancels fails with `ERROR_OPERATION_ABORTED` (118 of 118 closes, measured), so a last read
   that ends with no bytes is lost records, and it is passed on like every other one. A close
   can also find the read over and its packet not taken yet (`CancelIoEx` finds nothing): the
   kernel keeps the records that come after such a read until the handle is closed, so the
   handle is read on and closed when a read comes back cancelled. The call that queued the
   closes waits for them.

5. **A key at the directory of a handle is asked again after it moved.** Settled: a key that
   gets another handle is not told any more.

6. **Above the limit, path order decides.** When more than 64 handles are needed and the
   directories have only a top directory or a home directory in common, the last directories in
   path order get no handle. So a new directory can take the handle of one that was watched
   before. That one is told, asks again, and is answered `Failed` with the watch limit error.

7. **`unwatch` can open handles.** When a key goes and there is room again, shared handles move
   down. If one of the new handles cannot be opened, the handles stay where they were: correct,
   but higher than needed.
