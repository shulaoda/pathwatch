# pathwatch benchmark

The last full run on each OS, one after the other. What the columns mean, and how the numbers are taken: [README.md](README.md).

## macOS

- Started: 2026-10-08T06:49:19Z
- Machine: Apple M4 Pro, 14 cores, 48 GB, macOS 27.0 (Darwin 27.0.0 arm64), temporary directory on apfs
- Open files limit of the adapters: 1048576
- Load average at the start: 2.2 2.4 2.3
- Sizes: the full sizes; a quiet phase is 10 s
- Each scenario ran 3 times with each adapter: a time or a size is the middle one of the runs, a count (missed, extra, errors) is the worst one.
- Times are in milliseconds, memory is in MiB.

### Versions

| name | version |
| --- | ---: |
| chokidar | 5.0.0 |
| fs-watch | part of node |
| node | v26.0.0 |
| notify | 8.2.0 |
| notify-fork | 11.0.0 (git 3a1cde09) |
| parcel | 2.6.0 |
| pathwatch | 0.0.0 (working tree, acbff387) |
| rustc | rustc 1.99.0 (b940084d7 2026-09-28) |

### Bases: each adapter when it watches nothing

| adapter | start to ready | CPU | RSS | footprint |
| --- | ---: | ---: | ---: | ---: |
| chokidar | 52.1 | 53.9 | 49.6 | 17.7 |
| fs-watch | 49.1 | 50.2 | 48.3 | 16.4 |
| node-idle | 50.0 | 51.1 | 48.0 | 16.2 |
| notify | 3.1 | 2.8 | 6.1 | 1.9 |
| notify-fork | 2.6 | 2.3 | 6.1 | 1.9 |
| parcel | 55.9 | 57.9 | 49.6 | 17.6 |
| pathwatch | 2.8 | 2.5 | 6.2 | 1.9 |
| pathwatch-0 | 2.5 | 2.2 | 6.2 | 1.9 |

### Where pathwatch is behind

Every number in which pathwatch is more than 10% behind the best other library, and by more than 1 ms or 0.5 MiB. Only a library that did the job counts: it missed no change of the phase, and for the setup it stopped early in no phase of the run. `pathwatch` has the default settings, and `pathwatch-0` is the same without the 50 ms that pathwatch waits on purpose. `times` is `pathwatch-0` next to the best other, and `pathwatch` where there is no `pathwatch-0`. With more runs than one, a number is `middle (lowest to highest)`.

#### Behind with and without the wait

| scenario | where | number | pathwatch | pathwatch-0 | best other | which | times |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| S1 | quiet | CPU | 5.9 (5.8 to 6.0) | 5.5 (5.4 to 5.8) | 0.0 (0.0 to 0.0) | chokidar | - |
| S1 | single edits | p50 | 66.2 (66.1 to 66.2) | 11.7 (11.7 to 11.7) | 0.5 (0.5 to 0.6) | chokidar | 22.8 |
| S1 | single edits | max | 67.4 (67.4 to 67.6) | 12.8 (12.8 to 17.6) | 4.1 (3.5 to 6.0) | chokidar | 3.1 |
| S1 | single atomic saves | p50 | 66.2 (66.0 to 66.5) | 11.9 (11.9 to 12.0) | 1.3 (1.2 to 1.6) | chokidar | 9.5 |
| S1 | single atomic saves | max | 67.2 (67.1 to 69.3) | 13.3 (12.5 to 19.8) | 3.0 (2.6 to 4.2) | chokidar | 4.4 |
| S1 | single edits of the saved files | p50 | 65.9 (65.7 to 66.0) | 11.7 (11.7 to 11.9) | 0.5 (0.4 to 0.6) | chokidar | 25.4 |
| S1 | single edits of the saved files | max | 66.9 (66.8 to 67.6) | 12.4 (11.9 to 12.5) | 1.3 (0.7 to 4.1) | chokidar | 9.8 |
| S1 | burst of edits | CPU | 15.8 (12.2 to 18.9) | 50.0 (43.2 to 52.1) | 6.3 (6.3 to 7.1) | fs-watch | 7.9 |
| S1 | files removed | p50 | 184 (183 to 199) | 112 (109 to 117) | 57.5 (56.4 to 62.0) | fs-watch | 2.0 |
| S1 | files removed | max | 184 (183 to 199) | 124 (124 to 126) | 59.7 (59.2 to 64.6) | fs-watch | 2.1 |
| S1 | files removed | CPU | 5.6 (5.0 to 5.9) | 5.1 (4.4 to 7.1) | 2.0 (2.0 to 2.4) | fs-watch | 2.5 |
| S1 | files written again | CPU | 3.5 (2.9 to 4.7) | 11.1 (8.3 to 13.5) | 1.2 (1.1 to 1.8) | fs-watch | 9.1 |
| S2 | setup | time | 204 (194 to 208) | 190 (190 to 206) | 13.8 (5.7 to 14.0) | notify-fork | 13.7 |
| S2 | setup | CPU | 202 (192 to 204) | 189 (188 to 204) | 10.5 (4.7 to 11.9) | notify-fork | 18.0 |
| S2 | setup | memory above base | 9.1 (9.1 to 9.1) | 9.1 (9.1 to 9.2) | 0.0 (0.0 to 0.1) | fs-watch | - |
| S2 | first edit, right after ready | p50 | 66.5 (66.5 to 72.6) | 12.7 (11.7 to 12.7) | 1.4 | chokidar | 8.8 |
| S2 | first edit, right after ready | max | 66.5 (66.5 to 72.6) | 12.7 (11.7 to 12.7) | 1.4 | chokidar | 8.8 |
| S2 | quiet | CPU | 1.8 (1.7 to 1.8) | 1.7 (1.5 to 1.9) | 0.0 (0.0 to 0.0) | fs-watch | - |
| S2 | single edits | p50 | 66.7 (66.1 to 68.1) | 12.7 (11.7 to 12.7) | 0.5 (0.5 to 0.5) | chokidar | 23.8 |
| S2 | single edits | max | 73.5 (67.3 to 73.8) | 13.1 (13.0 to 19.6) | 1.2 (1.2 to 1.5) | chokidar | 10.7 |
| S2 | single edits | CPU | 45.9 (45.7 to 46.1) | 52.8 (52.3 to 53.7) | 24.2 (23.8 to 25.6) | notify-fork | 2.2 |
| S2 | new files in new directories | CPU | 42.5 (39.6 to 44.3) | 286 (276 to 286) | 14.1 (13.9 to 14.7) | fs-watch | 20.2 |
| S2 | a directory tree removed | p50 | 420 (417 to 421) | 225 (223 to 228) | 120 (116 to 121) | notify-fork | 1.9 |
| S2 | a directory tree removed | max | 421 (417 to 422) | 359 (343 to 361) | 240 (237 to 244) | notify-fork | 1.5 |
| S2 | a directory tree removed | CPU | 13.3 (13.1 to 15.4) | 23.2 (23.1 to 23.3) | 10.9 (10.7 to 11.3) | notify-fork | 2.1 |
| S3 | single edits under noise | max | 62.3 (61.3 to 64.7) | 4.0 (3.5 to 5.1) | 0.8 (0.8 to 4.2) | chokidar | 4.9 |
| S4 | setup | time | 43.0 (39.8 to 43.5) | 41.2 (40.0 to 43.3) | 12.6 (11.7 to 12.6) | notify-fork | 3.3 |
| S4 | setup | CPU | 40.6 (37.3 to 41.0) | 39.1 (37.7 to 40.9) | 10.1 (9.4 to 10.2) | notify-fork | 3.9 |
| S4 | setup | memory above base | 1.3 (1.3 to 1.3) | 1.3 (1.3 to 1.3) | 0.0 (0.0 to 0.1) | fs-watch | - |
| S4 | first edit, right after ready | p50 | 71.7 (70.9 to 72.0) | 11.0 (10.8 to 11.5) | 2.7 (2.7 to 3.0) | chokidar | 4.0 |
| S4 | first edit, right after ready | max | 71.7 (70.9 to 72.0) | 11.0 (10.8 to 11.5) | 2.7 (2.7 to 3.0) | chokidar | 4.0 |
| S4 | node_modules written, then one edit in src | CPU | 114 (103 to 116) | 146 (128 to 148) | 76.9 (76.2 to 81.8) | fs-watch | 1.9 |
| S4 | single edits in src | p50 | 70.7 (69.8 to 71.0) | 12.8 (12.6 to 13.0) | 0.8 (0.8 to 1.0) | chokidar | 16.0 |
| S4 | single edits in src | max | 73.2 (72.9 to 73.2) | 13.5 (13.3 to 13.6) | 1.2 (1.0 to 2.1) | chokidar | 11.3 |
| S4 | single edits in src | CPU | 6.7 (6.3 to 6.8) | 7.9 (7.3 to 8.9) | 3.6 (3.5 to 3.6) | notify-fork | 2.2 |
| S5 | setup | time | 179 (168 to 180) | 171 (166 to 175) | 39.9 (36.6 to 41.3) | notify-fork | 4.3 |
| S5 | setup | CPU | 178 (167 to 179) | 171 (165 to 174) | 38.8 (35.3 to 40.0) | notify-fork | 4.4 |
| S5 | first edit, right after ready | p50 | 124 (117 to 124) | 125 (120 to 125) | 23.2 (22.3 to 24.1) | chokidar | 5.4 |
| S5 | first edit, right after ready | max | 124 (117 to 124) | 125 (120 to 125) | 23.2 (22.3 to 24.1) | chokidar | 5.4 |
| S5 | first edit, right after ready | CPU | 124 (117 to 124) | 125 (120 to 125) | 45.8 (45.3 to 46.1) | notify | 2.7 |
| S5 | single edits | p50 | 506 (504 to 520) | 517 (502 to 524) | 62.3 (62.1 to 62.3) | fs-watch | 8.3 |
| S5 | single edits | max | 574 (561 to 580) | 570 (560 to 572) | 101 (101 to 102) | chokidar | 5.6 |
| S7 | setup | time | 80.5 (69.1 to 83.1) | 81.3 (67.6 to 81.6) | 26.4 (20.9 to 27.4) | notify-fork | 3.1 |
| S7 | setup | CPU | 63.3 (52.9 to 65.9) | 64.0 (50.9 to 64.4) | 9.4 (4.9 to 10.0) | notify-fork | 6.8 |
| S7 | setup | memory above base | 4.4 (3.5 to 4.5) | 4.4 (3.2 to 4.5) | 0.0 (0.0 to 0.2) | fs-watch | - |
| S7 | first edit, right after ready | p50 | 266 (185 to 303) | 148 (134 to 217) | 0.4 (0.4 to 0.4) | chokidar | 389.8 |
| S7 | first edit, right after ready | max | 266 (185 to 303) | 148 (134 to 217) | 0.4 (0.4 to 0.4) | chokidar | 389.8 |
| S7 | single edits | p50 | 73.2 (72.5 to 73.6) | 16.8 (16.2 to 17.9) | 3.9 (3.8 to 4.3) | chokidar | 4.4 |
| S7 | single edits | max | 487 (323 to 488) | 426 (344 to 470) | 39.8 (26.2 to 91.2) | chokidar | 10.7 |
| S7 | single edits | CPU | 196 (68.6 to 200) | 181 (118 to 188) | 57.9 (54.4 to 62.5) | notify-fork | 3.1 |
| S7 | single new files | p50 | 89.4 (89.2 to 89.5) | 28.7 (28.6 to 29.2) | 12.7 (12.6 to 13.0) | notify | 2.3 |
| S7 | single new files | max | 94.7 (93.8 to 95.5) | 34.5 (31.9 to 35.2) | 15.1 (14.9 to 16.2) | notify | 2.3 |
| S7 | single new files | CPU | 1887 (1849 to 1889) | 1617 (1592 to 1681) | 17.8 (17.5 to 18.3) | notify | 91.0 |
| S7 | burst of new files | p50 | 196 (98.7 to 202) | 103 (101 to 104) | 73.8 (67.8 to 79.4) | notify-fork | 1.4 |
| S7 | burst of new files | max | 202 (196 to 212) | 173 (168 to 175) | 129 (127 to 136) | notify-fork | 1.3 |
| S7 | burst of new files | CPU | 20.1 (20.1 to 39.1) | 173 (169 to 175) | 4.2 (4.0 to 4.3) | fs-watch | 41.5 |
| S7 | single files removed | p50 | 179 (179 to 179) | 122 (121 to 122) | 12.7 (12.5 to 12.9) | notify-fork | 9.6 |
| S7 | single files removed | max | 183 (183 to 183) | 124 (124 to 124) | 13.5 (13.5 to 14.1) | notify | 9.1 |
| S7 | single files removed | CPU | 46.0 (45.8 to 46.2) | 38.3 (38.0 to 38.5) | 17.7 (17.2 to 18.0) | notify | 2.2 |

#### Behind only with the wait: the price of the 50 ms

| scenario | where | number | pathwatch | pathwatch-0 | best other | which | times |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| S1 | first edit, right after ready | p50 | 63.5 (62.3 to 66.7) | 11.6 (11.0 to 11.7) | 11.7 (11.2 to 11.9) | fs-watch | 1.0 |
| S1 | first edit, right after ready | max | 63.5 (62.3 to 66.7) | 11.6 (11.0 to 11.7) | 11.7 (11.2 to 11.9) | fs-watch | 1.0 |
| S1 | burst of edits | p50 | 176 (104 to 188) | 62.1 (57.4 to 82.8) | 72.2 (70.7 to 73.2) | chokidar | 0.9 |
| S1 | burst of edits | max | 188 (176 to 191) | 122 (120 to 145) | 124 (123 to 133) | chokidar | 1.0 |
| S1 | files written again | p50 | 78.7 (78.6 to 82.3) | 11.0 (9.8 to 18.6) | 54.6 (54.0 to 55.7) | fs-watch | 0.2 |
| S1 | files written again | max | 78.7 (78.6 to 82.3) | 22.6 (18.7 to 34.2) | 56.5 (55.9 to 57.5) | fs-watch | 0.4 |
| S2 | new files in new directories | p50 | 375 (345 to 376) | 163 (156 to 172) | 150 (146 to 153) | notify | 1.1 |
| S2 | new files in new directories | max | 375 (345 to 376) | 317 (306 to 324) | 295 (288 to 298) | notify | 1.1 |
| S3 | first edit, right after ready | p50 | 72.7 (72.3 to 72.7) | 12.4 (11.6 to 12.7) | 12.7 (11.6 to 12.7) | fs-watch | 1.0 |
| S3 | first edit, right after ready | max | 72.7 (72.3 to 72.7) | 12.4 (11.6 to 12.7) | 12.7 (11.6 to 12.7) | fs-watch | 1.0 |
| S3 | single edits under noise | p50 | 60.4 (60.4 to 60.5) | 0.5 (0.5 to 0.5) | 0.1 (0.1 to 0.1) | chokidar | 4.7 |
| S4 | node_modules written, then one edit in src | p50 | 60.4 (59.6 to 69.4) | 0.4 (0.4 to 0.5) | 0.2 (0.2 to 0.2) | chokidar | 1.6 |
| S4 | node_modules written, then one edit in src | max | 60.4 (59.6 to 69.4) | 0.4 (0.4 to 0.5) | 0.2 (0.2 to 0.2) | chokidar | 1.6 |
| S6 | the missing paths are made | p50 | 145 (143 to 156) | 49.1 (48.8 to 49.5) | 56.0 (55.7 to 59.4) | fs-watch | 0.9 |
| S6 | the missing paths are made | max | 145 (143 to 156) | 81.7 (80.8 to 88.1) | 121 (121 to 128) | fs-watch | 0.7 |
| S6 | single edits after | p50 | 70.7 (69.1 to 71.1) | 12.7 (12.5 to 13.0) | 12.7 (12.6 to 13.0) | notify-fork | 1.0 |
| S6 | single edits after | max | 73.2 (73.2 to 73.7) | 13.2 (13.1 to 13.3) | 13.7 (13.6 to 14.0) | notify-fork | 1.0 |

#### Behind only without the wait

| scenario | where | number | pathwatch | pathwatch-0 | best other | which | times |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| S1 | single edits of the saved files | CPU | 13.4 (13.0 to 13.7) | 16.3 (14.0 to 18.6) | 12.9 (12.1 to 13.3) | fs-watch | 1.3 |
| S6 | the missing paths are made | CPU | 7.8 (7.7 to 8.2) | 42.3 (42.2 to 42.7) | 10.3 (10.2 to 11.5) | fs-watch | 4.1 |

Of 101 numbers that can be compared, `pathwatch` is the best or near it (inside the limits above) in 25, and `pathwatch-0` in 40.

Where pathwatch was wrong:

- S5 pathwatch, quiet: 0 missed, 0 extra, 1 events in a quiet time
- S5 pathwatch-0, quiet: 0 missed, 0 extra, 1 events in a quiet time

### S1: module files: every file is watched by its own path

Sizes: dirs 2000, files 20000, watched 20000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 82.3 | 79.3 | 18.2 | +12.0 | 13.9 | 18.6 | 18.6 | 196 | 0 |
| pathwatch-0 | 79.8 | 76.7 | 18.1 | +11.9 | 13.8 | 18.4 | 18.5 | 235 | 0 |
| notify | 446 | 443 | 27.6 | +21.5 | 22.1 | 27.6 | 27.6 | 443 | 0 |
| notify-fork | 245 | 242 | 18.5 | +12.4 | 14.3 | 18.7 | 18.8 | 3664 | 0 |
| chokidar | 593 | 863 | 261.3 | +211.8 | 222.9 | 212.0 | 261.7 | 1217 | 0 |
| parcel | 126 | 139 | 73.5 | +23.9 | 40.1 | 61.7 | 75.9 | 325 | 0 |
| fs-watch | 121 | 134 | 71.7 | +23.4 | 38.5 | 72.6 | 72.7 | 251 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 63.5 | 63.5 | 63.5 | 0.0 | 0.2 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 11.6 | 11.6 | 11.6 | 0.0 | 0.2 | update 1 |
| notify | 1 | 1 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 12.9 | 12.9 | 12.9 | 0.0 | 1.5 | metadata 1 |
| chokidar | 1 | 1 | 0 | 0 | 0 | - | - | - | 0.0 | 45.6 |  |
| parcel | 1 | 0 | 0 | 0 | 0 | 13.0 | 13.0 | 13.0 | 0.0 | 0.3 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 0 | 11.7 | 11.7 | 11.7 | 0.0 | 0.6 | change 1 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 5.9 | 10001 | 0 |
| pathwatch-0 | 5.5 | 10002 | 0 |
| notify | 0.0 | 10005 | 0 |
| notify-fork | 0.0 | 10005 | 0 |
| chokidar | 0.0 | 10004 | 0 |
| parcel | 0.0 | 10004 | 0 |
| fs-watch | 0.0 | 10005 | 0 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 66.2 | 66.9 | 67.4 | 0.0 | 56.1 | update 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 0 | 11.7 | 12.0 | 12.8 | 0.0 | 57.8 | update 200 |
| notify | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| notify-fork | 200 | 0 | 0 | 0 | 219 | 15.8 | 21.7 | 23.3 | 0.0 | 1010 | create 9, metadata 191 |
| chokidar | 200 | 0 | 0 | 0 | 0 | 0.5 | 0.9 | 4.1 | 0.0 | 105 | update 200 |
| parcel | 200 | 0 | 0 | 0 | 0 | 66.7 | 68.2 | 69.8 | 0.0 | 95.3 | update 200 |
| fs-watch | 200 | 0 | 0 | 0 | 0 | 15.8 | 20.1 | 21.1 | 0.0 | 57.7 | change 189, rename 11 |

#### single atomic saves

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 50 | 0 | 0 | 0 | 0 | 66.2 | 67.1 | 67.2 | 0.0 | 12.9 | update 50 |
| pathwatch-0 | 50 | 0 | 0 | 0 | 0 | 11.9 | 12.7 | 13.3 | 0.0 | 16.4 | update 50 |
| notify | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| notify-fork | 50 | 0 | 0 | 0 | 86 | 20.8 | 24.8 | 26.3 | 0.0 | 612 | rename 50 |
| chokidar | 50 | 0 | 0 | 0 | 0 | 1.3 | 2.1 | 3.0 | 0.0 | 38.3 | update 50 |
| parcel | 50 | 0 | 50 | 0 | 0 | 67.3 | 68.6 | 68.7 | 0.0 | 28.6 | create 50 |
| fs-watch | 50 | 0 | 50 | 0 | 50 | 15.8 | 20.0 | 20.9 | 0.0 | 31.3 | rename 50 |

#### single edits of the saved files

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 50 | 0 | 0 | 0 | 0 | 65.9 | 66.8 | 66.9 | 0.0 | 13.4 | update 50 |
| pathwatch-0 | 50 | 0 | 0 | 0 | 0 | 11.7 | 12.3 | 12.4 | 0.0 | 16.3 | update 50 |
| notify | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| notify-fork | 50 | 0 | 0 | 0 | 100 | 15.7 | 21.1 | 22.1 | 0.0 | 227 | rename 50 |
| chokidar | 50 | 0 | 0 | 0 | 0 | 0.5 | 0.8 | 1.3 | 0.0 | 28.3 | update 50 |
| parcel | 50 | 0 | 0 | 0 | 0 | 66.7 | 68.2 | 68.3 | 0.0 | 27.6 | update 50 |
| fs-watch | 50 | 0 | 0 | 0 | 0 | 15.7 | 20.4 | 20.6 | 0.0 | 12.9 | rename 50 |

#### burst of edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 2000 | 0 | 0 | 0 | 0 | 176 | 188 | 188 | 123 | 15.8 | update 2000 |
| pathwatch-0 | 2000 | 0 | 0 | 0 | 0 | 62.1 | 110 | 122 | 114 | 50.0 | update 2000 |
| notify | 2000 | 2000 | 0 | 0 | 0 | - | - | - | 136 | 0.0 |  |
| notify-fork | 2000 | 1303 | 0 | 0 | 776 | 507 | 936 | 987 | 124 | 988 | metadata 694, rename 3 |
| chokidar | 2000 | 0 | 0 | 0 | 0 | 72.2 | 121 | 124 | 124 | 70.2 | update 2000 |
| parcel | 2000 | 0 | 0 | 0 | 0 | 189 | 189 | 189 | 125 | 15.1 | update 2000 |
| fs-watch | 2000 | 0 | 0 | 0 | 0 | 85.5 | 127 | 179 | 133 | 6.3 | change 1875, rename 125 |

#### files removed

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 184 | 184 | 184 | 18.6 | 5.6 | delete 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 0 | 112 | 119 | 124 | 18.2 | 5.1 | delete 200 |
| notify | 200 | 200 | 0 | 0 | 0 | - | - | - | 17.6 | 0.0 |  |
| notify-fork | 200 | 0 | 0 | 0 | 198 | 160 | 278 | 287 | 24.9 | 286 | delete 200 |
| chokidar | 200 | 0 | 0 | 0 | 0 | 102 | 102 | 102 | 14.6 | 22.6 | delete 200 |
| parcel | 200 | 0 | 0 | 0 | 0 | 79.2 | 79.2 | 79.2 | 20.6 | 2.3 | delete 200 |
| fs-watch | 200 | 0 | 0 | 0 | 0 | 57.5 | 59.7 | 59.7 | 23.3 | 2.0 | rename 200 |

#### files written again

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 78.7 | 78.7 | 78.7 | 16.2 | 3.5 | create 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 0 | 11.0 | 18.2 | 22.6 | 18.3 | 11.1 | create 200 |
| notify | 200 | 200 | 0 | 0 | 0 | - | - | - | 27.1 | 0.0 |  |
| notify-fork | 200 | 0 | 0 | 0 | 414 | 172 | 290 | 300 | 24.9 | 297 | create 200 |
| chokidar | 200 | 200 | 0 | 0 | 0 | - | - | - | 26.8 | 0.0 |  |
| parcel | 200 | 0 | 0 | 0 | 0 | 89.8 | 89.8 | 89.8 | 25.7 | 9.7 | create 199, update 1 |
| fs-watch | 200 | 0 | 0 | 0 | 1 | 54.6 | 56.4 | 56.5 | 17.5 | 1.2 | rename 200 |

#### Notes

- notify, first edit, right after ready: missed for example `.../project/g00/d000/m0.js`
- notify, single edits: missed for example `.../project/g00/d000/m0.js`
- notify, single atomic saves: missed for example `.../project/g00/d000/m3.js`
- notify, single edits of the saved files: missed for example `.../project/g00/d000/m3.js`
- notify, burst of edits: missed for example `.../project/g00/d012/m0.js`
- notify, files removed: missed for example `.../project/g25/d020/m0.js`
- notify, files written again: missed for example `.../project/g00/d010/m1.js`
- notify-fork, burst of edits: missed for example `.../project/g14/d004/m0.js`
- chokidar, first edit, right after ready: missed for example `.../project/g00/d000/m0.js`
- chokidar, files written again: missed for example `.../project/g31/d020/m0.js`
- parcel: @parcel/watcher watches directories only: it watches .../project for 20000 paths
- parcel, single atomic saves: extra for example `delete .../project/g00/d000/m3.js.tmp`
- fs-watch: fs.watch is called once, for .../project, for 20000 paths
- fs-watch, single atomic saves: extra for example `rename .../project/g00/d000/m3.js.tmp`

### S2: project directory: one directory is watched

Sizes: dirs 5000, files 50000, watched 1.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 204 | 202 | 15.3 | +9.1 | 11.0 | 17.9 | 18.0 | 339 | 0 |
| pathwatch-0 | 190 | 189 | 15.3 | +9.1 | 11.0 | 18.1 | 18.1 | 591 | 0 |
| notify | 14.9 | 11.9 | 7.0 | +0.9 | 2.4 | 7.3 | 7.3 | 71.0 | 0 |
| notify-fork | 13.8 | 10.5 | 6.3 | +0.2 | 2.1 | 6.7 | 6.8 | 67.7 | 0 |
| chokidar | 115459 | 23481 | 2127.9 | +2078.3 | 1983.5 | 2146.1 | 2230.5 | 24201 | 4 |
| parcel | 83.8 | 84.2 | 50.0 | +0.4 | 18.0 | 61.9 | 61.9 | 314 | 0 |
| fs-watch | 80.3 | 80.5 | 48.2 | -0.1 | 16.3 | 53.9 | 54.0 | 181 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 66.5 | 66.5 | 66.5 | 0.0 | 0.2 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 12.7 | 12.7 | 12.7 | 0.0 | 0.2 | update 1 |
| notify | 1 | 0 | 0 | 0 | 1 | 11.8 | 11.8 | 11.8 | 0.0 | 0.1 | metadata 1 |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 12.2 | 12.2 | 12.2 | 0.0 | 0.1 | metadata 1 |
| chokidar | 1 | 1 | 0 | 0 | 0 | 1.4 | 1.4 | 1.4 | 0.0 | 398 |  |
| parcel | 1 | 0 | 0 | 0 | 0 | 12.9 | 12.9 | 12.9 | 0.0 | 0.4 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 0 | 11.7 | 11.7 | 11.7 | 0.0 | 0.4 | change 1 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 1.8 | 10005 | 0 |
| pathwatch-0 | 1.7 | 10004 | 0 |
| notify | 0.0 | 10005 | 0 |
| notify-fork | 0.0 | 10004 | 0 |
| chokidar | 485 | 10005 | 1 |
| parcel | 0.0 | 10005 | 0 |
| fs-watch | 0.0 | 10005 | 0 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 66.7 | 72.8 | 73.5 | 0.0 | 45.9 | update 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 0 | 12.7 | 12.9 | 13.1 | 0.0 | 52.8 | update 200 |
| notify | 200 | 0 | 0 | 0 | 201 | 12.5 | 12.7 | 13.0 | 0.0 | 25.0 | metadata 200 |
| notify-fork | 200 | 0 | 0 | 0 | 200 | 12.6 | 12.7 | 12.9 | 0.0 | 24.2 | metadata 200 |
| chokidar | 200 | 4 | 0 | 0 | 4 | 0.5 | 0.9 | 1.5 | 0.0 | 127 | update 196 |
| parcel | 200 | 0 | 0 | 0 | 0 | 69.9 | 74.3 | 74.6 | 0.0 | 85.1 | update 200 |
| fs-watch | 200 | 0 | 0 | 0 | 0 | 13.2 | 20.5 | 21.6 | 0.0 | 65.2 | change 200 |

#### new files in new directories

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5000 | 0 | 0 | 0 | 0 | 375 | 375 | 375 | 291 | 42.5 | create 5000 |
| pathwatch-0 | 5000 | 0 | 0 | 0 | 112 | 163 | 297 | 317 | 311 | 286 | create 5000 |
| notify | 5000 | 0 | 0 | 1000 | 10060 | 150 | 278 | 295 | 291 | 21.4 | create 5000 |
| notify-fork | 5000 | 0 | 0 | 1000 | 10068 | 157 | 282 | 298 | 296 | 20.9 | create 5000 |
| chokidar | 5000 | 5000 | 0 | 0 | 0 | - | - | - | 276 | 0.1 |  |
| parcel | 5000 | 0 | 0 | 500 | 0 | 356 | 356 | 356 | 292 | 106 | create 5000 |
| fs-watch | 5000 | 0 | 0 | 500 | 0 | 156 | 274 | 326 | 279 | 14.1 | rename 5000 |

#### a directory tree removed

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5500 | 0 | 0 | 0 | 0 | 420 | 421 | 421 | 248 | 13.3 | delete 5500 |
| pathwatch-0 | 5500 | 0 | 0 | 0 | 0 | 225 | 359 | 359 | 242 | 23.2 | delete 5500 |
| notify | 5500 | 0 | 0 | 565 | 210 | 124 | 226 | 247 | 237 | 11.1 | create 70, delete 5430 |
| notify-fork | 5500 | 0 | 0 | 553 | 30 | 120 | 225 | 240 | 236 | 10.9 | create 10, delete 5490 |
| chokidar | 5500 | 500 | 0 | 501 | 0 | 126 | 231 | 234 | 234 | 349 | delete 5000 |
| parcel | 5500 | 0 | 0 | 551 | 0 | 205 | 307 | 307 | 247 | 34.4 | delete 5500 |
| fs-watch | 5500 | 0 | 0 | 551 | 0 | 131 | 237 | 289 | 240 | 18.2 | rename 5500 |

#### a directory tree moved in

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5000 | 0 | 0 | 0 | 0 | 104 | 104 | 104 | 0.4 | 37.0 | create 5000 |
| pathwatch-0 | 5000 | 0 | 0 | 0 | 0 | 38.6 | 38.9 | 38.9 | 0.3 | 26.8 | create 5000 |
| notify | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.4 | 0.2 |  |
| notify-fork | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.4 | 0.3 |  |
| chokidar | 5000 | 5000 | 0 | 0 | 0 | - | - | - | 0.4 | 0.1 |  |
| parcel | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.4 | 0.6 |  |
| fs-watch | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.4 | 0.3 |  |

#### a directory tree moved out

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5000 | 0 | 0 | 0 | 0 | 175 | 176 | 176 | 0.4 | 6.2 | delete 5000 |
| pathwatch-0 | 5000 | 0 | 0 | 0 | 0 | 125 | 125 | 125 | 0.3 | 4.6 | delete 5000 |
| notify | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.4 | 0.2 |  |
| notify-fork | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.3 | 0.3 |  |
| chokidar | 5000 | 5000 | 0 | 0 | 0 | - | - | - | 0.3 | 0.0 |  |
| parcel | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.4 | 1.4 |  |
| fs-watch | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.4 | 0.3 |  |

#### Notes

- notify, a directory tree moved in: missed for example `.../project/moved/g00/d383/m3.js`
- notify, a directory tree moved out: missed for example `.../project/moved/g00/d080/m2.js`
- notify-fork, a directory tree moved in: missed for example `.../project/moved/g00/d208/m3.js`
- notify-fork, a directory tree moved out: missed for example `.../project/moved/g00/d047/m5.js`
- chokidar: 4 errors, the first: `Error: EMFILE: too many open files, watch`
- chokidar, first edit, right after ready: missed for example `.../project/g00/d000/m0.js`
- chokidar, single edits: missed for example `.../project/g00/d000/m0.js`
- chokidar, new files in new directories: missed for example `.../project/g03/new483/m8.js`
- chokidar, a directory tree removed: missed for example `.../project/g01/new491/m9.js`
- chokidar, a directory tree moved in: missed for example `.../project/moved/g00/d301/m2.js`
- chokidar, a directory tree moved out: missed for example `.../project/moved/g00/d348/m3.js`
- parcel, a directory tree moved in: missed for example `.../project/moved/g00/d101/m5.js`
- parcel, a directory tree moved out: missed for example `.../project/moved/g00/d439/m1.js`
- fs-watch, a directory tree moved in: missed for example `.../project/moved/g00/d413/m4.js`
- fs-watch, a directory tree moved out: missed for example `.../project/moved/g00/d214/m6.js`

### S3: noise: module files while another process writes

Sizes: dirs 2000, files 20000, watched 20000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 80.9 | 77.7 | 18.1 | +11.9 | 13.8 | 18.2 | 18.2 | 90.4 | 0 |
| pathwatch-0 | 81.6 | 78.9 | 18.1 | +11.9 | 13.8 | 18.2 | 18.3 | 90.1 | 0 |
| notify | 450 | 448 | 27.4 | +21.3 | 21.9 | 27.4 | 27.4 | 448 | 0 |
| notify-fork | 252 | 250 | 18.5 | +12.4 | 14.3 | 18.7 | 18.7 | 16952 | 0 |
| chokidar | 613 | 887 | 261.5 | +211.9 | 222.8 | 260.0 | 261.8 | 978 | 0 |
| parcel | 124 | 137 | 73.5 | +23.9 | 40.1 | 153.8 | 167.5 | 6217 | 0 |
| fs-watch | 124 | 137 | 71.9 | +23.7 | 38.9 | 72.9 | 73.0 | 308 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 72.7 | 72.7 | 72.7 | 0.0 | 0.2 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 12.4 | 12.4 | 12.4 | 0.0 | 0.2 | update 1 |
| notify | 1 | 1 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 14.2 | 14.2 | 14.2 | 0.0 | 1.9 | metadata 1 |
| chokidar | 1 | 1 | 0 | 0 | 0 | - | - | - | 0.0 | 44.6 |  |
| parcel | 1 | 0 | 0 | 0 | 0 | 13.5 | 13.5 | 13.5 | 0.0 | 0.3 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 0 | 12.7 | 12.7 | 12.7 | 0.0 | 0.6 | change 1 |

#### single edits under noise

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 60.4 | 60.7 | 62.3 | 0.0 | 11.1 | update 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 0 | 0.5 | 0.8 | 4.0 | 0.0 | 9.0 | update 100 |
| notify | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| notify-fork | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 15224 |  |
| chokidar | 100 | 0 | 0 | 0 | 0 | 0.1 | 0.2 | 0.8 | 0.0 | 10.7 | update 100 |
| parcel | 100 | 0 | 1097701 | 0 | 0 | 459 | 466 | 468 | 0.0 | 5997 | update 100 |
| fs-watch | 100 | 0 | 89106 | 0 | 0 | 8.7 | 11.8 | 17.7 | 0.0 | 151 | change 100 |

- pathwatch: noise files 50000, noise writes 214475, noise writes per second 22123
- pathwatch-0: noise files 50000, noise writes 82566, noise writes per second 20462
- notify: noise files 50000, noise writes 342387, noise writes per second 22178
- notify-fork: noise files 50000, noise writes 306647, noise writes per second 19851
- chokidar: noise files 50000, noise writes 86228, noise writes per second 21335
- parcel: noise files 50000, noise writes 1093967, noise writes per second 21848
- fs-watch: noise files 50000, noise writes 88057, noise writes per second 20946

#### quiet after the noise

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 0.6 | 1009 | 0 |
| pathwatch-0 | 0.5 | 1007 | 0 |
| notify | 0.0 | 1008 | 0 |
| notify-fork | 1010 | 1010 | 0 |
| chokidar | 0.0 | 1010 | 0 |
| parcel | 0.0 | 1006 | 0 |
| fs-watch | 0.0 | 1008 | 0 |

#### Notes

- notify, first edit, right after ready: missed for example `.../project/g00/d000/m0.js`
- notify, single edits under noise: missed for example `.../project/g00/d000/m0.js`
- notify-fork, single edits under noise: missed for example `.../project/g00/d000/m0.js`
- chokidar, first edit, right after ready: missed for example `.../project/g00/d000/m0.js`
- parcel: @parcel/watcher watches directories only: it watches .../project for 20000 paths
- parcel, single edits under noise: extra for example `create .../project/dist/n0000`
- fs-watch: fs.watch is called once, for .../project, for 20000 paths
- fs-watch, single edits under noise: extra for example `rename .../project/dist/n0000`

### S4: ignored directory: writes into node_modules

Sizes: dirs 500, files 5000, files written into node_modules 30000, watched 1.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 43.0 | 40.6 | 7.5 | +1.3 | 3.2 | 8.0 | 8.1 | 162 | 0 |
| pathwatch-0 | 41.2 | 39.1 | 7.5 | +1.3 | 3.2 | 8.1 | 8.2 | 196 | 0 |
| notify | 14.4 | 11.2 | 7.0 | +0.9 | 2.4 | 7.3 | 7.3 | 156 | 0 |
| notify-fork | 12.6 | 10.1 | 6.3 | +0.2 | 2.1 | 6.7 | 6.8 | 110 | 0 |
| chokidar | 315 | 524 | 176.5 | +126.9 | 151.6 | 180.2 | 180.3 | 770 | 0 |
| parcel | 85.8 | 86.5 | 50.5 | +0.9 | 18.4 | 50.8 | 50.9 | 275 | 0 |
| fs-watch | 80.3 | 80.2 | 48.3 | +0.1 | 16.4 | 55.2 | 55.2 | 169 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 71.7 | 71.7 | 71.7 | 0.0 | 0.2 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 11.0 | 11.0 | 11.0 | 0.0 | 0.2 | update 1 |
| notify | 1 | 0 | 0 | 0 | 1 | 12.5 | 12.5 | 12.5 | 0.0 | 0.1 | metadata 1 |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 12.6 | 12.6 | 12.6 | 0.0 | 0.1 | metadata 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 2.7 | 2.7 | 2.7 | 0.0 | 10.0 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 14.0 | 14.0 | 14.0 | 0.0 | 0.4 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 0 | 12.1 | 12.1 | 12.1 | 0.0 | 0.4 | change 1 |

#### node_modules written, then one edit in src

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 60.4 | 60.4 | 60.4 | 1695 | 114 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 0.4 | 0.4 | 0.4 | 1708 | 146 | update 1 |
| notify | 1 | 0 | 97515 | 0 | 1 | 7.9 | 7.9 | 7.9 | 1664 | 140 | metadata 1 |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 4.6 | 4.6 | 4.6 | 1714 | 96.8 | metadata 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 0.2 | 0.2 | 0.2 | 1590 | 154 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 7.1 | 7.1 | 7.1 | 1632 | 164 | update 1 |
| fs-watch | 1 | 0 | 33600 | 0 | 0 | 47.0 | 47.0 | 47.0 | 1617 | 76.9 | change 1 |

#### single edits in src

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 20 | 0 | 0 | 0 | 0 | 70.7 | 73.1 | 73.2 | 0.0 | 6.7 | update 20 |
| pathwatch-0 | 20 | 0 | 0 | 0 | 0 | 12.8 | 13.2 | 13.5 | 0.0 | 7.9 | update 20 |
| notify | 20 | 0 | 0 | 0 | 20 | 12.5 | 13.0 | 13.2 | 0.0 | 3.7 | metadata 20 |
| notify-fork | 20 | 0 | 0 | 0 | 20 | 12.4 | 13.0 | 13.4 | 0.0 | 3.6 | metadata 20 |
| chokidar | 20 | 0 | 0 | 0 | 6 | 0.8 | 1.1 | 1.2 | 0.0 | 58.4 | update 20 |
| parcel | 20 | 0 | 0 | 0 | 0 | 71.1 | 74.7 | 74.8 | 0.0 | 15.3 | update 20 |
| fs-watch | 20 | 0 | 0 | 0 | 0 | 13.1 | 16.3 | 20.9 | 0.0 | 6.8 | change 20 |

#### Notes

- notify: notify has no ignore rule: every event of the library is printed
- notify, node_modules written, then one edit in src: extra for example `create .../project/node_modules/package000`
- fs-watch: fs.watch has no ignore rule: every event is printed
- fs-watch, node_modules written, then one edit in src: extra for example `rename .../project/node_modules/package000`

### S5: polling: module files, polled every 100 ms

Sizes: dirs 2000, files 20000, watched 20000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 179 | 178 | 16.6 | +10.5 | 12.3 | 16.8 | 16.9 | 3919 | 0 |
| pathwatch-0 | 171 | 171 | 15.6 | +9.5 | 11.3 | 15.9 | 16.0 | 3873 | 0 |
| notify | 106 | 105 | 28.3 | +22.2 | 24.1 | 28.3 | 28.4 | 6602 | 0 |
| notify-fork | 39.9 | 38.8 | 22.0 | +15.9 | 17.8 | 28.5 | 28.6 | 9556 | 0 |
| chokidar | 370 | 757 | 277.3 | +227.7 | 239.9 | 275.2 | 277.8 | 15238 | 0 |
| parcel | unsupported: @parcel/watcher has no polling mode |
| fs-watch | 153 | 316 | 116.8 | +68.5 | 85.5 | 126.0 | 126.1 | 14717 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 124 | 124 | 124 | 0.0 | 124 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 125 | 125 | 125 | 0.0 | 125 | update 1 |
| notify | 1 | 0 | 0 | 0 | 0 | 37.6 | 37.6 | 37.6 | 0.0 | 45.8 | metadata 1 |
| notify-fork | 1 | 1 | 0 | 0 | 0 | - | - | - | 0.0 | 3375 |  |
| chokidar | 1 | 0 | 0 | 0 | 0 | 23.2 | 23.2 | 23.2 | 0.0 | 106 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 0 | 39.1 | 39.1 | 39.1 | 0.0 | 75.7 | change 1 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 948 | 10009 | 1 |
| pathwatch-0 | 939 | 10008 | 1 |
| notify | 3577 | 10001 | 0 |
| notify-fork | 3361 | 10006 | 0 |
| chokidar | 9444 | 10004 | 0 |
| fs-watch | 9329 | 10001 | 0 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 50 | 0 | 0 | 0 | 0 | 506 | 552 | 574 | 0.0 | 2668 | update 50 |
| pathwatch-0 | 50 | 0 | 0 | 0 | 0 | 517 | 562 | 570 | 0.0 | 2657 | update 50 |
| notify | 50 | 0 | 0 | 0 | 0 | 128 | 161 | 166 | 0.0 | 2826 | metadata 50 |
| notify-fork | 50 | 0 | 0 | 0 | 0 | 128 | 157 | 165 | 0.0 | 2699 | metadata 50 |
| chokidar | 50 | 0 | 0 | 0 | 0 | 62.3 | 67.7 | 101 | 0.0 | 4755 | update 50 |
| fs-watch | 50 | 0 | 0 | 0 | 0 | 62.3 | 70.3 | 103 | 0.0 | 4887 | change 50 |

#### Notes

- notify-fork, first edit, right after ready: missed for example `.../project/g00/d000/m0.js`
- fs-watch: polling: fs.watchFile for each path

### S6: missing paths: watched before they exist

Sizes: dirs 0, files 0, watched 1000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 13.0 | 10.8 | 7.1 | +0.9 | 2.8 | 7.4 | 7.5 | 25.8 | 0 |
| pathwatch-0 | 13.8 | 11.6 | 7.2 | +1.0 | 2.8 | 7.5 | 7.5 | 62.6 | 0 |
| notify | 13.5 | 12.1 | 6.3 | +0.2 | 2.1 | 6.3 | 6.3 | 12.4 | 4 |
| notify-fork | 25.1 | 23.2 | 7.2 | +1.1 | 3.0 | 7.4 | 7.5 | 169 | 0 |
| chokidar | 96.2 | 107 | 62.1 | +12.5 | 28.0 | 94.0 | 94.0 | 278 | 0 |
| parcel | 85.5 | 89.0 | 53.8 | +4.2 | 20.7 | 57.5 | 57.6 | 143 | 0 |
| fs-watch | 83.0 | 85.5 | 51.6 | +3.4 | 18.7 | 55.6 | 55.7 | 111 | 0 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 0.6 | 3006 | 0 |
| pathwatch-0 | 0.6 | 3006 | 0 |
| notify | 0.0 | 3008 | 0 |
| notify-fork | 0.0 | 3002 | 0 |
| chokidar | 0.0 | 3005 | 0 |
| parcel | 0.0 | 3008 | 0 |
| fs-watch | 0.0 | 3007 | 0 |

#### the missing paths are made

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1000 | 0 | 0 | 0 | 0 | 145 | 145 | 145 | 78.2 | 7.8 | create 1000 |
| pathwatch-0 | 1000 | 0 | 0 | 0 | 50 | 49.1 | 77.0 | 81.7 | 79.1 | 42.3 | create 1000 |
| notify | 1000 | 1000 | 0 | 0 | 0 | - | - | - | 77.3 | 0.0 |  |
| notify-fork | 1000 | 0 | 0 | 0 | 2014 | 71.0 | 584 | 593 | 76.3 | 132 | create 1000 |
| chokidar | 1000 | 1000 | 0 | 0 | 0 | - | - | - | 77.0 | 58.2 |  |
| parcel | 1000 | 0 | 0 | 110 | 0 | 142 | 142 | 143 | 78.9 | 29.7 | create 1000 |
| fs-watch | 1000 | 0 | 0 | 110 | 1 | 56.0 | 121 | 121 | 75.8 | 10.3 | rename 1000 |

#### single edits after

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 20 | 0 | 0 | 0 | 0 | 70.7 | 73.1 | 73.2 | 0.0 | 6.5 | update 20 |
| pathwatch-0 | 20 | 0 | 0 | 0 | 0 | 12.7 | 13.1 | 13.2 | 0.0 | 7.6 | update 20 |
| notify | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| notify-fork | 20 | 0 | 0 | 0 | 60 | 12.7 | 13.5 | 13.7 | 0.0 | 10.6 | create 20 |
| chokidar | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 0.6 |  |
| parcel | 20 | 0 | 0 | 0 | 0 | 71.4 | 74.8 | 74.8 | 0.0 | 16.7 | update 20 |
| fs-watch | 20 | 0 | 0 | 0 | 0 | 13.1 | 19.2 | 20.2 | 0.0 | 8.6 | rename 20 |

#### Notes

- notify: 4 errors, the first: `watch .../project/g00/d000/m0.js: No path was found. about [".../project/g00/d000/m0.js"]`
- notify, the missing paths are made: missed for example `.../project/g05/d005/m5.js`
- notify, single edits after: missed for example `.../project/g00/d000/m0.js`
- chokidar, the missing paths are made: missed for example `.../project/g04/d003/m3.js`
- chokidar, single edits after: missed for example `.../project/g00/d000/m0.js`
- parcel: @parcel/watcher watches directories only: it watches .../project for 1000 paths
- fs-watch: fs.watch is called once, for .../project, for 1000 paths

### S7: big directory: one watched directory holds every file

Sizes: dirs 1, files 20000, watched 1.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 80.5 | 63.3 | 10.6 | +4.4 | 6.3 | 11.8 | 11.9 | 2181 | 0 |
| pathwatch-0 | 81.3 | 64.0 | 10.6 | +4.4 | 6.3 | 11.7 | 11.8 | 2045 | 0 |
| notify | 28.4 | 10.6 | 7.0 | +0.9 | 2.4 | 7.5 | 7.5 | 119 | 0 |
| notify-fork | 26.4 | 9.4 | 6.3 | +0.2 | 2.1 | 7.0 | 7.0 | 169 | 0 |
| chokidar | 717 | 1059 | 184.6 | +135.0 | 160.5 | 203.6 | 225.0 | 63276 | 0 |
| parcel | 98.5 | 84.5 | 50.0 | +0.4 | 17.9 | 54.1 | 55.7 | 581 | 12 |
| fs-watch | 81.2 | 81.2 | 48.3 | +0.0 | 16.4 | 53.8 | 55.4 | 302 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 266 | 266 | 266 | 0.0 | 0.3 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 148 | 148 | 148 | 0.0 | 0.2 | update 1 |
| notify | 1 | 0 | 0 | 0 | 1 | 210 | 210 | 210 | 0.0 | 0.2 | metadata 1 |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 98.8 | 98.8 | 98.8 | 0.0 | 0.2 | metadata 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 0.4 | 0.4 | 0.4 | 0.0 | 0.6 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 201 | 201 | 201 | 0.0 | 1.1 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 0 | 267 | 267 | 267 | 0.0 | 1.1 | change 1 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 73.2 | 241 | 487 | 0.0 | 196 | update 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 0 | 16.8 | 125 | 426 | 0.0 | 181 | update 100 |
| notify | 100 | 0 | 43112 | 2 | 212 | 14.8 | 225 | 371 | 0.0 | 60.7 | create 7, metadata 93 |
| notify-fork | 100 | 0 | 46496 | 2 | 248 | 15.0 | 253 | 468 | 0.0 | 57.9 | create 22, metadata 78 |
| chokidar | 100 | 0 | 14375 | 0 | 135 | 3.9 | 5.6 | 39.8 | 0.0 | 7815 | update 100 |
| parcel | 100 | 2 | 5821 | 1 | 1 | 72.7 | 77.5 | 1070 | 0.0 | 320 | create 2, update 96 |
| fs-watch | 100 | 0 | 14123 | 0 | 23 | 17.4 | 122 | 400 | 0.0 | 129 | change 63, rename 37 |

#### single new files

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 89.4 | 93.2 | 94.7 | 0.0 | 1887 | create 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 0 | 28.7 | 31.5 | 34.5 | 0.0 | 1617 | create 100 |
| notify | 100 | 0 | 0 | 0 | 200 | 12.7 | 13.3 | 15.1 | 0.0 | 17.8 | create 100 |
| notify-fork | 100 | 0 | 0 | 0 | 200 | 12.9 | 15.0 | 16.4 | 0.0 | 31.4 | create 100 |
| chokidar | 100 | 0 | 688 | 0 | 4 | 144 | 150 | 212 | 0.0 | 26030 | create 100 |
| parcel | 100 | 0 | 0 | 0 | 0 | 72.3 | 74.9 | 75.1 | 0.0 | 72.5 | create 100 |
| fs-watch | 100 | 0 | 0 | 0 | 0 | 13.2 | 20.6 | 22.2 | 0.0 | 47.9 | rename 100 |

#### burst of new files

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 2000 | 0 | 0 | 0 | 0 | 196 | 202 | 202 | 126 | 20.1 | create 2000 |
| pathwatch-0 | 2000 | 0 | 0 | 0 | 0 | 103 | 166 | 173 | 160 | 173 | create 2000 |
| notify | 2000 | 0 | 0 | 0 | 4088 | 77.9 | 125 | 137 | 130 | 10.1 | create 2000 |
| notify-fork | 2000 | 0 | 0 | 0 | 4059 | 73.8 | 124 | 129 | 129 | 42.7 | create 2000 |
| chokidar | 2000 | 0 | 0 | 0 | 65 | 187 | 215 | 215 | 116 | 955 | create 2000 |
| parcel | 2000 | 0 | 0 | 0 | 0 | 201 | 201 | 201 | 130 | 42.0 | create 2000 |
| fs-watch | 2000 | 0 | 0 | 0 | 0 | 85.6 | 123 | 174 | 125 | 4.2 | rename 2000 |

#### single files removed

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 179 | 183 | 183 | 0.0 | 46.0 | delete 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 0 | 122 | 123 | 124 | 0.0 | 38.3 | delete 100 |
| notify | 100 | 0 | 0 | 0 | 138 | 12.8 | 13.2 | 13.5 | 0.0 | 17.7 | create 5, delete 95 |
| notify-fork | 100 | 0 | 0 | 0 | 0 | 12.7 | 13.2 | 13.6 | 0.0 | 30.0 | delete 100 |
| chokidar | 100 | 0 | 0 | 0 | 0 | 105 | 106 | 107 | 0.0 | 27398 | delete 100 |
| parcel | 100 | 0 | 0 | 0 | 0 | 71.7 | 74.7 | 74.9 | 0.0 | 57.2 | delete 100 |
| fs-watch | 100 | 0 | 0 | 0 | 0 | 13.1 | 20.3 | 21.8 | 0.0 | 33.5 | rename 100 |

#### Notes

- notify, single edits: extra for example `create .../project/m00245.js`
- notify-fork, single edits: extra for example `create .../project/m00001.js`
- chokidar, single edits: extra for example `update .../project/m00001.js`
- chokidar, single new files: extra for example `update .../project/m18887.js`
- parcel: 12 errors, the first: `Error: Events were dropped by the FSEvents client. File system must be re-scanned.`
- parcel, single edits: missed for example `.../project/m00600.js`
- parcel, single edits: extra for example `create .../project/m00001.js`
- fs-watch, single edits: extra for example `change .../project/project`

## Linux

- Started: 2026-10-08T07:57:40Z
- Machine: aarch64, 13 cores, 15.7 GB, Ubuntu 26.04 LTS (Linux 7.0.14-orbstack-00380-ga7e0a2dc9535 aarch64), temporary directory on tmpfs
- Open files limit of the adapters: 1048576
- Transparent huge pages: madvise; for malloc of the adapters: off
- Load average at the start: 0.0 0.0 0.0
- Sizes: the full sizes; a quiet phase is 10 s
- Each scenario ran 3 times with each adapter: a time or a size is the middle one of the runs, a count (missed, extra, errors) is the worst one.
- Times are in milliseconds, memory is in MiB.

### Versions

| name | version |
| --- | ---: |
| chokidar | 5.0.0 |
| fs-watch | part of node |
| node | v22.22.1 |
| notify | 8.2.0 |
| notify-fork | 11.0.0 (git 3a1cde09) |
| parcel | 2.6.0 |
| pathwatch | 0.0.0 (working tree, acbff387) |
| rustc | rustc 1.99.0 (b940084d7 2026-09-28) |

### Bases: each adapter when it watches nothing

| adapter | start to ready | CPU | RSS | footprint |
| --- | ---: | ---: | ---: | ---: |
| chokidar | 46.7 | 47.3 | 59.1 | 57.3 |
| fs-watch | 43.4 | 44.1 | 58.6 | 56.8 |
| node-idle | 43.1 | 43.8 | 58.5 | 56.8 |
| notify | 0.5 | 0.5 | 2.0 | 0.7 |
| notify-fork | 0.5 | 0.5 | 2.0 | 0.7 |
| parcel | 50.9 | 53.0 | 65.9 | 64.2 |
| pathwatch | 0.4 | 0.4 | 2.2 | 0.9 |
| pathwatch-0 | 0.5 | 0.5 | 2.2 | 0.9 |

### Where pathwatch is behind

Every number in which pathwatch is more than 10% behind the best other library, and by more than 1 ms or 0.5 MiB. Only a library that did the job counts: it missed no change of the phase, and for the setup it stopped early in no phase of the run. `pathwatch` has the default settings, and `pathwatch-0` is the same without the 50 ms that pathwatch waits on purpose. `times` is `pathwatch-0` next to the best other, and `pathwatch` where there is no `pathwatch-0`. With more runs than one, a number is `middle (lowest to highest)`.

#### Behind with and without the wait

| scenario | where | number | pathwatch | pathwatch-0 | best other | which | times |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| S1 | setup | memory above base | 7.0 (6.8 to 7.1) | 7.0 (6.9 to 7.0) | 5.8 (5.7 to 5.9) | notify-fork | 1.2 |
| S1 | quiet | CPU | 1.3 (1.2 to 1.4) | 1.5 (1.3 to 1.7) | 0.0 (0.0 to 0.0) | notify | - |
| S1 | single edits | CPU | 72.0 (70.4 to 74.7) | 47.1 (44.9 to 48.1) | 27.0 (22.2 to 27.7) | notify-fork | 1.7 |
| S1 | single atomic saves | p50 | 59.6 (59.2 to 59.7) | 1.5 (1.4 to 1.5) | 0.4 (0.3 to 0.4) | notify | 4.2 |
| S1 | single atomic saves | max | 65.5 (65.5 to 65.6) | 5.5 (5.4 to 5.6) | 0.5 (0.5 to 0.8) | notify | 10.2 |
| S1 | single atomic saves | CPU | 18.9 (18.4 to 20.5) | 11.9 (11.4 to 12.0) | 5.9 (5.3 to 17.6) | notify | 2.0 |
| S1 | single edits of the saved files | CPU | 18.6 (15.7 to 19.7) | 11.3 (10.7 to 11.8) | 7.0 (5.2 to 7.2) | notify-fork | 1.6 |
| S1 | burst of edits | p50 | 91.6 (88.4 to 99.5) | 18.3 (12.5 to 22.1) | 12.2 (10.9 to 18.5) | notify-fork | 1.5 |
| S1 | burst of edits | max | 92.0 (89.3 to 100) | 31.1 (22.5 to 32.2) | 21.0 (21.0 to 27.4) | notify-fork | 1.5 |
| S1 | files removed | p50 | 172 (171 to 172) | 113 (106 to 113) | 1.9 (1.3 to 2.5) | notify-fork | 59.5 |
| S1 | files removed | max | 172 (171 to 172) | 113 (106 to 113) | 3.4 (2.6 to 4.0) | notify-fork | 33.6 |
| S1 | files written again | p50 | 61.6 (60.3 to 67.3) | 7.3 (7.0 to 8.0) | 3.2 (2.0 to 3.3) | notify-fork | 2.3 |
| S1 | files written again | max | 61.7 (60.4 to 67.4) | 7.3 (7.1 to 8.1) | 5.1 (3.5 to 6.2) | notify-fork | 1.4 |
| S2 | setup | time | 96.7 (96.1 to 99.1) | 94.6 (93.6 to 95.5) | 65.0 (64.9 to 66.2) | notify-fork | 1.5 |
| S2 | setup | CPU | 96.3 (95.9 to 99.0) | 94.5 (93.5 to 95.1) | 64.9 (64.7 to 66.1) | notify-fork | 1.5 |
| S2 | setup | memory above base | 5.6 (5.5 to 5.6) | 5.6 (5.5 to 5.6) | 2.0 (1.9 to 2.0) | notify | 2.9 |
| S2 | quiet | CPU | 1.4 (1.3 to 1.5) | 1.4 (1.3 to 1.4) | 0.0 (0.0 to 0.0) | notify | - |
| S2 | single edits | max | 65.5 (65.5 to 65.6) | 1.7 (0.8 to 2.4) | 0.6 (0.5 to 1.7) | notify-fork | 2.7 |
| S2 | single edits | CPU | 68.9 (68.7 to 69.3) | 42.6 (42.2 to 43.5) | 26.3 (25.8 to 28.4) | notify-fork | 1.6 |
| S2 | a directory tree removed | p50 | 202 (198 to 202) | 133 (128 to 136) | 12.6 (11.7 to 13.4) | notify-fork | 10.6 |
| S2 | a directory tree removed | max | 204 (200 to 205) | 136 (130 to 138) | 21.7 (20.4 to 22.4) | notify-fork | 6.3 |
| S2 | a directory tree moved out | p50 | 177 (175 to 180) | 120 (120 to 121) | 106 (104 to 112) | chokidar | 1.1 |
| S2 | a directory tree moved out | max | 179 (178 to 182) | 122 (121 to 122) | 107 (105 to 113) | chokidar | 1.1 |
| S3 | setup | memory above base | 7.1 (7.1 to 7.1) | 7.2 (7.1 to 7.7) | 6.0 (5.9 to 6.0) | notify-fork | 1.2 |
| S3 | single edits under noise | CPU | 5.6 (5.2 to 5.8) | 3.5 (2.7 to 3.8) | 2.0 (2.0 to 2.1) | notify | 1.7 |
| S4 | setup | time | 22.8 (22.2 to 30.5) | 24.5 (22.1 to 24.6) | 15.4 (14.8 to 21.2) | notify-fork | 1.6 |
| S4 | setup | CPU | 22.7 (22.4 to 30.2) | 24.5 (22.0 to 24.6) | 15.3 (14.8 to 22.1) | notify-fork | 1.6 |
| S4 | single edits in src | CPU | 8.0 (7.9 to 8.6) | 4.7 (4.4 to 4.9) | 2.6 (2.5 to 3.0) | notify-fork | 1.8 |
| S5 | setup | time | 84.8 (84.8 to 88.7) | 85.7 (84.5 to 90.0) | 48.3 (48.1 to 50.3) | notify-fork | 1.8 |
| S5 | setup | CPU | 82.2 (81.0 to 84.7) | 82.0 (81.5 to 86.1) | 45.5 (44.9 to 46.8) | notify-fork | 1.8 |
| S5 | first edit, right after ready | p50 | 75.3 (74.2 to 78.4) | 73.8 (66.9 to 73.8) | 31.7 (26.5 to 32.1) | fs-watch | 2.3 |
| S5 | first edit, right after ready | max | 75.3 (74.2 to 78.4) | 73.8 (66.9 to 73.8) | 31.7 (26.5 to 32.1) | fs-watch | 2.3 |
| S5 | first edit, right after ready | CPU | 36.4 (35.2 to 36.7) | 35.7 (35.7 to 35.8) | 28.8 (28.7 to 29.6) | notify | 1.2 |
| S5 | single edits | p50 | 416 (414 to 418) | 415 (413 to 416) | 62.8 (61.7 to 63.0) | chokidar | 6.6 |
| S5 | single edits | max | 472 (447 to 477) | 449 (440 to 475) | 90.5 (80.0 to 98.1) | chokidar | 5.0 |
| S7 | setup | time | 48.8 (48.6 to 52.1) | 48.3 (48.1 to 49.0) | 25.1 (22.5 to 25.5) | notify-fork | 1.9 |
| S7 | setup | CPU | 48.5 (48.5 to 51.9) | 48.2 (48.1 to 49.0) | 24.9 (22.3 to 25.0) | notify-fork | 1.9 |
| S7 | setup | memory above base | 3.2 (3.2 to 3.2) | 3.2 (3.2 to 3.2) | 0.3 (0.3 to 0.3) | notify | 10.9 |
| S7 | single edits | CPU | 35.9 (33.7 to 37.8) | 20.4 (20.0 to 20.8) | 12.7 (12.3 to 12.8) | notify | 1.6 |
| S7 | single new files | CPU | 35.5 (34.2 to 36.3) | 21.7 (21.4 to 22.8) | 11.5 (11.0 to 11.9) | notify | 1.9 |
| S7 | burst of new files | p50 | 90.3 (88.3 to 92.9) | 19.7 (15.8 to 21.5) | 14.6 (14.2 to 14.8) | notify | 1.4 |
| S7 | burst of new files | max | 90.8 (89.2 to 94.8) | 28.8 (21.7 to 30.8) | 24.3 (22.9 to 25.9) | notify | 1.2 |
| S7 | single files removed | p50 | 165 (165 to 165) | 109 (109 to 109) | 0.2 (0.2 to 0.2) | notify | 510.6 |
| S7 | single files removed | max | 171 (171 to 172) | 111 (111 to 111) | 0.4 (0.4 to 0.5) | notify-fork | 254.7 |
| S7 | single files removed | CPU | 53.7 (52.2 to 53.9) | 40.1 (39.5 to 40.3) | 10.9 (10.9 to 12.1) | notify-fork | 3.7 |

#### Behind only with the wait: the price of the 50 ms

| scenario | where | number | pathwatch | pathwatch-0 | best other | which | times |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| S1 | first edit, right after ready | p50 | 60.2 (54.6 to 60.2) | 0.1 (0.1 to 0.1) | 0.0 (0.0 to 0.1) | notify | - |
| S1 | first edit, right after ready | max | 60.2 (54.6 to 60.2) | 0.1 (0.1 to 0.1) | 0.0 (0.0 to 0.1) | notify | - |
| S1 | single edits | p50 | 59.0 (58.8 to 59.0) | 0.3 (0.3 to 0.3) | 0.3 (0.3 to 0.3) | notify | 1.2 |
| S1 | single edits | max | 65.5 (65.5 to 65.6) | 0.8 (0.6 to 1.2) | 1.1 (0.6 to 1.4) | notify | 0.8 |
| S1 | single edits of the saved files | p50 | 59.4 (59.2 to 60.5) | 0.3 (0.3 to 0.3) | 0.3 (0.3 to 0.3) | notify-fork | 1.2 |
| S1 | single edits of the saved files | max | 65.4 (64.8 to 66.1) | 0.5 (0.5 to 0.6) | 0.9 (0.5 to 1.1) | notify-fork | 0.6 |
| S1 | files removed | CPU | 4.6 (4.3 to 5.0) | 3.9 (3.9 to 4.1) | 3.2 (2.0 to 3.2) | notify-fork | 1.2 |
| S2 | first edit, right after ready | p50 | 60.2 (60.2 to 60.2) | 0.1 (0.1 to 0.1) | 0.3 (0.3 to 0.4) | parcel | 0.3 |
| S2 | first edit, right after ready | max | 60.2 (60.2 to 60.2) | 0.1 (0.1 to 0.1) | 0.3 (0.3 to 0.4) | parcel | 0.3 |
| S2 | single edits | p50 | 58.9 (58.8 to 59.7) | 0.3 (0.3 to 0.3) | 0.3 (0.3 to 0.3) | notify | 1.2 |
| S2 | a directory tree moved in | p50 | 84.3 (82.1 to 85.7) | 19.2 (19.1 to 20.1) | 62.6 (60.8 to 64.8) | fs-watch | 0.3 |
| S2 | a directory tree moved in | max | 86.6 (83.2 to 86.9) | 20.9 (20.9 to 22.0) | 62.9 (61.0 to 65.3) | fs-watch | 0.3 |
| S3 | first edit, right after ready | p50 | 60.2 (60.2 to 60.2) | 0.1 (0.1 to 0.1) | 0.1 (0.0 to 0.1) | notify | 2.6 |
| S3 | first edit, right after ready | max | 60.2 (60.2 to 60.2) | 0.1 (0.1 to 0.1) | 0.1 (0.0 to 0.1) | notify | 2.6 |
| S3 | single edits under noise | p50 | 60.1 (60.1 to 60.2) | 0.1 (0.0 to 0.1) | 0.0 (0.0 to 0.0) | notify | - |
| S3 | single edits under noise | max | 65.1 (60.3 to 65.1) | 0.2 (0.1 to 0.2) | 0.1 (0.1 to 0.1) | notify | 1.7 |
| S4 | first edit, right after ready | p50 | 60.1 (57.3 to 60.5) | 0.1 (0.1 to 0.2) | 0.3 (0.3 to 0.4) | parcel | 0.5 |
| S4 | first edit, right after ready | max | 60.1 (57.3 to 60.5) | 0.1 (0.1 to 0.2) | 0.3 (0.3 to 0.4) | parcel | 0.5 |
| S4 | node_modules written, then one edit in src | p50 | 60.2 (60.2 to 65.2) | 0.1 (0.1 to 0.1) | 0.0 (0.0 to 0.1) | notify | - |
| S4 | node_modules written, then one edit in src | max | 60.2 (60.2 to 65.2) | 0.1 (0.1 to 0.1) | 0.0 (0.0 to 0.1) | notify | - |
| S4 | single edits in src | p50 | 60.1 (59.2 to 60.5) | 0.3 (0.3 to 0.4) | 0.3 (0.3 to 0.3) | notify-fork | 1.2 |
| S4 | single edits in src | max | 65.3 (65.1 to 65.4) | 0.6 (0.6 to 1.0) | 0.4 (0.4 to 0.4) | notify | 1.5 |
| S6 | the missing paths are made | p50 | 77.2 (76.3 to 78.1) | 9.2 (9.0 to 10.0) | 44.9 (41.8 to 47.0) | fs-watch | 0.2 |
| S6 | the missing paths are made | max | 77.7 (76.7 to 79.1) | 12.5 (11.7 to 13.3) | 45.0 (42.4 to 47.5) | fs-watch | 0.3 |
| S6 | single edits after | p50 | 58.5 (58.4 to 60.5) | 0.3 (0.3 to 0.3) | 1.1 (0.9 to 1.1) | fs-watch | 0.3 |
| S6 | single edits after | max | 62.7 (60.9 to 65.5) | 0.5 (0.5 to 0.6) | 1.9 (1.7 to 3.9) | fs-watch | 0.2 |
| S7 | first edit, right after ready | p50 | 60.4 (60.4 to 60.4) | 0.1 (0.1 to 0.2) | 0.1 (0.1 to 0.1) | notify | 1.5 |
| S7 | first edit, right after ready | max | 60.4 (60.4 to 60.4) | 0.1 (0.1 to 0.2) | 0.1 (0.1 to 0.1) | notify | 1.5 |
| S7 | single edits | p50 | 59.6 (59.5 to 59.6) | 0.3 (0.3 to 0.3) | 0.3 (0.3 to 0.3) | notify | 1.1 |
| S7 | single edits | max | 65.4 (65.4 to 65.6) | 0.6 (0.5 to 1.0) | 0.6 (0.6 to 0.7) | notify-fork | 0.9 |
| S7 | single new files | p50 | 59.6 (59.1 to 60.4) | 0.3 (0.3 to 0.3) | 0.3 (0.2 to 0.3) | notify | 1.1 |
| S7 | single new files | max | 65.4 (65.1 to 65.5) | 0.5 (0.5 to 0.7) | 0.5 (0.5 to 0.7) | notify-fork | 1.0 |

#### Behind only without the wait

In no number.

Of 107 numbers that can be compared, `pathwatch` is the best or near it (inside the limits above) in 30, and `pathwatch-0` in 62.

pathwatch missed no change, gave no event too many, and no error.

### S1: module files: every file is watched by its own path

Sizes: dirs 2000, files 20000, watched 20000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 63.3 | 60.7 | 9.1 | +6.8 | 7.8 | 9.2 | 9.2 | 197 | 0 |
| pathwatch-0 | 55.4 | 52.2 | 9.1 | +6.8 | 7.8 | 9.1 | 9.2 | 149 | 0 |
| notify | 492 | 327 | 9.9 | +7.9 | 8.7 | 10.0 | 10.0 | 396 | 0 |
| notify-fork | 407 | 263 | 7.8 | +5.7 | 6.5 | 7.8 | 7.8 | 325 | 0 |
| chokidar | 444 | 775 | 231.5 | +172.5 | 230.3 | 165.0 | 232.4 | 1758 | 0 |
| parcel | 151 | 173 | 85.8 | +19.8 | 84.5 | 80.7 | 85.3 | 551 | 0 |
| fs-watch | 248 | 325 | 132.7 | +74.2 | 131.5 | 125.2 | 137.9 | 1223 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 60.2 | 60.2 | 60.2 | 0.0 | 0.1 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 0.1 | 0.1 | 0.1 | 0.0 | 0.1 | update 1 |
| notify | 1 | 0 | 0 | 0 | 1 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | update 1 |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | update 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 0.6 | 0.6 | 0.6 | 0.0 | 0.8 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 0.3 | 0.3 | 0.3 | 0.0 | 0.3 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 1 | 0.4 | 0.4 | 0.4 | 0.0 | 0.4 | change 1 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 1.3 | 10010 | 0 |
| pathwatch-0 | 1.5 | 10008 | 0 |
| notify | 0.0 | 10010 | 0 |
| notify-fork | 0.0 | 10007 | 0 |
| chokidar | 322 | 10008 | 0 |
| parcel | 60.2 | 10006 | 0 |
| fs-watch | 187 | 10004 | 0 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 59.0 | 64.4 | 65.5 | 0.0 | 72.0 | update 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 65 | 0.3 | 0.5 | 0.8 | 0.0 | 47.1 | update 200 |
| notify | 200 | 0 | 0 | 0 | 313 | 0.3 | 0.4 | 1.1 | 0.0 | 27.8 | update 200 |
| notify-fork | 200 | 0 | 0 | 0 | 282 | 0.3 | 0.4 | 1.4 | 0.0 | 27.0 | update 200 |
| chokidar | 200 | 0 | 0 | 0 | 0 | 0.9 | 1.2 | 3.7 | 0.0 | 219 | update 200 |
| parcel | 200 | 0 | 0 | 0 | 0 | 59.4 | 61.2 | 61.8 | 0.0 | 175 | update 200 |
| fs-watch | 200 | 0 | 0 | 0 | 200 | 0.9 | 1.3 | 2.1 | 0.0 | 190 | change 200 |

#### single atomic saves

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 50 | 0 | 0 | 0 | 0 | 59.6 | 64.2 | 65.5 | 0.0 | 18.9 | update 50 |
| pathwatch-0 | 50 | 0 | 0 | 0 | 0 | 1.5 | 5.3 | 5.5 | 0.0 | 11.9 | update 50 |
| notify | 50 | 0 | 0 | 0 | 50 | 0.4 | 0.5 | 0.5 | 0.0 | 5.9 | metadata 50 |
| notify-fork | 50 | 0 | 0 | 0 | 0 | 0.4 | 0.5 | 0.9 | 0.0 | 8.6 | rename 50 |
| chokidar | 50 | 0 | 0 | 0 | 0 | 1.4 | 2.3 | 3.0 | 0.0 | 77.5 | update 50 |
| parcel | 50 | 0 | 0 | 0 | 0 | 59.6 | 61.2 | 61.4 | 0.0 | 42.7 | create 50 |
| fs-watch | 50 | 0 | 0 | 0 | 100 | 0.9 | 1.3 | 1.5 | 0.0 | 42.7 | change 50 |

#### single edits of the saved files

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 50 | 0 | 0 | 0 | 0 | 59.4 | 65.1 | 65.4 | 0.0 | 18.6 | update 50 |
| pathwatch-0 | 50 | 0 | 0 | 0 | 16 | 0.3 | 0.5 | 0.5 | 0.0 | 11.3 | update 50 |
| notify | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| notify-fork | 50 | 0 | 0 | 0 | 69 | 0.3 | 0.5 | 0.9 | 0.0 | 7.0 | update 50 |
| chokidar | 50 | 0 | 0 | 0 | 0 | 0.9 | 1.2 | 2.2 | 0.0 | 57.1 | update 50 |
| parcel | 50 | 0 | 0 | 0 | 0 | 59.4 | 61.1 | 61.2 | 0.0 | 42.1 | update 50 |
| fs-watch | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 2.3 |  |

#### burst of edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 2000 | 0 | 0 | 0 | 0 | 91.6 | 92.0 | 92.0 | 22.5 | 14.8 | update 2000 |
| pathwatch-0 | 2000 | 0 | 0 | 0 | 19 | 18.3 | 30.1 | 31.1 | 30.5 | 13.1 | update 2000 |
| notify | 2000 | 16 | 0 | 0 | 2712 | 15.2 | 23.7 | 24.5 | 24.5 | 17.5 | update 1984 |
| notify-fork | 2000 | 0 | 0 | 0 | 2537 | 12.2 | 20.2 | 21.0 | 20.9 | 20.5 | update 2000 |
| chokidar | 2000 | 0 | 0 | 0 | 0 | 60.5 | 60.8 | 60.9 | 18.3 | 125 | update 2000 |
| parcel | 2000 | 0 | 0 | 0 | 0 | 95.3 | 96.1 | 96.1 | 22.5 | 30.6 | update 2000 |
| fs-watch | 2000 | 16 | 0 | 0 | 1984 | 98.7 | 99.4 | 99.4 | 22.7 | 106 | change 1984 |

#### files removed

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 172 | 172 | 172 | 2.9 | 4.6 | delete 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 0 | 113 | 113 | 113 | 2.9 | 3.9 | delete 200 |
| notify | 200 | 25 | 0 | 0 | 175 | 2.2 | 3.7 | 3.9 | 3.8 | 3.4 | metadata 175 |
| notify-fork | 200 | 0 | 0 | 0 | 0 | 1.9 | 3.3 | 3.4 | 3.3 | 3.2 | delete 200 |
| chokidar | 200 | 0 | 0 | 0 | 0 | 128 | 128 | 128 | 3.6 | 178 | delete 200 |
| parcel | 200 | 0 | 0 | 0 | 0 | 58.0 | 58.0 | 58.0 | 2.8 | 5.8 | delete 200 |
| fs-watch | 200 | 25 | 0 | 0 | 0 | 297 | 297 | 297 | 2.0 | 327 | rename 175 |

#### files written again

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 61.6 | 61.6 | 61.7 | 4.1 | 2.7 | create 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 1 | 7.3 | 7.3 | 7.3 | 4.8 | 3.1 | create 200 |
| notify | 200 | 200 | 0 | 0 | 0 | - | - | - | 4.0 | 0.0 |  |
| notify-fork | 200 | 0 | 0 | 0 | 400 | 3.2 | 5.1 | 5.1 | 4.7 | 4.3 | create 200 |
| chokidar | 200 | 200 | 0 | 0 | 0 | - | - | - | 3.9 | 0.0 |  |
| parcel | 200 | 0 | 0 | 0 | 0 | 66.2 | 66.3 | 66.3 | 4.6 | 6.2 | create 200 |
| fs-watch | 200 | 25 | 0 | 0 | 0 | 20.6 | 20.7 | 20.7 | 4.1 | 27.9 | rename 175 |

#### Notes

- notify, single edits of the saved files: missed for example `.../project/g00/d000/m3.js`
- notify, burst of edits: missed for example `.../project/g28/d000/m0.js`
- notify, files removed: missed for example `.../project/g32/d040/m0.js`
- notify, files written again: missed for example `.../project/g36/d030/m0.js`
- chokidar, files written again: missed for example `.../project/g26/d030/m0.js`
- parcel: @parcel/watcher watches directories only: it watches .../project for 20000 paths
- fs-watch: fs.watch is called once, for .../project, for 20000 paths
- fs-watch, single edits of the saved files: missed for example `.../project/g00/d000/m3.js`
- fs-watch, burst of edits: missed for example `.../project/g27/d010/m0.js`
- fs-watch, files removed: missed for example `.../project/g19/d010/m1.js`
- fs-watch, files written again: missed for example `.../project/g31/d010/m0.js`

### S2: project directory: one directory is watched

Sizes: dirs 5000, files 50000, watched 1.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 96.7 | 96.3 | 7.7 | +5.4 | 6.4 | 8.8 | 8.8 | 243 | 0 |
| pathwatch-0 | 94.6 | 94.5 | 7.7 | +5.4 | 6.5 | 8.9 | 8.9 | 217 | 0 |
| notify | 93.1 | 92.7 | 3.9 | +1.8 | 2.7 | 4.1 | 4.3 | 221 | 0 |
| notify-fork | 65.0 | 64.9 | 3.9 | +1.9 | 2.7 | 4.2 | 4.2 | 190 | 0 |
| chokidar | 1702 | 3026 | 488.1 | +429.0 | 487.0 | 420.5 | 487.3 | 6637 | 0 |
| parcel | 156 | 158 | 82.1 | +16.1 | 80.9 | 89.8 | 89.8 | 923 | 0 |
| fs-watch | 368 | 460 | 181.3 | +122.7 | 180.1 | 213.4 | 213.4 | 1581 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 60.2 | 60.2 | 60.2 | 0.0 | 0.1 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 0.1 | 0.1 | 0.1 | 0.0 | 0.1 | update 1 |
| notify | 1 | 0 | 0 | 0 | 1 | 3.3 | 3.3 | 3.3 | 0.0 | 1.9 | update 1 |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 4.8 | 4.8 | 4.8 | 0.0 | 3.9 | update 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 0.8 | 0.8 | 0.8 | 0.0 | 1.2 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 0.3 | 0.3 | 0.3 | 0.0 | 0.3 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 1 | 0.4 | 0.4 | 0.4 | 0.0 | 55.7 | change 1 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 1.4 | 10010 | 0 |
| pathwatch-0 | 1.4 | 10008 | 0 |
| notify | 0.0 | 10008 | 0 |
| notify-fork | 0.0 | 10002 | 0 |
| chokidar | 0.4 | 10007 | 0 |
| parcel | 47.6 | 10006 | 0 |
| fs-watch | 294 | 10010 | 0 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 58.9 | 65.1 | 65.5 | 0.0 | 68.9 | update 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 45 | 0.3 | 0.5 | 1.7 | 0.0 | 42.6 | update 200 |
| notify | 200 | 0 | 0 | 0 | 302 | 0.3 | 0.4 | 0.7 | 0.0 | 26.7 | update 200 |
| notify-fork | 200 | 0 | 0 | 0 | 268 | 0.3 | 0.4 | 0.6 | 0.0 | 26.3 | update 200 |
| chokidar | 200 | 0 | 0 | 0 | 0 | 1.6 | 3.0 | 11.7 | 0.0 | 1529 | update 200 |
| parcel | 200 | 0 | 0 | 0 | 0 | 59.3 | 61.2 | 61.8 | 0.0 | 179 | update 200 |
| fs-watch | 200 | 0 | 0 | 0 | 200 | 0.9 | 1.2 | 2.1 | 0.0 | 202 | change 200 |

#### new files in new directories

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5000 | 0 | 0 | 0 | 0 | 127 | 128 | 128 | 40.9 | 26.7 | create 5000 |
| pathwatch-0 | 5000 | 0 | 0 | 0 | 11 | 30.6 | 38.2 | 38.5 | 37.1 | 34.7 | create 5000 |
| notify | 5000 | 3122 | 0 | 500 | 5757 | 28.7 | 43.0 | 43.6 | 43.6 | 42.4 | close-write 26, create 2795, update 167 |
| notify-fork | 5000 | 3407 | 0 | 500 | 3687 | 31.3 | 45.6 | 46.9 | 46.8 | 44.2 | close-write 16, create 1766, update 155 |
| chokidar | 5000 | 0 | 0 | 500 | 0 | 311 | 312 | 312 | 29.5 | 755 | create 5000 |
| parcel | 5000 | 2674 | 0 | 500 | 0 | 115 | 117 | 117 | 42.4 | 38.3 | create 2386, update 121 |
| fs-watch | 5000 | 0 | 0 | 500 | 0 | 240 | 241 | 242 | 37.8 | 300 | rename 5000 |

#### a directory tree removed

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5500 | 0 | 0 | 0 | 0 | 202 | 204 | 204 | 23.9 | 11.4 | delete 5500 |
| pathwatch-0 | 5500 | 0 | 0 | 0 | 0 | 133 | 136 | 136 | 22.9 | 11.6 | delete 5500 |
| notify | 5500 | 0 | 0 | 551 | 0 | 14.5 | 23.8 | 24.6 | 24.6 | 23.7 | delete 5500 |
| notify-fork | 5500 | 0 | 0 | 551 | 0 | 12.6 | 20.9 | 21.7 | 21.6 | 23.2 | delete 5500 |
| chokidar | 5500 | 0 | 0 | 551 | 0 | 284 | 285 | 285 | 27.9 | 598 | delete 5500 |
| parcel | 5500 | 0 | 0 | 551 | 0 | 438 | 439 | 439 | 23.7 | 438 | delete 5500 |
| fs-watch | 5500 | 5498 | 0 | 3 | 0 | 55.2 | 55.2 | 55.2 | 26.1 | 92.3 | rename 2 |

#### a directory tree moved in

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5000 | 0 | 0 | 0 | 0 | 84.3 | 86.5 | 86.6 | 0.1 | 23.7 | create 5000 |
| pathwatch-0 | 5000 | 0 | 0 | 0 | 0 | 19.2 | 20.7 | 20.9 | 0.1 | 17.4 | create 5000 |
| notify | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.1 | 15.2 |  |
| notify-fork | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.1 | 9.4 |  |
| chokidar | 5000 | 0 | 0 | 502 | 0 | 192 | 193 | 193 | 0.1 | 323 | create 5000 |
| parcel | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.1 | 16.3 |  |
| fs-watch | 5000 | 0 | 0 | 502 | 0 | 62.6 | 62.8 | 62.9 | 0.1 | 87.5 | rename 5000 |

#### a directory tree moved out

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5000 | 0 | 0 | 0 | 0 | 177 | 179 | 179 | 0.1 | 7.6 | delete 5000 |
| pathwatch-0 | 5000 | 0 | 0 | 0 | 0 | 120 | 122 | 122 | 0.1 | 8.6 | delete 5000 |
| notify | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.1 | 5.8 |  |
| notify-fork | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.1 | 5.6 |  |
| chokidar | 5000 | 0 | 0 | 502 | 0 | 106 | 107 | 107 | 0.1 | 317 | delete 5000 |
| parcel | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 0.1 | 13.3 |  |
| fs-watch | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 3.4 | 40.8 |  |

#### Notes

- notify, new files in new directories: missed for example `.../project/g02/new072/m8.js`
- notify, a directory tree moved in: missed for example `.../project/moved/g00/d110/m6.js`
- notify, a directory tree moved out: missed for example `.../project/moved/g00/d096/m6.js`
- notify-fork, new files in new directories: missed for example `.../project/g03/new313/m3.js`
- notify-fork, a directory tree moved in: missed for example `.../project/moved/g00/d055/m2.js`
- notify-fork, a directory tree moved out: missed for example `.../project/moved/g00/d484/m5.js`
- parcel, new files in new directories: missed for example `.../project/g02/new182/m5.js`
- parcel, a directory tree moved in: missed for example `.../project/moved/g00/d184/m9.js`
- parcel, a directory tree moved out: missed for example `.../project/moved/g00/d254/m2.js`
- fs-watch, a directory tree removed: missed for example `.../project/g01/new221/m4.js`
- fs-watch, a directory tree moved out: missed for example `.../project/moved/g00/d144/m9.js`

### S3: noise: module files while another process writes

Sizes: dirs 2000, files 20000, watched 20000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 63.1 | 60.3 | 9.1 | +6.8 | 7.9 | 9.1 | 9.2 | 69.7 | 0 |
| pathwatch-0 | 63.1 | 59.2 | 9.1 | +6.9 | 8.0 | 9.2 | 9.2 | 65.9 | 0 |
| notify | 436 | 295 | 9.9 | +7.9 | 8.9 | 9.9 | 9.9 | 316 | 0 |
| notify-fork | 450 | 287 | 7.7 | +5.7 | 6.7 | 7.7 | 7.7 | 293 | 0 |
| chokidar | 424 | 739 | 226.9 | +167.9 | 225.8 | 227.0 | 227.0 | 785 | 0 |
| parcel | 154 | 174 | 85.9 | +19.9 | 84.8 | 244.0 | 253.0 | 60682 | 0 |
| fs-watch | 243 | 325 | 132.9 | +74.4 | 131.8 | 441.9 | 441.9 | 27324 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 60.2 | 60.2 | 60.2 | 0.0 | 0.1 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 0.1 | 0.1 | 0.1 | 0.0 | 0.1 | update 1 |
| notify | 1 | 0 | 0 | 0 | 1 | 0.1 | 0.1 | 0.1 | 0.0 | 0.0 | update 1 |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 0.1 | 0.1 | 0.1 | 0.0 | 0.0 | update 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 0.6 | 0.6 | 0.6 | 0.0 | 0.8 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 0.4 | 0.4 | 0.4 | 0.0 | 0.4 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 1 | 0.4 | 0.4 | 0.4 | 0.0 | 0.4 | change 1 |

#### single edits under noise

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 60.1 | 64.9 | 65.1 | 0.0 | 5.6 | update 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 6 | 0.1 | 0.1 | 0.2 | 0.0 | 3.5 | update 100 |
| notify | 100 | 0 | 0 | 0 | 100 | 0.0 | 0.1 | 0.1 | 0.0 | 2.0 | update 100 |
| notify-fork | 100 | 0 | 0 | 0 | 104 | 0.0 | 0.1 | 0.1 | 0.0 | 2.3 | update 100 |
| chokidar | 100 | 0 | 0 | 0 | 0 | 0.1 | 0.3 | 0.4 | 0.0 | 17.1 | update 100 |
| parcel | 100 | 0 | 5000500 | 0 | 0 | 461 | 468 | 472 | 0.0 | 60239 | update 100 |
| fs-watch | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 15282 |  |

- pathwatch: noise files 50000, noise writes 4880082, noise writes per second 481526
- pathwatch-0: noise files 50000, noise writes 2007055, noise writes per second 479919
- notify: noise files 50000, noise writes 1856652, noise writes per second 485024
- notify-fork: noise files 50000, noise writes 1954818, noise writes per second 481576
- chokidar: noise files 50000, noise writes 1922438, noise writes per second 479833
- parcel: noise files 50000, noise writes 19833125, noise writes per second 396104
- fs-watch: noise files 50000, noise writes 6526164, noise writes per second 424261

#### quiet after the noise

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 0.1 | 1010 | 0 |
| pathwatch-0 | 0.1 | 1010 | 0 |
| notify | 0.0 | 1008 | 0 |
| notify-fork | 0.0 | 1007 | 0 |
| chokidar | 0.0 | 1006 | 0 |
| parcel | 0.2 | 1005 | 0 |
| fs-watch | 1014 | 1010 | 0 |

#### Notes

- parcel: @parcel/watcher watches directories only: it watches .../project for 20000 paths
- parcel, single edits under noise: extra for example `create .../project/dist/n0000`
- fs-watch: fs.watch is called once, for .../project, for 20000 paths
- fs-watch, single edits under noise: missed for example `.../project/g00/d000/m0.js`

### S4: ignored directory: writes into node_modules

Sizes: dirs 500, files 5000, files written into node_modules 30000, watched 1.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 22.8 | 22.7 | 2.9 | +0.7 | 1.8 | 2.9 | 2.9 | 32.5 | 0 |
| pathwatch-0 | 24.5 | 24.5 | 2.9 | +0.7 | 1.8 | 2.9 | 2.9 | 30.8 | 0 |
| notify | 21.2 | 20.9 | 2.4 | +0.4 | 1.3 | 4.0 | 4.0 | 195 | 0 |
| notify-fork | 15.4 | 15.3 | 2.5 | +0.4 | 1.3 | 2.5 | 2.5 | 20.6 | 0 |
| chokidar | 303 | 496 | 149.4 | +90.4 | 148.3 | 151.7 | 151.7 | 643 | 0 |
| parcel | 125 | 127 | 68.2 | +2.3 | 67.1 | 68.4 | 68.4 | 162 | 0 |
| fs-watch | 135 | 174 | 85.4 | +26.8 | 84.2 | 196.7 | 199.3 | 620 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 60.1 | 60.1 | 60.1 | 0.0 | 0.3 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 0.1 | 0.1 | 0.1 | 0.0 | 0.1 | update 1 |
| notify | 1 | 0 | 0 | 0 | 2 | 0.4 | 0.4 | 0.4 | 0.0 | 0.1 | update 1 |
| notify-fork | 1 | 0 | 0 | 0 | 2 | 0.5 | 0.5 | 0.5 | 0.0 | 1.2 | update 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 0.6 | 0.6 | 0.6 | 0.0 | 1.1 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 0.3 | 0.3 | 0.3 | 0.0 | 0.3 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 1 | 0.4 | 0.4 | 0.4 | 0.0 | 10.5 | change 1 |

#### node_modules written, then one edit in src

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 60.2 | 60.2 | 60.2 | 114 | 0.1 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 1 | 0.1 | 0.1 | 0.1 | 108 | 0.1 | update 1 |
| notify | 1 | 0 | 51519 | 0 | 1 | 0.0 | 0.0 | 0.0 | 131 | 166 | update 1 |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 0.1 | 0.1 | 0.1 | 110 | 0.1 | update 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 0.4 | 0.4 | 0.4 | 79.4 | 0.9 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 60.3 | 60.3 | 60.3 | 67.4 | 0.3 | update 1 |
| fs-watch | 1 | 0 | 33600 | 0 | 1 | 205 | 205 | 205 | 82.6 | 382 | change 1 |

#### single edits in src

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 20 | 0 | 0 | 0 | 0 | 60.1 | 63.3 | 65.3 | 0.0 | 8.0 | update 20 |
| pathwatch-0 | 20 | 0 | 0 | 0 | 6 | 0.3 | 0.5 | 0.6 | 0.0 | 4.7 | update 20 |
| notify | 20 | 0 | 0 | 0 | 32 | 0.3 | 0.4 | 0.4 | 0.0 | 2.8 | update 20 |
| notify-fork | 20 | 0 | 0 | 0 | 23 | 0.3 | 0.4 | 0.4 | 0.0 | 2.6 | update 20 |
| chokidar | 20 | 0 | 0 | 0 | 0 | 1.8 | 3.0 | 3.5 | 0.0 | 105 | update 20 |
| parcel | 20 | 0 | 0 | 0 | 0 | 59.5 | 61.6 | 62.0 | 0.0 | 23.3 | update 20 |
| fs-watch | 20 | 0 | 0 | 0 | 20 | 1.0 | 1.3 | 1.8 | 0.0 | 19.6 | change 20 |

#### Notes

- notify: notify has no ignore rule: every event of the library is printed
- notify, node_modules written, then one edit in src: extra for example `create .../project/node_modules/package000`
- fs-watch: fs.watch has no ignore rule: every event is printed
- fs-watch, node_modules written, then one edit in src: extra for example `rename .../project/node_modules/package000`

### S5: polling: module files, polled every 100 ms

Sizes: dirs 2000, files 20000, watched 20000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 84.8 | 82.2 | 9.8 | +7.6 | 8.7 | 9.8 | 9.8 | 3392 | 0 |
| pathwatch-0 | 85.7 | 82.0 | 9.8 | +7.5 | 8.7 | 9.8 | 9.8 | 3404 | 0 |
| notify | 83.1 | 79.9 | 17.5 | +15.4 | 16.3 | 17.5 | 17.5 | 6341 | 0 |
| notify-fork | 48.3 | 45.5 | 13.4 | +11.4 | 12.3 | 16.2 | 16.6 | 9649 | 0 |
| chokidar | 507 | 920 | 232.0 | +172.9 | 230.8 | 227.8 | 233.7 | 12186 | 0 |
| parcel | unsupported: @parcel/watcher has no polling mode |
| fs-watch | 184 | 304 | 117.5 | +58.9 | 116.3 | 105.1 | 119.5 | 11935 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 75.3 | 75.3 | 75.3 | 0.0 | 36.4 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 73.8 | 73.8 | 73.8 | 0.0 | 35.7 | update 1 |
| notify | 1 | 0 | 0 | 0 | 0 | 55.1 | 55.1 | 55.1 | 0.0 | 28.8 | metadata 1 |
| notify-fork | 1 | 1 | 0 | 0 | 0 | - | - | - | 0.0 | 3432 |  |
| chokidar | 1 | 0 | 0 | 0 | 0 | 97.8 | 97.8 | 97.8 | 0.0 | 43.7 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 0 | 31.7 | 31.7 | 31.7 | 0.0 | 34.4 | change 1 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 1011 | 10010 | 0 |
| pathwatch-0 | 1010 | 10009 | 0 |
| notify | 3445 | 10010 | 0 |
| notify-fork | 3412 | 10009 | 0 |
| chokidar | 7331 | 10010 | 0 |
| fs-watch | 7496 | 10005 | 0 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 50 | 0 | 0 | 0 | 0 | 416 | 440 | 472 | 0.0 | 2261 | update 50 |
| pathwatch-0 | 50 | 0 | 0 | 0 | 0 | 415 | 436 | 449 | 0.0 | 2265 | update 50 |
| notify | 50 | 0 | 0 | 0 | 0 | 122 | 159 | 162 | 0.0 | 2709 | metadata 50 |
| notify-fork | 50 | 0 | 0 | 0 | 0 | 126 | 146 | 151 | 0.0 | 2650 | metadata 50 |
| chokidar | 50 | 0 | 0 | 0 | 0 | 62.8 | 78.9 | 90.5 | 0.0 | 3724 | update 50 |
| fs-watch | 50 | 0 | 0 | 0 | 0 | 70.7 | 79.9 | 96.2 | 0.0 | 4002 | change 50 |

#### Notes

- notify-fork, first edit, right after ready: missed for example `.../project/g00/d000/m0.js`
- fs-watch: polling: fs.watchFile for each path

### S6: missing paths: watched before they exist

Sizes: dirs 0, files 0, watched 1000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 9.9 | 10.0 | 2.6 | +0.3 | 1.4 | 2.7 | 2.7 | 24.0 | 0 |
| pathwatch-0 | 4.2 | 4.6 | 2.6 | +0.3 | 1.4 | 2.7 | 2.7 | 18.2 | 0 |
| notify | 16.1 | 15.3 | 2.1 | +0.1 | 1.1 | 2.1 | 2.1 | 16.1 | 4 |
| notify-fork | 31.5 | 23.6 | 2.2 | +0.2 | 1.2 | 2.2 | 2.2 | 24.2 | 4 |
| chokidar | 115 | 123 | 72.2 | +13.1 | 71.0 | 75.2 | 101.4 | 405 | 0 |
| parcel | 105 | 107 | 67.5 | +1.5 | 66.3 | 65.6 | 68.3 | 204 | 0 |
| fs-watch | 97.4 | 97.8 | 59.2 | +0.6 | 58.1 | 73.4 | 73.4 | 202 | 0 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 0.4 | 3010 | 0 |
| pathwatch-0 | 0.4 | 3004 | 0 |
| notify | 0.0 | 3008 | 0 |
| notify-fork | 0.0 | 3002 | 0 |
| chokidar | 0.0 | 3010 | 0 |
| parcel | 0.5 | 3009 | 0 |
| fs-watch | 0.0 | 3010 | 0 |

#### the missing paths are made

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1000 | 0 | 0 | 0 | 0 | 77.2 | 77.7 | 77.7 | 10.9 | 6.1 | create 1000 |
| pathwatch-0 | 1000 | 0 | 0 | 0 | 9 | 9.2 | 12.5 | 12.5 | 10.8 | 7.8 | create 1000 |
| notify | 1000 | 1000 | 0 | 0 | 0 | - | - | - | 12.1 | 0.0 |  |
| notify-fork | 1000 | 1000 | 0 | 0 | 0 | - | - | - | 11.0 | 0.0 |  |
| chokidar | 1000 | 1000 | 0 | 0 | 0 | - | - | - | 12.1 | 61.4 |  |
| parcel | 1000 | 441 | 0 | 99 | 1 | 73.9 | 74.1 | 74.1 | 12.2 | 18.9 | create 534, update 25 |
| fs-watch | 1000 | 0 | 0 | 110 | 0 | 44.9 | 45.0 | 45.0 | 15.7 | 69.0 | rename 1000 |

#### single edits after

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 20 | 0 | 0 | 0 | 0 | 58.5 | 62.1 | 62.7 | 0.0 | 6.9 | update 20 |
| pathwatch-0 | 20 | 0 | 0 | 0 | 6 | 0.3 | 0.4 | 0.5 | 0.0 | 4.3 | update 20 |
| notify | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| notify-fork | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| chokidar | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 67.2 |  |
| parcel | 20 | 11 | 0 | 0 | 1 | 1.2 | 1.7 | 1.7 | 0.0 | 63.2 | update 9 |
| fs-watch | 20 | 0 | 0 | 0 | 20 | 1.1 | 1.6 | 1.9 | 0.0 | 21.4 | change 20 |

#### Notes

- notify: 4 errors, the first: `watch .../project/g00/d000/m0.js: No path was found. about [".../project/g00/d000/m0.js"]`
- notify, the missing paths are made: missed for example `.../project/g00/d006/m6.js`
- notify, single edits after: missed for example `.../project/g00/d000/m0.js`
- notify-fork: 4 errors, the first: `watch .../project/g00/d000/m0.js: No path was found. about [".../project/g00/d000"]`
- notify-fork, the missing paths are made: missed for example `.../project/g04/d006/m6.js`
- notify-fork, single edits after: missed for example `.../project/g00/d000/m0.js`
- chokidar, the missing paths are made: missed for example `.../project/g06/d002/m0.js`
- chokidar, single edits after: missed for example `.../project/g00/d000/m0.js`
- parcel: @parcel/watcher watches directories only: it watches .../project for 1000 paths
- parcel, the missing paths are made: missed for example `.../project/g01/d006/m1.js`
- parcel, single edits after: missed for example `.../project/g00/d000/m0.js`
- fs-watch: fs.watch is called once, for .../project, for 1000 paths

### S7: big directory: one watched directory holds every file

Sizes: dirs 1, files 20000, watched 1.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 48.8 | 48.5 | 5.2 | +2.9 | 4.1 | 5.4 | 5.4 | 187 | 0 |
| pathwatch-0 | 48.3 | 48.2 | 5.1 | +2.9 | 4.0 | 5.4 | 5.4 | 144 | 0 |
| notify | 46.6 | 46.2 | 2.1 | +0.1 | 1.0 | 2.2 | 2.2 | 103 | 0 |
| notify-fork | 25.1 | 24.9 | 2.2 | +0.2 | 1.1 | 2.3 | 2.3 | 86.6 | 0 |
| chokidar | 556 | 917 | 163.7 | +104.6 | 162.5 | 229.3 | 259.5 | 62029 | 0 |
| parcel | 116 | 117 | 71.1 | +5.1 | 69.9 | 73.2 | 73.2 | 477 | 0 |
| fs-watch | 200 | 267 | 128.7 | +70.1 | 127.5 | 252.7 | 262.2 | 37457 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 60.4 | 60.4 | 60.4 | 0.0 | 0.2 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 0.1 | 0.1 | 0.1 | 0.0 | 0.1 | update 1 |
| notify | 1 | 0 | 0 | 0 | 2 | 0.1 | 0.1 | 0.1 | 0.0 | 0.0 | update 1 |
| notify-fork | 1 | 0 | 0 | 0 | 2 | 0.1 | 0.1 | 0.1 | 0.0 | 0.0 | update 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 0.7 | 0.7 | 0.7 | 0.0 | 54.1 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 0.3 | 0.3 | 0.3 | 0.0 | 0.3 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 1 | 25.6 | 25.6 | 25.6 | 0.0 | 32.4 | change 1 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 59.6 | 64.3 | 65.4 | 0.0 | 35.9 | update 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 29 | 0.3 | 0.5 | 0.6 | 0.0 | 20.4 | update 100 |
| notify | 100 | 0 | 0 | 0 | 158 | 0.3 | 0.4 | 0.7 | 0.0 | 12.7 | update 100 |
| notify-fork | 100 | 0 | 0 | 0 | 144 | 0.3 | 0.4 | 0.6 | 0.0 | 12.9 | update 100 |
| chokidar | 100 | 1 | 0 | 0 | 0 | 4.1 | 5.4 | 11.8 | 0.0 | 5999 | update 99 |
| parcel | 100 | 0 | 0 | 0 | 0 | 59.1 | 61.2 | 61.5 | 0.0 | 145 | update 100 |
| fs-watch | 100 | 0 | 0 | 0 | 100 | 22.9 | 24.6 | 28.9 | 0.0 | 2632 | change 100 |

#### single new files

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 59.6 | 63.4 | 65.4 | 0.0 | 35.5 | create 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 44 | 0.3 | 0.4 | 0.5 | 0.0 | 21.7 | create 100 |
| notify | 100 | 0 | 0 | 0 | 200 | 0.3 | 0.4 | 0.8 | 0.0 | 11.5 | create 100 |
| notify-fork | 100 | 0 | 0 | 0 | 200 | 0.3 | 0.4 | 0.5 | 0.0 | 14.6 | create 100 |
| chokidar | 100 | 0 | 0 | 0 | 0 | 145 | 240 | 246 | 0.0 | 32106 | create 100 |
| parcel | 100 | 0 | 0 | 0 | 0 | 59.5 | 61.2 | 61.5 | 0.0 | 87.2 | create 100 |
| fs-watch | 100 | 0 | 0 | 0 | 0 | 22.6 | 24.5 | 29.8 | 0.0 | 2601 | rename 100 |

#### burst of new files

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 2000 | 0 | 0 | 0 | 0 | 90.3 | 90.7 | 90.8 | 22.0 | 10.8 | create 2000 |
| pathwatch-0 | 2000 | 0 | 0 | 0 | 11 | 19.7 | 28.7 | 28.8 | 23.4 | 12.1 | create 2000 |
| notify | 2000 | 0 | 0 | 0 | 4000 | 14.6 | 23.4 | 24.3 | 24.3 | 20.9 | create 2000 |
| notify-fork | 2000 | 0 | 0 | 0 | 4000 | 15.1 | 23.7 | 25.0 | 24.4 | 21.5 | create 2000 |
| chokidar | 2000 | 0 | 0 | 0 | 0 | 139 | 160 | 160 | 5.6 | 462 | create 2000 |
| parcel | 2000 | 0 | 0 | 0 | 0 | 96.2 | 97.7 | 97.7 | 25.5 | 30.6 | create 2000 |
| fs-watch | 2000 | 2000 | 0 | 0 | 0 | - | - | - | 8.2 | 3672 |  |

#### single files removed

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 165 | 171 | 171 | 0.0 | 53.7 | delete 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 0 | 109 | 111 | 111 | 0.0 | 40.1 | delete 100 |
| notify | 100 | 0 | 0 | 0 | 0 | 0.2 | 0.4 | 0.5 | 0.0 | 11.1 | delete 100 |
| notify-fork | 100 | 0 | 0 | 0 | 0 | 0.2 | 0.3 | 0.4 | 0.0 | 10.9 | delete 100 |
| chokidar | 100 | 0 | 0 | 0 | 0 | 106 | 108 | 110 | 0.0 | 22534 | delete 100 |
| parcel | 100 | 0 | 0 | 0 | 0 | 59.5 | 61.1 | 61.3 | 0.0 | 88.0 | delete 100 |
| fs-watch | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 17032 |  |

#### Notes

- chokidar, single edits: missed for example `.../project/m00000.js`
- fs-watch, burst of new files: missed for example `.../project/burst01875.js`
- fs-watch, single files removed: missed for example `.../project/m00001.js`

## Windows

- Started: 2026-10-10T16:17:51Z
- Machine: 13th Gen Intel(R) Core(TM) i5-13600KF, 20 cores, 32 GB, Windows 11 Pro (Windows NT 10.0.26200.9457 x86_64), temporary directory on NTFS
- Busy cores at the start (half a second): 0.7
- Sizes: the full sizes; a quiet phase is 10 s
- Times are in milliseconds, memory is in MiB.

### Versions

| name | version |
| --- | ---: |
| chokidar | 5.0.0 |
| fs-watch | part of node |
| node | v24.12.0 |
| notify | 8.2.0 |
| notify-fork | 11.0.0 (git 3a1cde09) |
| parcel | 2.6.0 |
| pathwatch | 0.0.0 (working tree, 53dd5b45) |
| python | Python 3.13.13 |
| rustc | rustc 1.99.0 (b940084d7 2026-09-28) |
| watchdog | 6.0.0 |

### Bases: each adapter when it watches nothing

| adapter | start to ready | CPU | RSS | footprint |
| --- | ---: | ---: | ---: | ---: |
| chokidar | 32.9 | 31.2 | 37.4 | 23.3 |
| fs-watch | 26.7 | 15.6 | 35.6 | 21.8 |
| node-idle | 30.6 | 31.2 | 35.1 | 21.7 |
| notify | 2.8 | 0.0 | 3.9 | 0.8 |
| notify-fork | 2.5 | 0.0 | 3.9 | 0.8 |
| parcel | 35.4 | 31.2 | 37.9 | 23.4 |
| pathwatch | 2.5 | 15.6 | 3.9 | 0.8 |
| pathwatch-0 | 2.5 | 0.0 | 3.9 | 0.8 |
| watchdog | 51.3 | 93.8 | 19.2 | 12.7 |

### Where pathwatch is behind

Every number in which pathwatch is more than 10% behind the best other library, and by more than 1 ms or 0.5 MiB. Only a library that did the job counts: it missed no change of the phase, and for the setup it stopped early in no phase of the run. `pathwatch` has the default settings, and `pathwatch-0` is the same without the 50 ms that pathwatch waits on purpose. `times` is `pathwatch-0` next to the best other, and `pathwatch` where there is no `pathwatch-0`. With more runs than one, a number is `middle (lowest to highest)`.

#### Behind with and without the wait

| scenario | where | number | pathwatch | pathwatch-0 | best other | which | times |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| S1 | setup | memory above base | 9.5 | 9.7 | 2.5 | watchdog | 3.8 |
| S1 | quiet | CPU | 15.6 | 15.6 | 0.0 | notify-fork | - |
| S1 | single edits | max | 73.2 | 12.1 | 1.9 | watchdog | 6.4 |
| S1 | single edits | CPU | 78.1 | 78.1 | 46.9 | notify-fork | 1.7 |
| S1 | single atomic saves | max | 71.6 | 3.4 | 1.8 | notify-fork | 1.8 |
| S1 | single atomic saves | CPU | 46.9 | 46.9 | 0.0 | notify-fork | - |
| S1 | single edits of the saved files | CPU | 31.2 | 15.6 | 0.0 | notify-fork | - |
| S1 | burst of edits | p50 | 520 | 443 | 328 | notify | 1.4 |
| S1 | burst of edits | max | 955 | 936 | 759 | notify-fork | 1.2 |
| S1 | files removed | p50 | 204 | 137 | 7.7 | fs-watch | 17.7 |
| S1 | files removed | max | 204 | 152 | 12.9 | fs-watch | 11.8 |
| S1 | files written again | p50 | 144 | 59.8 | 45.7 | notify-fork | 1.3 |
| S1 | files written again | max | 144 | 118 | 107 | notify-fork | 1.1 |
| S1 | files written again | CPU | 46.9 | 78.1 | 15.6 | chokidar | 5.0 |
| S2 | setup | time | 169 | 168 | 4.5 | notify | 37.7 |
| S2 | setup | CPU | 203 | 203 | 0.0 | notify | - |
| S2 | setup | memory above base | 5.7 | 6.2 | 0.0 | fs-watch | - |
| S2 | single edits | max | 96.0 | 41.5 | 13.5 | watchdog | 3.1 |
| S2 | single edits | CPU | 78.1 | 78.1 | 46.9 | notify-fork | 1.7 |
| S2 | new files in new directories | p50 | 1755 | 1632 | 1441 | notify-fork | 1.1 |
| S2 | a directory tree removed | p50 | 356 | 260 | 149 | watchdog | 1.7 |
| S2 | a directory tree removed | max | 533 | 440 | 300 | watchdog | 1.5 |
| S2 | a directory tree removed | CPU | 78.1 | 141 | 46.9 | parcel | 3.0 |
| S3 | setup | memory above base | 9.5 | 9.3 | 2.3 | watchdog | 3.9 |
| S3 | single edits under noise | max | 65.4 | 4.5 | 3.2 | notify-fork | 1.4 |
| S4 | setup | time | 30.5 | 27.1 | 5.7 | notify | 4.8 |
| S4 | setup | CPU | 31.2 | 15.6 | 0.0 | notify | - |
| S4 | setup | memory above base | 2.0 | 1.7 | 0.0 | fs-watch | - |
| S4 | node_modules written, then one edit in src | CPU | 2172 | 4297 | 1547 | notify-fork | 2.8 |
| S4 | single edits in src | max | 64.1 | 4.1 | 1.0 | watchdog | 4.0 |
| S5 | setup | time | 133 | 129 | 34.9 | notify-fork | 3.7 |
| S5 | setup | CPU | 125 | 125 | 31.2 | notify-fork | 4.0 |
| S5 | first edit, right after ready | p50 | 96.2 | 99.3 | 33.5 | fs-watch | 3.0 |
| S5 | first edit, right after ready | max | 96.2 | 99.3 | 33.5 | fs-watch | 3.0 |
| S5 | first edit, right after ready | CPU | 109 | 93.8 | 78.1 | notify | 1.2 |
| S5 | single edits | p50 | 884 | 887 | 72.6 | fs-watch | 12.2 |
| S5 | single edits | max | 942 | 942 | 84.5 | chokidar | 11.2 |
| S7 | setup | time | 12.9 | 18.7 | 3.9 | notify-fork | 4.8 |
| S7 | setup | CPU | 15.6 | 15.6 | 0.0 | notify-fork | - |
| S7 | setup | memory above base | 2.6 | 1.9 | 0.0 | fs-watch | - |
| S7 | single edits | max | 69.4 | 12.8 | 1.6 | watchdog | 8.0 |
| S7 | single edits | CPU | 78.1 | 46.9 | 0.0 | notify-fork | - |
| S7 | single new files | CPU | 62.5 | 46.9 | 15.6 | notify | 3.0 |
| S7 | burst of new files | p50 | 791 | 667 | 478 | notify | 1.4 |
| S7 | burst of new files | max | 1198 | 1311 | 925 | notify-fork | 1.4 |
| S7 | burst of new files | CPU | 141 | 531 | 109 | notify | 4.9 |
| S7 | single files removed | p50 | 175 | 112 | 0.4 | notify-fork | 266.6 |
| S7 | single files removed | max | 178 | 116 | 2.8 | notify | 41.2 |
| S7 | single files removed | CPU | 31.2 | 15.6 | 0.0 | fs-watch | - |

#### Behind only with the wait: the price of the 50 ms

| scenario | where | number | pathwatch | pathwatch-0 | best other | which | times |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| S1 | first edit, right after ready | p50 | 53.9 | 0.3 | 0.3 | notify | 0.8 |
| S1 | first edit, right after ready | max | 53.9 | 0.3 | 0.3 | notify | 0.8 |
| S1 | single edits | p50 | 62.4 | 0.9 | 0.6 | notify-fork | 1.4 |
| S1 | single atomic saves | p50 | 63.9 | 1.8 | 1.3 | notify-fork | 1.3 |
| S1 | single edits of the saved files | p50 | 63.9 | 0.9 | 0.6 | notify-fork | 1.4 |
| S1 | single edits of the saved files | max | 64.9 | 1.4 | 1.0 | notify-fork | 1.4 |
| S2 | first edit, right after ready | p50 | 57.6 | 0.9 | 0.1 | notify | 5.9 |
| S2 | first edit, right after ready | max | 57.6 | 0.9 | 0.1 | notify | 5.9 |
| S2 | single edits | p50 | 63.9 | 0.8 | 0.6 | notify | 1.3 |
| S2 | a directory tree moved in | p50 | 77.9 | 23.0 | 27.1 | watchdog | 0.8 |
| S2 | a directory tree moved in | max | 78.3 | 23.4 | 44.3 | watchdog | 0.5 |
| S3 | first edit, right after ready | p50 | 64.5 | 0.3 | 0.2 | notify-fork | 1.4 |
| S3 | first edit, right after ready | max | 64.5 | 0.3 | 0.2 | notify-fork | 1.4 |
| S3 | single edits under noise | p50 | 63.3 | 0.7 | 0.4 | notify-fork | 1.6 |
| S4 | first edit, right after ready | p50 | 53.1 | 0.2 | 0.3 | watchdog | 0.7 |
| S4 | first edit, right after ready | max | 53.1 | 0.2 | 0.3 | watchdog | 0.7 |
| S4 | node_modules written, then one edit in src | p50 | 51.8 | 0.3 | 0.4 | notify-fork | 0.7 |
| S4 | node_modules written, then one edit in src | max | 51.8 | 0.3 | 0.4 | notify-fork | 0.7 |
| S4 | single edits in src | p50 | 63.4 | 0.5 | 0.4 | notify | 1.2 |
| S6 | the missing paths are made | p50 | 518 | 336 | 307 | watchdog | 1.1 |
| S6 | single edits after | p50 | 63.8 | 0.5 | 0.5 | watchdog | 1.0 |
| S6 | single edits after | max | 66.3 | 1.3 | 1.0 | watchdog | 1.4 |
| S6 | single edits after | CPU | 15.6 | 0.0 | 0.0 | watchdog | - |
| S7 | first edit, right after ready | p50 | 66.1 | 0.3 | 0.2 | notify-fork | 1.6 |
| S7 | first edit, right after ready | max | 66.1 | 0.3 | 0.2 | notify-fork | 1.6 |
| S7 | single edits | p50 | 63.9 | 0.7 | 0.5 | notify | 1.3 |
| S7 | single new files | p50 | 64.1 | 1.0 | 0.6 | notify | 1.7 |
| S7 | single new files | max | 67.3 | 1.6 | 0.9 | notify | 1.7 |

#### Behind only without the wait

| scenario | where | number | pathwatch | pathwatch-0 | best other | which | times |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| S1 | burst of edits | CPU | 172 | 359 | 172 | notify-fork | 2.1 |
| S1 | files removed | CPU | 0.0 | 31.2 | 0.0 | fs-watch | - |
| S2 | new files in new directories | max | 2876 | 3291 | 2879 | notify-fork | 1.1 |
| S2 | new files in new directories | CPU | 562 | 1484 | 516 | notify-fork | 2.9 |
| S3 | setup | CPU | 141 | 156 | 141 | parcel | 1.1 |
| S3 | single edits under noise | CPU | 15.6 | 46.9 | 15.6 | notify | 3.0 |
| S6 | the missing paths are made | CPU | 62.5 | 281 | 93.8 | parcel | 3.0 |

Of 104 numbers that can be compared, `pathwatch` is the best or near it (inside the limits above) in 27, and `pathwatch-0` in 48.

pathwatch missed no change, gave no event too many, and no error.

### S1: module files: every file is watched by its own path

Sizes: dirs 2000, files 20000, watched 20000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 156 | 156 | 13.2 | +9.3 | 10.3 | 13.6 | 14.0 | 547 | 0 |
| pathwatch-0 | 145 | 156 | 13.4 | +9.5 | 10.5 | 13.6 | 14.2 | 781 | 0 |
| notify | 1633 | 1672 | 330.7 | +326.8 | 328.8 | 330.7 | 330.7 | 2312 | 0 |
| notify-fork | 246 | 234 | 17.3 | +13.4 | 14.8 | 17.4 | 18.1 | 484 | 0 |
| chokidar | 2145 | 2500 | 380.5 | +343.1 | 401.8 | 213.3 | 380.8 | 3656 | 0 |
| parcel | 165 | 188 | 55.2 | +17.3 | 69.8 | 49.7 | 56.6 | 859 | 0 |
| fs-watch | 147 | 203 | 51.5 | +15.9 | 33.5 | 46.3 | 53.3 | 1891 | 0 |
| watchdog | 161 | 156 | 22.5 | +3.3 | 15.2 | 22.5 | 22.5 | 1250 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 53.9 | 53.9 | 53.9 | 0.0 | 0.0 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 1 | 0.3 | 0.3 | 0.3 | 0.0 | 0.0 | update 1 |
| notify | 1 | 0 | 0 | 0 | 1 | 0.3 | 0.3 | 0.3 | 0.0 | 0.0 | update 1 |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 5.1 | 5.1 | 5.1 | 0.0 | 0.0 | update 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 2.4 | 2.4 | 2.4 | 0.0 | 78.1 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 0.4 | 0.4 | 0.4 | 0.0 | 0.0 | update 1 |
| fs-watch | 1 | 0 | 0 | 2 | 1 | 1.5 | 1.5 | 1.5 | 0.0 | 0.0 | change 1 |
| watchdog | 1 | 0 | 0 | 1 | 0 | 0.4 | 0.4 | 0.4 | 0.0 | 0.0 | update 1 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 15.6 | 10002 | 0 |
| pathwatch-0 | 15.6 | 10009 | 0 |
| notify | 15.6 | 10004 | 0 |
| notify-fork | 0.0 | 10000 | 0 |
| chokidar | 188 | 10014 | 0 |
| parcel | 15.6 | 10011 | 0 |
| fs-watch | 78.1 | 10002 | 0 |
| watchdog | 0.0 | 10006 | 0 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 62.4 | 70.7 | 73.2 | 0.0 | 78.1 | update 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 180 | 0.9 | 1.3 | 12.1 | 0.0 | 78.1 | update 200 |
| notify | 200 | 0 | 0 | 0 | 200 | 0.7 | 0.9 | 35.3 | 0.0 | 62.5 | update 200 |
| notify-fork | 200 | 0 | 0 | 0 | 200 | 0.6 | 0.9 | 2.3 | 0.0 | 46.9 | update 200 |
| chokidar | 200 | 0 | 0 | 0 | 0 | 1.0 | 1.5 | 3.2 | 0.0 | 172 | update 200 |
| parcel | 200 | 0 | 0 | 0 | 1 | 64.1 | 64.7 | 67.0 | 0.0 | 203 | update 200 |
| fs-watch | 200 | 0 | 0 | 238 | 206 | 3.6 | 4.5 | 7.5 | 0.0 | 594 | change 200 |
| watchdog | 200 | 0 | 0 | 199 | 151 | 1.1 | 1.5 | 1.9 | 0.0 | 250 | update 200 |

#### single atomic saves

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 50 | 0 | 0 | 0 | 0 | 63.9 | 64.9 | 71.6 | 0.0 | 46.9 | update 50 |
| pathwatch-0 | 50 | 0 | 0 | 0 | 5 | 1.8 | 2.5 | 3.4 | 0.0 | 46.9 | update 50 |
| notify | 50 | 0 | 0 | 0 | 50 | 1.6 | 2.3 | 3.0 | 0.0 | 31.2 | delete 50 |
| notify-fork | 50 | 0 | 0 | 0 | 50 | 1.3 | 1.8 | 1.8 | 0.0 | 0.0 | delete 50 |
| chokidar | 50 | 0 | 0 | 0 | 0 | 1.8 | 2.3 | 2.5 | 0.0 | 46.9 | update 50 |
| parcel | 50 | 0 | 0 | 0 | 0 | 64.1 | 64.7 | 65.0 | 0.0 | 31.2 | update 50 |
| fs-watch | 50 | 0 | 176 | 100 | 50 | 4.5 | 5.5 | 6.4 | 0.0 | 219 | rename 50 |
| watchdog | 50 | 0 | 150 | 100 | 50 | 2.1 | 2.7 | 5.8 | 0.0 | 93.8 | delete 50 |

#### single edits of the saved files

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 50 | 0 | 0 | 0 | 0 | 63.9 | 64.5 | 64.9 | 0.0 | 31.2 | update 50 |
| pathwatch-0 | 50 | 0 | 0 | 0 | 46 | 0.9 | 1.1 | 1.4 | 0.0 | 15.6 | update 50 |
| notify | 50 | 0 | 0 | 0 | 50 | 0.7 | 0.9 | 1.0 | 0.0 | 15.6 | update 50 |
| notify-fork | 50 | 0 | 0 | 0 | 50 | 0.6 | 0.8 | 1.0 | 0.0 | 0.0 | update 50 |
| chokidar | 50 | 0 | 0 | 0 | 0 | 0.9 | 1.3 | 1.5 | 0.0 | 15.6 | update 50 |
| parcel | 50 | 0 | 0 | 0 | 0 | 64.1 | 64.7 | 65.3 | 0.0 | 31.2 | update 50 |
| fs-watch | 50 | 0 | 0 | 0 | 66 | 2.2 | 2.7 | 7.4 | 0.0 | 156 | change 50 |
| watchdog | 50 | 0 | 0 | 0 | 44 | 1.0 | 1.3 | 1.4 | 0.0 | 46.9 | update 50 |

#### burst of edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 2000 | 0 | 0 | 0 | 1 | 520 | 871 | 955 | 892 | 172 | update 2000 |
| pathwatch-0 | 2000 | 0 | 0 | 0 | 1539 | 443 | 896 | 936 | 936 | 359 | update 2000 |
| notify | 2000 | 0 | 0 | 0 | 2000 | 328 | 713 | 769 | 769 | 344 | update 2000 |
| notify-fork | 2000 | 0 | 0 | 0 | 2000 | 383 | 726 | 759 | 760 | 172 | update 2000 |
| chokidar | 2000 | 0 | 0 | 0 | 0 | 448 | 960 | 1009 | 1009 | 516 | update 2000 |
| parcel | 2000 | 0 | 0 | 0 | 1 | 958 | 958 | 1020 | 959 | 328 | update 2000 |
| fs-watch | 2000 | 1736 | 0 | 242 | 260 | 471 | 900 | 900 | 830 | 547 | change 264 |
| watchdog | 2000 | 0 | 0 | 1800 | 1517 | 458 | 787 | 822 | 822 | 641 | update 2000 |

#### files removed

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 204 | 204 | 204 | 40.3 | 0.0 | delete 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 0 | 137 | 152 | 152 | 39.9 | 31.2 | delete 200 |
| notify | 200 | 0 | 0 | 0 | 0 | 34.0 | 58.9 | 60.4 | 60.4 | 15.6 | delete 200 |
| notify-fork | 200 | 0 | 0 | 0 | 0 | 20.3 | 35.6 | 38.0 | 38.0 | 0.0 | delete 200 |
| chokidar | 200 | 0 | 0 | 0 | 0 | 104 | 104 | 104 | 48.5 | 31.2 | delete 200 |
| parcel | 200 | 0 | 0 | 0 | 0 | 104 | 104 | 104 | 40.9 | 0.0 | delete 200 |
| fs-watch | 200 | 0 | 0 | 0 | 0 | 7.7 | 12.5 | 12.9 | 12.9 | 0.0 | rename 200 |
| watchdog | 200 | 0 | 0 | 0 | 0 | 23.4 | 32.4 | 32.8 | 32.8 | 31.2 | delete 200 |

#### files written again

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 144 | 144 | 144 | 84.8 | 46.9 | create 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 188 | 59.8 | 114 | 118 | 118 | 78.1 | create 200 |
| notify | 200 | 0 | 0 | 0 | 200 | 56.6 | 107 | 110 | 110 | 46.9 | create 200 |
| notify-fork | 200 | 0 | 0 | 0 | 200 | 45.7 | 104 | 107 | 107 | 31.2 | create 200 |
| chokidar | 200 | 0 | 0 | 0 | 0 | 57.2 | 112 | 120 | 119 | 15.6 | update 200 |
| parcel | 200 | 0 | 0 | 0 | 0 | 174 | 174 | 174 | 119 | 62.5 | create 200 |
| fs-watch | 200 | 125 | 0 | 126 | 75 | 125 | 150 | 150 | 118 | 93.8 | rename 75 |
| watchdog | 200 | 0 | 0 | 240 | 200 | 62.6 | 115 | 120 | 120 | 15.6 | create 200 |

#### Notes

- parcel: @parcel/watcher watches directories only: it watches ...\project for 20000 paths
- fs-watch: fs.watch is called once, for ...\project, for 20000 paths
- fs-watch, single atomic saves: extra for example `rename ...\project\g00\d000\m3.js.tmp`
- fs-watch, burst of edits: missed for example `...\project\g05\d010\m0.js`
- fs-watch, files written again: missed for example `...\project\g10\d040\m1.js`
- watchdog: watchdog watches directories: it watches ...\project for 20000 paths
- watchdog, single atomic saves: extra for example `create ...\project\g00\d000\m3.js.tmp`

### S2: project directory: one directory is watched

Sizes: dirs 5000, files 50000, watched 1.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 169 | 203 | 9.7 | +5.8 | 6.5 | 9.9 | 13.2 | 953 | 0 |
| pathwatch-0 | 168 | 203 | 9.9 | +5.9 | 7.0 | 10.2 | 13.1 | 1922 | 0 |
| notify | 4.5 | 0.0 | 4.1 | +0.2 | 0.8 | 4.1 | 4.1 | 812 | 0 |
| notify-fork | 9.2 | 0.0 | 4.2 | +0.2 | 0.8 | 4.2 | 4.2 | 719 | 0 |
| chokidar | 7493 | 10203 | 793.6 | +756.2 | 824.4 | 791.2 | 796.1 | 18656 | 0 |
| parcel | 48.2 | 31.2 | 40.3 | +2.4 | 57.3 | 43.3 | 47.6 | 1250 | 0 |
| fs-watch | 30.9 | 31.2 | 35.4 | -0.1 | 21.5 | 41.8 | 42.3 | 2000 | 0 |
| watchdog | 53.7 | 46.9 | 19.1 | -0.1 | 12.5 | 19.5 | 19.5 | 2547 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 57.6 | 57.6 | 57.6 | 0.0 | 0.0 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 0.9 | 0.9 | 0.9 | 0.0 | 0.0 | update 1 |
| notify | 1 | 0 | 0 | 1 | 1 | 0.1 | 0.1 | 0.1 | 0.0 | 0.0 | update 1 |
| notify-fork | 1 | 0 | 0 | 1 | 1 | 0.2 | 0.2 | 0.2 | 0.0 | 0.0 | update 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 3.1 | 3.1 | 3.1 | 0.0 | 31.2 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 0.3 | 0.3 | 0.3 | 0.0 | 0.0 | update 1 |
| fs-watch | 1 | 0 | 0 | 2 | 1 | 1.5 | 1.5 | 1.5 | 0.0 | 0.0 | change 1 |
| watchdog | 1 | 0 | 0 | 1 | 1 | 0.3 | 0.3 | 0.3 | 0.0 | 0.0 | update 1 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 0.0 | 10010 | 0 |
| pathwatch-0 | 0.0 | 10003 | 0 |
| notify | 0.0 | 10002 | 0 |
| notify-fork | 0.0 | 10016 | 0 |
| chokidar | 266 | 10010 | 0 |
| parcel | 0.0 | 10005 | 0 |
| fs-watch | 0.0 | 10015 | 0 |
| watchdog | 0.0 | 10010 | 0 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 200 | 0 | 0 | 0 | 0 | 63.9 | 64.6 | 96.0 | 0.0 | 78.1 | update 200 |
| pathwatch-0 | 200 | 0 | 0 | 0 | 169 | 0.8 | 1.2 | 41.5 | 0.0 | 78.1 | update 200 |
| notify | 200 | 0 | 0 | 199 | 200 | 0.6 | 0.9 | 26.1 | 0.0 | 78.1 | update 200 |
| notify-fork | 200 | 0 | 0 | 199 | 200 | 0.7 | 0.9 | 42.4 | 0.0 | 46.9 | update 200 |
| chokidar | 200 | 0 | 0 | 0 | 0 | 3.7 | 5.9 | 38.7 | 0.0 | 1031 | update 200 |
| parcel | 200 | 0 | 0 | 0 | 1 | 64.0 | 64.6 | 79.7 | 0.0 | 141 | update 200 |
| fs-watch | 200 | 0 | 0 | 208 | 204 | 3.3 | 4.2 | 13.7 | 0.0 | 500 | change 200 |
| watchdog | 200 | 0 | 0 | 199 | 165 | 1.0 | 1.5 | 13.5 | 0.0 | 203 | update 200 |

#### new files in new directories

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5000 | 0 | 0 | 0 | 30 | 1755 | 2778 | 2876 | 2816 | 562 | create 5000 |
| pathwatch-0 | 5000 | 0 | 0 | 0 | 4682 | 1632 | 3135 | 3291 | 3291 | 1484 | create 5000 |
| notify | 5000 | 0 | 0 | 5500 | 5000 | 1478 | 2796 | 2936 | 2936 | 656 | create 5000 |
| notify-fork | 5000 | 0 | 0 | 5500 | 5000 | 1441 | 2736 | 2879 | 2879 | 516 | create 5000 |
| chokidar | 5000 | 0 | 0 | 500 | 0 | 4469 | 4524 | 4551 | 2734 | 4891 | create 5000 |
| parcel | 5000 | 0 | 0 | 500 | 3 | 1973 | 3290 | 3290 | 3232 | 1000 | create 5000 |
| fs-watch | 5000 | 4252 | 0 | 919 | 715 | 1387 | 2662 | 2806 | 2786 | 1344 | change 17, rename 731 |
| watchdog | 5000 | 0 | 0 | 5500 | 5533 | 1543 | 2941 | 3107 | 3107 | 1984 | create 5000 |

#### a directory tree removed

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5500 | 0 | 0 | 0 | 0 | 356 | 533 | 533 | 353 | 78.1 | delete 5500 |
| pathwatch-0 | 5500 | 0 | 0 | 0 | 0 | 260 | 440 | 440 | 327 | 141 | delete 5500 |
| notify | 5500 | 0 | 0 | 1082 | 0 | 151 | 315 | 327 | 331 | 62.5 | delete 5500 |
| notify-fork | 5500 | 0 | 0 | 1082 | 0 | 161 | 297 | 311 | 320 | 125 | delete 5500 |
| chokidar | 5500 | 0 | 0 | 551 | 0 | 267 | 690 | 690 | 372 | 844 | delete 5500 |
| parcel | 5500 | 0 | 0 | 551 | 0 | 227 | 375 | 375 | 312 | 46.9 | delete 5500 |
| fs-watch | 5500 | 67 | 0 | 663 | 0 | 153 | 286 | 299 | 314 | 78.1 | rename 5433 |
| watchdog | 5500 | 0 | 0 | 1082 | 0 | 149 | 286 | 300 | 306 | 203 | delete 5500 |

#### a directory tree moved in

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5000 | 0 | 0 | 0 | 0 | 77.9 | 78.3 | 78.3 | 3.3 | 15.6 | create 5000 |
| pathwatch-0 | 5000 | 0 | 0 | 0 | 0 | 23.0 | 23.4 | 23.4 | 3.6 | 15.6 | create 5000 |
| notify | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 3.5 | 0.0 |  |
| notify-fork | 5000 | 5000 | 0 | 3 | 0 | - | - | - | 3.5 | 0.0 |  |
| chokidar | 5000 | 0 | 0 | 502 | 0 | 595 | 842 | 843 | 3.5 | 1109 | create 5000 |
| parcel | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 3.6 | 0.0 |  |
| fs-watch | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 3.5 | 0.0 |  |
| watchdog | 5000 | 0 | 0 | 943 | 0 | 27.1 | 42.6 | 44.3 | 3.4 | 46.9 | create 5000 |

#### a directory tree moved out

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 5000 | 0 | 0 | 0 | 0 | 174 | 174 | 174 | 2.6 | 15.6 | delete 5000 |
| pathwatch-0 | 5000 | 0 | 0 | 0 | 0 | 109 | 110 | 110 | 1.9 | 0.0 | delete 5000 |
| notify | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 1.7 | 0.0 |  |
| notify-fork | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 1.8 | 0.0 |  |
| chokidar | 5000 (stopped early) | 5000 | 0 | 0 | 0 | - | - | - | 0.8 | 0.0 |  |
| parcel | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 1.7 | 0.0 |  |
| fs-watch | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 1.7 | 0.0 |  |
| watchdog | 5000 | 5000 | 0 | 1 | 0 | - | - | - | 1.9 | 0.0 |  |

#### Notes

- notify, a directory tree moved in: missed for example `...\project\moved\g00\d138\m9.js`
- notify, a directory tree moved out: missed for example `...\project\moved\g00\d272\m3.js`
- notify-fork, a directory tree moved in: missed for example `...\project\moved\g00\d065\m9.js`
- notify-fork, a directory tree moved out: missed for example `...\project\moved\g00\d138\m9.js`
- chokidar: a directory tree moved out: the system refused a change of the driver: Access is denied. (os error 5)
- chokidar, a directory tree moved out: missed for example `...\project\moved\g00\d186\m3.js`
- parcel, a directory tree moved in: missed for example `...\project\moved\g00\d361\m1.js`
- parcel, a directory tree moved out: missed for example `...\project\moved\g00\d342\m7.js`
- fs-watch, new files in new directories: missed for example `...\project\g03\new363\m5.js`
- fs-watch, a directory tree removed: missed for example `...\project\g01\d002\m9.js`
- fs-watch, a directory tree moved in: missed for example `...\project\moved\g00\d137\m5.js`
- fs-watch, a directory tree moved out: missed for example `...\project\moved\g00\d412\m1.js`
- watchdog, a directory tree moved out: missed for example `...\project\moved\g00\d292\m4.js`

### S3: noise: module files while another process writes

Sizes: dirs 2000, files 20000, watched 20000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 143 | 141 | 13.2 | +9.3 | 10.4 | 13.3 | 14.1 | 156 | 0 |
| pathwatch-0 | 146 | 156 | 13.0 | +9.0 | 10.1 | 13.0 | 13.8 | 203 | 0 |
| notify | 1638 | 1750 | 330.7 | +326.8 | 328.8 | 330.8 | 330.8 | 1875 | 0 |
| notify-fork | 255 | 250 | 17.5 | +13.5 | 14.8 | 17.5 | 18.8 | 719 | 0 |
| chokidar | 2112 | 2406 | 381.3 | +344.0 | 402.9 | 372.4 | 381.5 | 2688 | 0 |
| parcel | 162 | 141 | 55.1 | +17.2 | 69.2 | 70.8 | 70.8 | 16109 | 0 |
| fs-watch | 144 | 156 | 51.7 | +16.1 | 34.2 | 52.1 | 53.3 | 8453 | 0 |
| watchdog | 160 | 156 | 22.4 | +3.2 | 15.0 | 22.4 | 22.4 | 2438 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 64.5 | 64.5 | 64.5 | 0.0 | 0.0 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 1 | 0.3 | 0.3 | 0.3 | 0.0 | 0.0 | update 1 |
| notify | 1 | 0 | 0 | 0 | 1 | 0.5 | 0.5 | 0.5 | 0.0 | 0.0 | update 1 |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 0.2 | 0.2 | 0.2 | 0.0 | 0.0 | update 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 1.3 | 1.3 | 1.3 | 0.0 | 93.8 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 0.4 | 0.4 | 0.4 | 0.0 | 0.0 | update 1 |
| fs-watch | 1 | 0 | 0 | 2 | 1 | 1.4 | 1.4 | 1.4 | 0.0 | 0.0 | change 1 |
| watchdog | 1 | 0 | 0 | 1 | 0 | 0.5 | 0.5 | 0.5 | 0.0 | 0.0 | update 1 |

#### single edits under noise

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 63.3 | 64.7 | 65.4 | 0.0 | 15.6 | update 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 92 | 0.7 | 2.5 | 4.5 | 0.0 | 46.9 | update 100 |
| notify | 100 | 0 | 0 | 0 | 100 | 0.6 | 1.8 | 4.0 | 0.0 | 15.6 | update 100 |
| notify-fork | 100 | 0 | 0 | 0 | 100 | 0.4 | 1.8 | 3.2 | 0.0 | 438 | update 100 |
| chokidar | 100 | 0 | 0 | 0 | 1 | 1.0 | 3.2 | 4.6 | 0.0 | 78.1 | update 100 |
| parcel | 100 | 0 | 83074 | 0 | 0 | 462 | 477 | 487 | 0.0 | 15828 | update 100 |
| fs-watch | 5 (stopped early) | 5 | 14491 | 232 | 0 | - | - | - | 0.0 | 8172 |  |
| watchdog | 100 | 0 | 19615 | 99 | 100 | 0.9 | 2.5 | 3.5 | 0.0 | 2109 | update 100 |

- pathwatch: noise files 50000, noise writes 17436, noise writes per second 1806
- pathwatch-0: noise files 50000, noise writes 7636, noise writes per second 1950
- notify: noise files 50000, noise writes 7379, noise writes per second 1956
- notify-fork: noise files 50000, noise writes 7208, noise writes per second 1955
- chokidar: noise files 50000, noise writes 7817, noise writes per second 1935
- parcel: noise files 50000, noise writes 82599, noise writes per second 1718
- fs-watch: noise files 50000, noise writes 26858, noise writes per second 1747
- watchdog: noise files 50000, noise writes 6493, noise writes per second 1904

#### quiet after the noise

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 0.0 | 1013 | 0 |
| pathwatch-0 | 0.0 | 1011 | 0 |
| notify | 0.0 | 1012 | 0 |
| notify-fork | 0.0 | 1013 | 0 |
| chokidar | 0.0 | 1015 | 0 |
| parcel | 0.0 | 1014 | 0 |
| fs-watch | 0.0 | 1010 | 0 |
| watchdog | 0.0 | 1003 | 0 |

#### Notes

- parcel: @parcel/watcher watches directories only: it watches ...\project for 20000 paths
- parcel, single edits under noise: extra for example `create ...\project\dist\n0000`
- fs-watch: fs.watch is called once, for ...\project, for 20000 paths
- fs-watch, single edits under noise: missed for example `...\project\g00\d000\m0.js`
- fs-watch, single edits under noise: extra for example `rename ...\project\dist\n0000`
- watchdog: watchdog watches directories: it watches ...\project for 20000 paths
- watchdog, single edits under noise: extra for example `create-dir ...\project\dist\n0000`

### S4: ignored directory: writes into node_modules

Sizes: dirs 500, files 5000, files written into node_modules 30000, watched 1.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 30.5 | 31.2 | 4.8 | +0.9 | 2.8 | 4.9 | 6.4 | 2203 | 0 |
| pathwatch-0 | 27.1 | 15.6 | 5.7 | +1.8 | 2.5 | 4.9 | 6.1 | 4312 | 0 |
| notify | 5.7 | 0.0 | 4.1 | +0.2 | 0.8 | 4.1 | 4.1 | 2875 | 0 |
| notify-fork | 9.2 | 15.6 | 4.2 | +0.2 | 0.8 | 4.2 | 4.2 | 1562 | 0 |
| chokidar | 1030 | 1344 | 186.7 | +149.3 | 257.4 | 515.3 | 515.3 | 17438 | 0 |
| parcel | 51.5 | 31.2 | 40.4 | +2.5 | 57.4 | 40.6 | 40.6 | 1891 | 0 |
| fs-watch | 30.2 | 31.2 | 35.5 | -0.1 | 21.5 | 41.7 | 42.9 | 9797 | 0 |
| watchdog | 55.6 | 46.9 | 19.4 | +0.2 | 12.9 | 19.6 | 19.6 | 9312 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 53.1 | 53.1 | 53.1 | 0.0 | 0.0 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 1 | 0.2 | 0.2 | 0.2 | 0.0 | 0.0 | update 1 |
| notify | 1 | 0 | 0 | 1 | 1 | 0.3 | 0.3 | 0.3 | 0.0 | 0.0 | update 1 |
| notify-fork | 1 | 0 | 0 | 1 | 1 | 0.3 | 0.3 | 0.3 | 0.0 | 0.0 | update 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 2.2 | 2.2 | 2.2 | 0.0 | 0.0 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 0.4 | 0.4 | 0.4 | 0.0 | 0.0 | update 1 |
| fs-watch | 1 | 0 | 0 | 3 | 1 | 1.7 | 1.7 | 1.7 | 0.0 | 0.0 | change 1 |
| watchdog | 1 | 0 | 0 | 1 | 1 | 0.3 | 0.3 | 0.3 | 0.0 | 0.0 | update 1 |

#### node_modules written, then one edit in src

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 51.8 | 51.8 | 51.8 | 18839 | 2172 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 1 | 0.3 | 0.3 | 0.3 | 18889 | 4297 | update 1 |
| notify | 1 | 0 | 93600 | 1 | 1 | 0.7 | 0.7 | 0.7 | 18933 | 2875 | update 1 |
| notify-fork | 1 | 0 | 0 | 1 | 1 | 0.4 | 0.4 | 0.4 | 19251 | 1547 | update 1 |
| chokidar | 1 | 0 | 17327 | 0 | 0 | 744 | 744 | 744 | 19177 | 11891 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 2 | 0.5 | 0.5 | 0.5 | 18905 | 1828 | update 1 |
| fs-watch | 1 | 1 | 14996 | 659 | 0 | - | - | - | 19250 | 9656 |  |
| watchdog | 1 | 0 | 0 | 1 | 1 | 0.6 | 0.6 | 0.6 | 19691 | 9266 | update 1 |

#### single edits in src

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 20 | 0 | 0 | 0 | 0 | 63.4 | 63.9 | 64.1 | 0.0 | 0.0 | update 20 |
| pathwatch-0 | 20 | 0 | 0 | 0 | 11 | 0.5 | 2.1 | 4.1 | 0.0 | 0.0 | update 20 |
| notify | 20 | 0 | 0 | 18 | 20 | 0.4 | 1.0 | 2.3 | 0.0 | 0.0 | update 20 |
| notify-fork | 20 | 0 | 0 | 18 | 20 | 0.5 | 1.1 | 1.6 | 0.0 | 0.0 | update 20 |
| chokidar | 20 | 0 | 16273 | 0 | 2 | 3.8 | 796 | 828 | 0.0 | 4000 | update 20 |
| parcel | 20 | 0 | 0 | 0 | 0 | 63.7 | 64.6 | 64.9 | 0.0 | 15.6 | update 20 |
| fs-watch | 20 | 0 | 0 | 27 | 20 | 3.6 | 4.5 | 4.8 | 0.0 | 109 | change 20 |
| watchdog | 20 | 0 | 0 | 18 | 15 | 0.6 | 0.7 | 1.0 | 0.0 | 0.0 | update 20 |

#### Notes

- notify: notify has no ignore rule: every event of the library is printed
- notify, node_modules written, then one edit in src: extra for example `update ...\project\node_modules`
- chokidar, node_modules written, then one edit in src: extra for example `create-dir ...\project\node_modules\package000`
- chokidar, single edits in src: extra for example `create ...\project\node_modules\package149\g00\d000\m2.js`
- fs-watch: fs.watch has no ignore rule: every event is printed
- fs-watch, node_modules written, then one edit in src: missed for example `...\project\src\g05\d000\m0.js`
- fs-watch, node_modules written, then one edit in src: extra for example `change ...\project\node_modules`
- watchdog: watchdog watches the ignored directory too, and drops its events

### S5: polling: module files, polled every 100 ms

Sizes: dirs 2000, files 20000, watched 20000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 133 | 125 | 8.8 | +4.9 | 6.8 | 9.0 | 12.5 | 5250 | 0 |
| pathwatch-0 | 129 | 125 | 8.9 | +4.9 | 6.1 | 9.0 | 12.5 | 5203 | 0 |
| notify | 1199 | 1203 | 23.6 | +19.7 | 21.2 | 23.7 | 23.7 | 16125 | 0 |
| notify-fork | 34.9 | 31.2 | 15.0 | +11.1 | 13.6 | 19.2 | 19.3 | 22094 | 0 |
| chokidar | 436 | 969 | 311.6 | +274.2 | 331.3 | 116.1 | 311.6 | 21141 | 0 |
| parcel | unsupported: @parcel/watcher has no polling mode |
| fs-watch | 114 | 297 | 107.7 | +72.2 | 132.4 | 83.9 | 116.6 | 21297 | 0 |
| watchdog | 350 | 328 | 45.6 | +26.4 | 39.0 | 67.4 | 76.3 | 18422 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 96.2 | 96.2 | 96.2 | 0.0 | 109 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 99.3 | 99.3 | 99.3 | 0.0 | 93.8 | update 1 |
| notify | 1 | 0 | 0 | 0 | 0 | 103 | 103 | 103 | 0.0 | 78.1 | metadata 1 |
| notify-fork | 1 | 1 | 0 | 0 | 0 | - | - | - | 0.0 | 6828 |  |
| chokidar | 1 | 0 | 0 | 0 | 0 | 97.0 | 97.0 | 97.0 | 0.0 | 219 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 0 | 33.5 | 33.5 | 33.5 | 0.0 | 109 | change 1 |
| watchdog | 1 | 0 | 0 | 0 | 0 | 328 | 328 | 328 | 0.0 | 234 | update 1 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 953 | 10009 | 0 |
| pathwatch-0 | 938 | 10008 | 0 |
| notify | 6859 | 10003 | 0 |
| notify-fork | 6766 | 10011 | 0 |
| chokidar | 12672 | 10011 | 0 |
| fs-watch | 13609 | 10015 | 0 |
| watchdog | 6562 | 10003 | 0 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 50 | 0 | 0 | 0 | 0 | 884 | 929 | 942 | 0.0 | 4062 | update 50 |
| pathwatch-0 | 50 | 0 | 0 | 0 | 0 | 887 | 923 | 942 | 0.0 | 4047 | update 50 |
| notify | 50 | 0 | 0 | 0 | 0 | 193 | 331 | 343 | 0.0 | 7828 | metadata 50 |
| notify-fork | 50 | 0 | 0 | 0 | 0 | 247 | 330 | 337 | 0.0 | 8312 | metadata 50 |
| chokidar | 50 | 0 | 0 | 0 | 0 | 73.0 | 82.5 | 84.5 | 0.0 | 7000 | update 50 |
| fs-watch | 50 | 0 | 0 | 0 | 0 | 72.6 | 85.6 | 104 | 0.0 | 6984 | change 50 |
| watchdog | 50 | 0 | 0 | 0 | 0 | 293 | 312 | 385 | 0.0 | 11109 | update 50 |

#### Notes

- notify-fork, first edit, right after ready: missed for example `...\project\g00\d000\m0.js`
- fs-watch: polling: fs.watchFile for each path
- watchdog: watchdog watches directories: it watches ...\project for 20000 paths

### S6: missing paths: watched before they exist

Sizes: dirs 0, files 0, watched 1000.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 12.8 | 31.2 | 4.5 | +0.6 | 1.3 | 4.6 | 4.7 | 109 | 0 |
| pathwatch-0 | 13.5 | 0.0 | 4.5 | +0.6 | 1.2 | 4.7 | 4.7 | 281 | 0 |
| notify | 29.1 | 31.2 | 4.2 | +0.3 | 1.0 | 4.2 | 4.2 | 31.2 | 4 |
| notify-fork | 19.5 | 15.6 | 5.0 | +1.1 | 1.7 | 5.0 | 5.0 | 31.2 | 1 |
| chokidar | 51.9 | 78.1 | 50.2 | +12.8 | 66.9 | 45.2 | 89.4 | 656 | 0 |
| parcel | 60.2 | 46.9 | 45.4 | +7.5 | 59.4 | 46.4 | 46.4 | 172 | 0 |
| fs-watch | 45.0 | 62.5 | 41.7 | +6.1 | 24.0 | 42.2 | 42.5 | 516 | 0 |
| watchdog | 64.2 | 78.1 | 19.5 | +0.3 | 12.8 | 19.6 | 19.6 | 484 | 0 |

#### quiet

| adapter | CPU | in (wall) | events (none is right) |
| --- | ---: | ---: | ---: |
| pathwatch | 0.0 | 3003 | 0 |
| pathwatch-0 | 0.0 | 3013 | 0 |
| notify | 0.0 | 3005 | 0 |
| notify-fork | 0.0 | 3005 | 0 |
| chokidar | 0.0 | 3013 | 0 |
| parcel | 0.0 | 3015 | 0 |
| fs-watch | 0.0 | 3002 | 0 |
| watchdog | 0.0 | 3008 | 0 |

#### the missing paths are made

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1000 | 0 | 0 | 0 | 3 | 518 | 642 | 642 | 589 | 62.5 | create 1000 |
| pathwatch-0 | 1000 | 0 | 0 | 0 | 1137 | 336 | 633 | 667 | 667 | 281 | create 1000 |
| notify | 1000 | 1000 | 0 | 0 | 0 | - | - | - | 461 | 0.0 |  |
| notify-fork | 1000 | 1000 | 0 | 0 | 0 | - | - | - | 523 | 15.6 |  |
| chokidar | 1000 | 1000 | 0 | 0 | 0 | - | - | - | 469 | 375 |  |
| parcel | 1000 | 0 | 0 | 110 | 1 | 505 | 718 | 718 | 653 | 93.8 | create 1000 |
| fs-watch | 1000 | 829 | 0 | 206 | 166 | 349 | 619 | 619 | 587 | 312 | change 2, rename 169 |
| watchdog | 1000 | 0 | 0 | 1107 | 1105 | 307 | 580 | 614 | 614 | 391 | create 1000 |

#### single edits after

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 20 | 0 | 0 | 0 | 0 | 63.8 | 64.8 | 66.3 | 0.0 | 15.6 | update 20 |
| pathwatch-0 | 20 | 0 | 0 | 0 | 9 | 0.5 | 1.1 | 1.3 | 0.0 | 0.0 | update 20 |
| notify | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| notify-fork | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 0.0 |  |
| chokidar | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 15.6 |  |
| parcel | 20 | 0 | 0 | 0 | 0 | 63.8 | 64.4 | 64.6 | 0.0 | 15.6 | update 20 |
| fs-watch | 20 | 0 | 0 | 18 | 22 | 3.5 | 4.8 | 5.3 | 0.0 | 141 | change 20 |
| watchdog | 20 | 0 | 0 | 20 | 15 | 0.5 | 0.8 | 1.0 | 0.0 | 0.0 | update 20 |

#### Notes

- notify: 4 errors, the first: `watch ...\project\g00\d000\m0.js: Input watch path is neither a file nor a directory.`
- notify, the missing paths are made: missed for example `...\project\g03\d006\m9.js`
- notify, single edits after: missed for example `...\project\g00\d000\m0.js`
- notify-fork: 1 errors, the first: `commit: No path was found. about ["...\\project\\g01\\d002"]`
- notify-fork, the missing paths are made: missed for example `...\project\g04\d006\m4.js`
- notify-fork, single edits after: missed for example `...\project\g00\d000\m0.js`
- chokidar, the missing paths are made: missed for example `...\project\g02\d001\m7.js`
- chokidar, single edits after: missed for example `...\project\g00\d000\m0.js`
- parcel: @parcel/watcher watches directories only: it watches ...\project for 1000 paths
- fs-watch: fs.watch is called once, for ...\project, for 1000 paths
- fs-watch, the missing paths are made: missed for example `...\project\g09\d005\m0.js`
- watchdog: watchdog watches directories: it watches ...\project for 1000 paths

### S7: big directory: one watched directory holds every file

Sizes: dirs 1, files 20000, watched 1.

#### Setup and memory

| adapter | setup | setup CPU | RSS ready | RSS above base | footprint ready | RSS end | RSS peak | CPU whole run | errors |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 12.9 | 15.6 | 6.1 | +2.1 | 3.5 | 6.2 | 9.0 | 328 | 0 |
| pathwatch-0 | 18.7 | 15.6 | 5.9 | +1.9 | 2.8 | 6.3 | 9.0 | 672 | 0 |
| notify | 8.0 | 15.6 | 4.1 | +0.2 | 0.8 | 4.1 | 4.1 | 172 | 0 |
| notify-fork | 3.9 | 0.0 | 4.1 | +0.2 | 0.8 | 4.2 | 4.2 | 266 | 0 |
| chokidar | 4246 | 4891 | 316.7 | +279.4 | 339.3 | 1045.4 | 1190.5 | 530469 | 0 |
| parcel | 50.6 | 46.9 | 40.5 | +2.6 | 57.5 | 42.3 | 44.1 | 625 | 0 |
| fs-watch | 32.9 | 15.6 | 35.4 | -0.1 | 21.6 | 40.5 | 42.3 | 1078 | 0 |
| watchdog | 52.9 | 62.5 | 19.3 | +0.1 | 12.9 | 19.5 | 19.5 | 750 | 0 |

#### first edit, right after ready

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 1 | 0 | 0 | 0 | 0 | 66.1 | 66.1 | 66.1 | 0.0 | 0.0 | update 1 |
| pathwatch-0 | 1 | 0 | 0 | 0 | 0 | 0.3 | 0.3 | 0.3 | 0.0 | 0.0 | update 1 |
| notify | 1 | 0 | 0 | 0 | 1 | 0.2 | 0.2 | 0.2 | 0.0 | 0.0 | update 1 |
| notify-fork | 1 | 0 | 0 | 0 | 1 | 0.2 | 0.2 | 0.2 | 0.0 | 0.0 | update 1 |
| chokidar | 1 | 0 | 0 | 0 | 0 | 1273 | 1273 | 1273 | 0.0 | 1250 | update 1 |
| parcel | 1 | 0 | 0 | 0 | 0 | 0.5 | 0.5 | 0.5 | 0.0 | 0.0 | update 1 |
| fs-watch | 1 | 0 | 0 | 0 | 1 | 0.6 | 0.6 | 0.6 | 0.0 | 0.0 | change 1 |
| watchdog | 1 | 0 | 0 | 0 | 1 | 0.3 | 0.3 | 0.3 | 0.0 | 0.0 | update 1 |

#### single edits

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 63.9 | 64.6 | 69.4 | 0.0 | 78.1 | update 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 71 | 0.7 | 1.1 | 12.8 | 0.0 | 46.9 | update 100 |
| notify | 100 | 0 | 0 | 0 | 100 | 0.5 | 0.8 | 11.5 | 0.0 | 31.2 | update 100 |
| notify-fork | 100 | 0 | 0 | 0 | 100 | 0.6 | 0.8 | 10.7 | 0.0 | 0.0 | update 100 |
| chokidar | 100 | 0 | 0 | 0 | 58 | 1804 | 2789 | 2882 | 0.0 | 256344 | update 100 |
| parcel | 100 | 0 | 0 | 0 | 0 | 63.9 | 64.7 | 72.0 | 0.0 | 31.2 | update 100 |
| fs-watch | 100 | 0 | 0 | 0 | 124 | 1.5 | 2.3 | 2.7 | 0.0 | 234 | change 100 |
| watchdog | 100 | 0 | 0 | 0 | 92 | 0.9 | 1.3 | 1.6 | 0.0 | 78.1 | update 100 |

#### single new files

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 64.1 | 64.9 | 67.3 | 0.0 | 62.5 | create 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 107 | 1.0 | 1.4 | 1.6 | 0.0 | 46.9 | create 100 |
| notify | 100 | 0 | 0 | 0 | 100 | 0.6 | 0.8 | 0.9 | 0.0 | 15.6 | create 100 |
| notify-fork | 100 | 0 | 0 | 99 | 100 | 0.6 | 1.0 | 11.0 | 0.0 | 15.6 | create 100 |
| chokidar | 5 (stopped early) | 5 | 0 | 0 | 0 | - | - | - | 0.0 | 19625 |  |
| parcel | 100 | 0 | 0 | 0 | 0 | 64.0 | 64.7 | 70.9 | 0.0 | 78.1 | create 100 |
| fs-watch | 100 | 0 | 0 | 0 | 158 | 1.8 | 2.7 | 11.4 | 0.0 | 188 | rename 100 |
| watchdog | 100 | 0 | 0 | 0 | 100 | 0.9 | 1.4 | 1.6 | 0.0 | 78.1 | create 100 |

#### burst of new files

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 2000 | 0 | 0 | 0 | 2 | 791 | 1198 | 1198 | 1091 | 141 | create 2000 |
| pathwatch-0 | 2000 | 0 | 0 | 0 | 1415 | 667 | 1252 | 1311 | 1311 | 531 | create 2000 |
| notify | 2000 | 0 | 0 | 0 | 2000 | 478 | 886 | 940 | 940 | 109 | create 2000 |
| notify-fork | 2000 | 0 | 0 | 2000 | 2000 | 495 | 875 | 925 | 925 | 234 | create 2000 |
| chokidar | 2000 | 0 | 32719 | 0 | 2165 | 6764 | 8145 | 8145 | 2153 | 12422 | create 2000 |
| parcel | 2000 | 0 | 0 | 0 | 2 | 968 | 1355 | 1355 | 1295 | 359 | create 2000 |
| fs-watch | 2000 | 1311 | 0 | 14 | 676 | 552 | 1009 | 1048 | 1003 | 641 | change 8, rename 681 |
| watchdog | 2000 | 0 | 0 | 0 | 2000 | 590 | 1136 | 1194 | 1193 | 531 | create 2000 |

#### single files removed

| adapter | changes | missed | extra | dir events | repeats | p50 | p95 | max | write | CPU | first word |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| pathwatch | 100 | 0 | 0 | 0 | 0 | 175 | 177 | 178 | 0.0 | 31.2 | delete 100 |
| pathwatch-0 | 100 | 0 | 0 | 0 | 0 | 112 | 114 | 116 | 0.0 | 15.6 | delete 100 |
| notify | 100 | 0 | 0 | 0 | 0 | 0.4 | 0.7 | 2.8 | 0.0 | 0.0 | delete 100 |
| notify-fork | 100 | 0 | 0 | 0 | 0 | 0.4 | 0.7 | 12.5 | 0.0 | 15.6 | delete 100 |
| chokidar | 100 | 8 | 0 | 0 | 72061 | 1855 | 2975 | 3002 | 0.0 | 235703 | delete 92 |
| parcel | 100 | 0 | 0 | 0 | 0 | 64.0 | 64.9 | 71.2 | 0.0 | 93.8 | delete 100 |
| fs-watch | 100 | 0 | 0 | 0 | 0 | 0.6 | 0.8 | 4.2 | 0.0 | 0.0 | rename 100 |
| watchdog | 100 | 0 | 0 | 0 | 0 | 0.5 | 1.0 | 3.1 | 0.0 | 0.0 | delete 100 |

#### Notes

- chokidar, single new files: missed for example `...\project\new00000.js`
- chokidar, burst of new files: extra for example `update ...\project\m00001.js`
- chokidar, single files removed: missed for example `...\project\m01000.js`
- fs-watch, burst of new files: missed for example `...\project\burst01056.js`

