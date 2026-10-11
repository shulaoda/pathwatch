# watchfiles, used the way `watchfiles.watch` works inside: the `RustNotify` class behind it,
# with the defaults of `watch` (changes are handed over 50 ms after the last one, and after
# 1600 ms at the latest), so that `ready` is said when the watcher is live. The protocol is
# described in ../adapters/src/lib.rs.
#
# With `poll`: `force_polling` with the pause. With `ignore`: the changes of such paths are
# dropped, as a `watch_filter` would drop them. Without it nothing is dropped: the default
# filter of `watch` would drop `node_modules` and dotfiles, which the benchmark wants to see.

import os
import threading

from watchfiles import Change
from watchfiles._rust_notify import RustNotify

from protocol import event, has_part, line, read_job, start_writer, stdin, write

WORDS = {Change.added: "create", Change.modified: "update", Change.deleted: "delete"}

# The defaults of `watchfiles.watch`.
DEBOUNCE_MS = 1600
STEP_MS = 50
RUST_TIMEOUT_MS = 5000


def run(watcher, ignore):
    while True:
        changes = watcher.watch(DEBOUNCE_MS, STEP_MS, RUST_TIMEOUT_MS, None)
        if isinstance(changes, str):
            if changes == "stop":
                return
            continue
        for kind, path in changes:
            if ignore is None or not has_part(path, ignore):
                event(WORDS.get(Change(kind), str(kind)), path)


def main():
    job = read_job()
    start_writer()
    if job["paths"]:
        polling = job["poll"] is not None
        pause = job["poll"] if polling else 300
        try:
            watcher = RustNotify(job["paths"], False, polling, pause, True, False)
        except Exception as problem:  # noqa: BLE001: whatever the library raises is its error
            line("error", problem)
        else:
            threading.Thread(target=run, args=(watcher, job["ignore"]), daemon=True).start()
    write(b"ready\n")

    # When stdin is closed the adapter exits.
    for _ in stdin:
        pass
    os._exit(0)


main()
