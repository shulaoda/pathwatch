# The benchmark of pathwatch

This directory runs pathwatch next to other file watchers, on the same trees of files, and
measures all of them in the same way. It is a cargo workspace of its own, so the lock file,
the tests and the CI of pathwatch never see the other libraries.

```
bench/
  driver/     the driver (`bench`) and the program that makes the tables (`report`)
  adapters/   one small Rust program for each Rust library
  js/         one small Node script for each Node library
  results/    the JSON file of a run, and its tables as Markdown
```

## The design: one driver, many adapters

An **adapter** is a small program around one library. It reads its job from stdin (the paths
to watch, an ignore rule, polling), starts the library, prints `ready` when the library says
that it watches, and then prints one line for each event that the library gives: a word for
the kind, and the path. The protocol is at the top of `adapters/src/lib.rs`.

The **driver** makes a tree of files in a temporary directory, starts ONE adapter as a child
process, changes files, and reads the lines. It knows nothing about the library behind the
adapter, so every library is measured in the same way:

| What | How the driver measures it |
| --- | --- |
| setup | The time from the start of the process to the `ready` line. |
| memory | The resident size (RSS) of the adapter process at `ready` and after all phases, its highest value, and the memory that belongs to the process alone ("footprint"). The same adapter is also started with no path at all: that is its base. |
| CPU | User plus system time of the adapter process: up to `ready`, in each phase, and in a quiet phase where nothing changes. |
| latency | The driver writes one file and takes the time until the first line for that path comes. Many single changes give the median (p50), the 95th percentile (p95) and the worst (max). |
| bursts | Many changes at once. Every path counts its time from the start of the burst, so `max` is the time until the events for ALL paths came. |
| right or wrong | The changed paths that got no event (`missed`), and the events for paths that should have none (`extra`). |

How the numbers are read:

- macOS: `proc_pid_rusage` (CPU time in nanoseconds, resident size, physical footprint).
- Linux: the CPU clock of the process (`clock_getcpuclockid`, nanoseconds) and
  `/proc/<pid>/smaps_rollup` (resident size, private pages). The highest resident size is
  `VmHWM` of `/proc/<pid>/status`.

## The adapters

| Name | Library | How it is used |
| --- | --- | --- |
| `pathwatch` | this repository | `Watcher::with_config` with a handler that sends each batch to a channel, and `watch_all`. Default settings (50 ms debounce). |
| `pathwatch-0` | this repository | The same with `with_debounce(Duration::ZERO)`. |
| `notify` | notify 8 from crates.io | `recommended_watcher` with a channel; `paths_mut()` to add all paths in one step; `Recursive` for a directory, `NonRecursive` for a file. Polling: `PollWatcher`. It has no ignore rule. |
| `notify-fork` | `rolldown-notify` from `github.com/rolldown/notify` | The way rolldown uses it today: `RecommendedWatcher::new` with a `Config`, `paths_mut()`, every path with `Recursive` and `TrackPath`. Ignore: `Config::with_ignored`. Polling: `PollWatcher`. |
| `chokidar` | chokidar 5 | `chokidar.watch(paths, { ignoreInitial: true })` and the `all` event. Ignore: a function for `ignored`. Polling: `usePolling` with `interval` and `binaryInterval`. |
| `parcel` | @parcel/watcher 2 | `subscribe(dir, callback)`. It watches directories only, so for a set of files it watches the lowest directory that holds them all. Ignore: globs for `ignore`. It has no polling mode. |
| `fs-watch` | Node itself | `fs.watch(dir, { recursive: true })`, once, for the lowest directory that holds all paths. It is the floor: no library. Polling: `fs.watchFile` for each path. It has no ignore rule. |

Why this is fair:

- Each library is used the way its own documentation shows for the case. An adapter does
  nothing clever: no filter after the library, no retry, no cache.
- Each adapter runs in its own process, so its memory and CPU time are its own. `node-idle`
  (Node with no watcher) and each adapter with no path give the base of the runtime.
- When a library cannot do a job, the adapter says so (`unsupported`, or a `note` in the
  tables) and the numbers show what the library gives. Example: notify has no ignore rule, so
  in S4 the events for `node_modules` are counted as `extra`.
- All adapters write their lines in the same way: the events that wait go out in one write.
- Each adapter is started once before anything is measured, because the first start of a new
  program is slow (on macOS the first start took 283 ms, and later ones 5 to 20 ms).
- The driver raises its limit of open files to 65,536 and the adapters get it from the driver,
  so the limit of the shell (256 on macOS) does not decide a result.
- Linux: the adapters run with `GLIBC_TUNABLES=glibc.malloc.hugetlb=0`. With the default of
  Ubuntu 26.04 (glibc 2.43) every thread that allocates gets one huge page of 2 MiB: an adapter
  that watches one file had a resident size of 8.3 MiB with it and 2.3 MiB without. The
  memory of the data structures can only be seen without it. `--keep-huge-pages` leaves the
  default.

## The scenarios

Full sizes. `--scale` multiplies them, `--quick` is `--scale 0.05` with a quiet phase of 2 s.

| | Tree | Watched | Phases |
| --- | --- | --- | --- |
| S1 module files | 20,000 files in 2,000 directories | every file by its own path | first edit right after `ready`; quiet; 200 single edits; 50 single atomic saves (write a temporary file, rename it over the file) and one edit of each saved file after; a burst of 2,000 edits; 200 files removed; the same files written again |
| S2 project directory | 50,000 files in 5,000 directories | the one directory | first edit; quiet; 200 single edits; 5,000 new files in 500 new directories; a tree of 5,000 files removed; a tree of 5,000 files moved in by one rename, and moved out by one rename |
| S3 noise | as S1 | as S1 | 100 single edits while another process writes 50,000 files in `project/dist` again and again (`--noise-outside`: in a directory next to the project) |
| S4 ignored directory | 5,000 files in `src`, and `node_modules` | the project directory, with a rule that ignores `node_modules` | 30,000 files are written into `node_modules`, then one edit in `src` right after; 20 single edits |
| S5 polling | as S1 | as S1, with the polling mode of each library at 100 ms | first edit; quiet; 50 single edits |
| S6 missing paths | an empty directory | 1,000 paths that do not exist, in directories that do not exist | quiet; all paths are made; 20 single edits of them |
| S7 big directory | 20,000 files in one directory | the one directory | first edit; 100 single edits; 100 single new files; a burst of 2,000 new files; 100 files removed one by one |

S7 is there for a watcher that lists a directory when something in it changes: it pays for
every entry of the directory. pathwatch does that for a new file on a disk that takes two
spellings of a name for one (macOS, Windows).

## How to run it

Once: the Node packages and the build.

```sh
cd bench
(cd js && npm install)
cargo build --release
```

`cargo build` builds the driver and the Rust adapters. (`cargo run` alone builds only the
driver; the driver says which adapter is missing.)

A quick run on macOS (about 5 minutes), and the full run (about 20 minutes for each of the
three rounds):

```sh
./target/release/bench --quick --out results/quick-macos.json
./target/release/report results/quick-macos.json > results/quick-macos.md

./target/release/bench --repeat 3 --out results/macos.json
./target/release/report results/macos.json > results/macos.md
```

In the OrbStack VM (Linux). The build directory and the Node packages must be in the VM, and
the trees must be on a disk of the VM: inotify does not see changes that macOS makes in the
shared folder.

```sh
orb bash -c 'source $HOME/.cargo/env; cd /path/to/pathwatch/bench \
  && CARGO_TARGET_DIR=$HOME/pathwatch-bench-target cargo build --release'
orb bash -c 'mkdir -p $HOME/pathwatch-bench-js && cd /path/to/pathwatch/bench/js \
  && cp package.json package-lock.json *.mjs $HOME/pathwatch-bench-js/ \
  && cd $HOME/pathwatch-bench-js && npm ci'

orb bash -c 'cd /path/to/pathwatch/bench && B=$HOME/pathwatch-bench-target/release \
  && $B/bench --quick --tmp /tmp --js $HOME/pathwatch-bench-js --out results/quick-linux.json \
  && $B/report results/quick-linux.json > results/quick-linux.md'

orb bash -c 'cd /path/to/pathwatch/bench && B=$HOME/pathwatch-bench-target/release \
  && $B/bench --repeat 3 --tmp /tmp --js $HOME/pathwatch-bench-js --out results/linux.json \
  && $B/report results/linux.json > results/linux.md'
```

`/tmp` of the VM is a `tmpfs` (files in memory). `--tmp $HOME` puts the trees on its disk.
Copy the scripts again (the second command) after a change in `js/`.

All flags: `./target/release/bench --help`. The useful ones:

- `--scenarios s1,s3` and `--adapters pathwatch,chokidar` (or `rust` for the four Rust ones)
- `--scale 0.25`, `--quiet-secs 10`
- `--repeat 3`: every scenario runs three times with every adapter, one round after the other
- `--tmp <dir>`, `--js <dir>`

**Run the full sizes on a quiet machine.** The driver prints a warning when the load average is
above a quarter of the cores, and the tables carry it. Counts (missed, extra, events) are exact
on a busy machine too. Times and CPU are not.

## The JSON file

One file for a run. It holds the machine (CPU, cores, memory, OS, kernel, the file system of
the temporary directory, the limit of open files, the load average at the start), the version
of every library, of Node and of the Rust compiler, the settings of the run, the base of each
adapter, and for each scenario and adapter every number of the tables. It also holds what the
tables leave out: the notes and errors of the adapter, and up to five samples of the missed
and of the extra paths of each phase. The types are in `driver/src/record.rs`.

## The tables

`report` prints the summary "Where pathwatch is behind", and then, for each scenario, one
table "Setup and memory" and one table for each phase. Times are milliseconds, memory is MiB.

With `--repeat`, a time or a size in the tables is the middle one of the runs (of two runs:
the lower one), and a count (missed, extra, dir events, repeats, errors) is the worst one. A
run that failed is left out when another one did not fail, and a note says so.

Where pathwatch is behind:

| Part | Meaning |
| --- | --- |
| which numbers | The time and the CPU time of the setup, the memory above the base (the footprint, or the resident size where there is no footprint), and for each phase the p50, the max and the CPU time. Lower is better in each. |
| who is compared | `pathwatch` and `pathwatch-0` with the best of the other libraries in that number. A library counts in a phase only when it missed no change there and did not stop early. |
| behind | More than 10% above the best other, and by more than 1 ms or 0.5 MiB. |
| the three tables | Behind with both settings; behind only with the default settings (what the 50 ms wait costs); behind only without the wait. |
| `middle (lowest to highest)` | With more runs than one: the middle run, and how far the runs are apart. Two numbers whose ranges overlap are not apart. |
| times | `pathwatch-0` divided by the best other (`pathwatch` where `pathwatch-0` has no number). |
| below the tables | In how many numbers each setting is the best or near it, and every phase in which pathwatch missed a change, gave an event too many, or printed an error. |

Setup and memory:

| Column | Meaning |
| --- | --- |
| setup | From the start of the process to `ready`. It holds the start of the runtime: see the base. |
| setup CPU | User plus system time of the process up to `ready`. |
| RSS ready | Resident size at `ready`. |
| RSS above base | `RSS ready` minus the RSS of the same adapter with no path. This is what the watched paths cost. |
| footprint ready | The memory of the process alone at `ready`. macOS: the physical footprint (what Activity Monitor shows). Linux: the private pages. It leaves out pages that are shared with other processes, so it is the better number for a comparison. |
| RSS end, RSS peak | After all phases, and the highest value of the whole run. |
| CPU whole run | User plus system time from the start to the end of the process. |
| errors | How many error lines the adapter printed. The first one is in the notes. |

A phase:

| Column | Meaning |
| --- | --- |
| changes | How many paths the driver changed. Each should get at least one event. |
| missed | Changed paths that got no event. |
| extra | Events for paths that the driver never changed, or that nobody asked for. |
| dir events | Events for a directory above a changed path. They are not wrong; pathwatch never gives them. |
| repeats | More events for a changed path after its first one, and late events for a path of an earlier phase. |
| p50, p95, max | From the start of a change to the first event of its path. In a burst every path counts from the start of the burst. |
| write | How long the driver took to make the changes of a burst. |
| CPU | User plus system time of the adapter in the phase, with the wait for late events. |
| first word | The kind of the first event of each changed path, in the words of the library, and how often it came. |

A quiet phase has only the CPU time and the number of events (none is right).

The rules of the driver:

- An event of the kind `access` (a file was opened or read; only notify gives it) is counted
  apart and never used for a time.
- After a single change the driver waits 30 ms before the next one. A phase is over when the
  adapter printed nothing for 250 ms (at most 3 s).
- The wait for a single change is 3 s (10 s with polling). After five misses in a row the phase
  stops early, and the table says so.
- A burst gives up when not one line came for 3 s, and after 60 s in any case.
- The driver waits 1.5 s between making a tree and starting the adapter. FSEvents hands a new
  stream the changes of the last moments.

## What it cannot tell

- **A missed path is not always a bug.** notify, @parcel/watcher and `fs.watch` report what the
  OS tells them. For a directory that is renamed in or out, the OS names the directory only,
  so these libraries give one event and the table shows every file below it as missed.
  A program that needs the files must list the directory itself. The table shows how much
  work is left to the program, not that the library is broken.
- **rolldown's own code is not measured.** rolldown puts a layer on top of the fork (a check
  of every event against the disk, and a walk of each new directory). `notify-fork` here is
  the library alone, so its `missed` is higher and its CPU lower than what rolldown has today.
- **`extra` in S1 and S3 for `parcel` and `fs-watch`** comes from the way they must be used:
  they watch the directory above all files, so they also report the temporary files of an
  atomic save and the noise in `project/dist`. A real program would filter by hand.
- **The cost of the adapter is in the numbers**: one line of text for each event, and for Node
  the event loop. The floor of a latency is about 0.1 ms (pipe and thread of the driver).
- **pathwatch waits on purpose.** By default it collects changes for 50 ms, and it holds a
  `Delete` for 100 ms. So its latency is at least that. `pathwatch-0` shows it without the
  50 ms.
- **The word of an event is not checked.** `first word` shows it, and that is all. On macOS
  notify says `create` for many edits, because FSEvents repeats old flags of a path.
- **Three runs are few.** `--repeat 3` shows whether two numbers are apart, not how a number
  is spread. Without `--repeat` there is one run and no spread at all.
- **Memory of Node moves with its garbage collector.** It can go down between `ready` and the
  end. The resident size on macOS counts shared pages of the system libraries (about 6 MiB for
  a Rust program that does nothing): read the footprint.
- **S3: the speed of the noise is not fixed.** The writer goes as fast as the disk lets it,
  for as long as the edits take. The number of writes and the rate are printed below the
  table.
- **Polling is not the same thing in every library.** pathwatch spends at most about a tenth
  of a core on polling, so on a big set its round is longer than the 100 ms that were asked.
  The single edits come one after the other, each right after the event of the one before,
  so each waits about one whole round: the p50 of that phase is the time of a round.
- **S5: the tree is young.** It is 1.5 s old when the watcher starts. Under polling pathwatch
  takes a file that changed less than 2 s ago for young and reads it (a second write can leave
  its time as it is), so its setup reads all 20,000 files. The first edit comes right after
  `ready`, and a round can look at the file between the cut and the write: then pathwatch
  reports two states, and the second one is counted as an event of the quiet phase.
- **S7 on macOS: FSEvents is late.** A new stream gets the events of the 20,000 files that were
  made seconds before it started, over the first seconds. A library that hands on what the OS
  says prints them (`extra` in the single edits), and every library on FSEvents is slow in the
  first two phases. pathwatch checks each of them against the disk: that is the CPU time
  there.
- **Linux in a VM is not Linux on a machine**, and a `tmpfs` is not a disk.
- **Windows is not supported** by the driver (it reads `/proc` and `proc_pid_rusage`).
