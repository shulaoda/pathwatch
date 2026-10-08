# The Windows backend: open points

The Windows backend (`crates/pathwatch/src/windows.rs`) was written without a Windows machine.
**It has never run.**

What was done:

- The whole backend, with its tests, is type-checked for `x86_64-pc-windows-msvc` by cross
  compilation.
- The parts that make no Windows call (`place`, `Handles`, `Records`, `translate`) are tested on
  other systems, by `crates/pathwatch/tests/windows_parts.rs`.

What was not done: the part of the file behind `cfg(windows)` (`WindowsBackend` and `port`) and
`crates/pathwatch/tests/windows.rs` have never opened a handle. The points below come from the
Windows documents and from the source of other watchers. Each one has to be checked on a real
machine before this backend is trusted. `pathwatch` uses it on Windows all the same:
`Config::with_polling(true)` is the way around it until then.

## How to check

On macOS or Linux, with a toolchain that has the Windows target:

```
cargo test -p pathwatch --test windows_parts
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

- **The spelling of a name** (`Fs::name_on_disk` in `fs.rs`). On a disk that takes two
  spellings of a name for one, a look asks how the disk spells the name that `lstat` found
  (design 5.2). Here that is `FindFirstFileW` for the path, and the name that it gives back.
  It was only compiled. To check: it gives the name as the directory holds it, also for a
  link; and a path of more than 260 units works (std puts such a path into `\\?\` form for
  its own calls, this call passes the path as it is).
- **How a name that is gone is spelled in a record.** FSEvents names a removed entry as the
  caller spelled it, and the engine is right for both spellings. Which one
  `ReadDirectoryChangesW` gives is not known.
- **Which names the disk takes for one.** The rule for plain names (design 5.2) was measured
  on APFS. NTFS compares names through its table of upper-case letters and knows no Unicode
  forms, so it should take fewer names for one. Not checked.
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
- **How the directory is opened.** `port` opens it with `std::fs::OpenOptions`: access mode
  `FILE_LIST_DIRECTORY`, share mode `READ | WRITE | DELETE`, custom flags `BACKUP_SEMANTICS |
  OVERLAPPED`. Expected: std passes these to `CreateFileW` as they are, and `File::metadata`
  works on such a handle. The engine reads the disk through std too, so both sides open a path in
  the same way.
- **Long paths.** std puts a long path (about 248 units or more) into `\\?\` form by itself.
  Names in records are relative to the handle and have no limit.
- **Names that end with a dot or a space.** Windows drops the dot or the space unless the path is
  given in `\\?\` form. This is the same for the backend and for the engine, so such a directory
  can only be watched under its `\\?\` spelling.
- **Short (8.3) names.** Windows may report a change under the short name of a file. A name that
  looks like one (`LONGFI~1.TXT`) becomes a `Changed` hint for the directory that holds it. Not
  checked: which parts of a reported path can be short names, and how often real long names look
  like one.
- **Junctions and directory symlinks.** A handle opened through a link watches the target. A
  handle does not report changes that happen behind a link below it: they are reported under the
  real path, and only if that path is below a handle too.
- **Closing a handle.** Expected: after `CancelIoEx` and `CloseHandle` the pending read completes
  and its packet reaches the port. Until then the buffer is kept. If the packet does not come
  within two seconds at shutdown, the buffer is leaked.
- **A read that cannot be queued.** Expected: when `ReadDirectoryChangesW` returns `FALSE`, no
  packet comes for that call.
- **Network shares.** A server that cannot notify makes the first read fail. The answer is
  `Failed`, and the engine polls the directory.

### More directories than handles

When more than 64 directories want a handle, directories share a handle on a directory above
them (`place` in `windows.rs`). All of this is new and has only run as plain code:

- **The names in the records of a shared handle.** Expected: paths relative to the directory of
  the handle, with `\` between the parts, spelled as on the disk. The backend joins such a name
  to the path of the handle and looks for the deepest key whose directory holds it. The
  directory of a key is spelled as the engine got it. If the two differ in the case of ASCII
  letters, the key is still found. Not checked: how often they differ in another way on a real
  machine (a letter that is not ASCII in another case, a short name in the watched path). Such a
  key gets no hints while it shares a handle.
- **A record for the directory of a key itself.** A handle above reports when a directory below
  it is added, removed or renamed. Expected actions: `ADDED`, `REMOVED`, `RENAMED_OLD_NAME`,
  `RENAMED_NEW_NAME` with the path of that directory. They become `Changed` with an empty `rel`
  for its key. A `MODIFIED` record for that path is expected whenever an entry in the directory
  changes, and is dropped. If Windows sends `MODIFIED` and nothing else for a directory that
  was replaced, the replacement is only found when the engine looks.
- **Moving the handles.** When a directory comes or goes, handles can move up to a common
  directory or down again. The new handles are opened first, then the old ones are closed, then
  each key that got another handle gets a `Changed` with an empty `rel`. Expected: no change is
  missed in between, and a change that both handles report only costs the engine a second
  look. Not checked: how long opening and closing some dozens of handles takes.
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
handles): all `watch` calls together take 12 to 28 ms, one `unwatch` takes 0.5 to 1 ms (each
one works out the whole placement again), and one record takes 0.7 us to find its key.

## Tests to run on Windows first

`crates/pathwatch/tests/windows.rs` compiles only on Windows. These tests settle the points
above. When one fails, the point and the code behind it have to be read again:

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

The other tests check the backend itself: a watch is live when `watch` returns, each kind of
change makes its hint, the rules of `Handles` hold with real handles, and `drop` ends the thread.

## Known gaps to close when it first runs

These are known from reading. The limits of the backend are in the comments of `windows.rs`, at
the code that they are about (`Handle`, `Handles`, `Handles::route`, `may_hold_a_handle`).

1. **A link below a handle is taken for covered.** The engine calls `watch` for a directory that
   is reached through a junction or a directory link, because a handle above does not report
   what is behind the link. `Handles` compares path text, finds the handle above, and
   answers `WatchedTree`. So that directory is not polled, and changes in it are not found. This
   breaks rule 4 of `Backend` ("the answer is the truth").

   Fix to try: before a handle above is taken for the key, check the path parts between the
   directory of the handle and `dir` with `fs::symlink_metadata`. If one of them is a link,
   open a handle on `dir`. The open follows the link, so the handle is on the target.

2. **Hints after `unwatch`.** A record finds its key among the keys of now, so a read that was
   made before `unwatch` gives no hint with that key. One case is left: the notice "another
   handle serves this key now" can be on its way when the key is unwatched. Rule 12 of `Backend`
   allows a late hint, and the engine drops a hint whose key names no node.

3. **A short name in the directory of a key itself** becomes `Changed` with an empty `rel`.
   For the engine that means "check this directory and watch it again", so everything below is
   scanned, and a handle on that directory is opened again. It is correct, but more work than
   needed. A hint that says "scan this one directory" does not exist. With a shared handle it
   can be worse: when no key is at or above the directory that holds the short name, the short
   name can be a directory on the way to any key below, so each of these keys gets that hint.

4. **Lost records in the last read of a closed handle.** The last read of a handle that the
   backend closed is expected to end with no bytes, and lost records look the same. So the worker
   passes on the last read only when it holds records. When the handles move, each key that got
   another handle is told, so nothing is missed there. One case is left: a `watch` call for the
   very directory of a handle replaces that handle, and the other keys of the handle are not
   told. The engine made that call and scans the directory after it, but only as deep as it
   decided to.

5. **A key at the directory of a handle is asked again after it moved.** A key that gets
   another handle is told with `Changed` and an empty `rel`, and the engine then calls `watch`
   for it. If the new handle is on the directory of that key, this call opens one more handle
   there and closes the one that was just opened. It is correct, but one open and one close
   too many for each such key. The backend cannot tell this call from the one that the engine
   makes when it finds another directory at the path.

6. **Above the limit, path order decides.** When more than 64 handles are needed and the
   directories have only a top directory or a home directory in common, the last directories in
   path order get no handle. So a new directory can take the handle of one that was watched
   before. That one is told, asks again, and is answered `Failed` with the watch limit error.

7. **`unwatch` can open handles.** When a key goes and there is room again, shared handles move
   down. If one of the new handles cannot be opened, the handles stay where they were: correct,
   but higher than needed.
