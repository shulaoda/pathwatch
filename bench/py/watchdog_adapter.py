# watchdog, used as its documentation shows: an `Observer`, one handler, and `schedule` with
# `recursive=True` for a directory. The protocol is described in ../adapters/src/lib.rs.
#
# It watches directories. For a set of files the adapter watches the lowest directory that
# holds them all, so the library also reports files that the job did not name.
# With `poll`: `PollingObserver` with the pause as its timeout.
# With `ignore`: `RegexMatchingEventHandler` with `ignore_regexes`. The library still watches
# the ignored directory and drops its events.

import os
import re

from watchdog.events import FileSystemEventHandler, RegexMatchingEventHandler
from watchdog.observers import Observer
from watchdog.observers.polling import PollingObserver

from protocol import common_directory, event, line, read_job, start_writer, stdin, write

# The words of the library, and the words of the benchmark for them. Opening a file, and
# closing one that was not written, is not a change (Linux reports both). The close after a
# write is an event of the file, as notify-fork reports the same event of Linux.
WORDS = {
    "created": "create",
    "modified": "update",
    "deleted": "delete",
    "opened": "access",
    "closed": "close-write",
    "closed_no_write": "access",
}


class Printer:
    def on_any_event(self, change):
        tail = "-dir" if change.is_directory else ""
        if change.event_type == "moved":
            event("move-from" + tail, change.src_path)
            event("move-to" + tail, change.dest_path)
            return
        word = WORDS.get(change.event_type, change.event_type)
        event(word if word == "access" else word + tail, change.src_path)


class Handler(Printer, FileSystemEventHandler):
    pass


class IgnoringHandler(Printer, RegexMatchingEventHandler):
    pass


def main():
    job = read_job()
    start_writer()

    directory = common_directory(job["paths"])
    if directory is not None:
        if job["paths"] != [directory]:
            count = len(job["paths"])
            line("note", f"watchdog watches directories: it watches {directory} for {count} paths")
        if job["ignore"] is None:
            handler = Handler()
        else:
            name = re.escape(job["ignore"])
            line("note", "watchdog watches the ignored directory too, and drops its events")
            handler = IgnoringHandler(ignore_regexes=[rf".*[\\/]{name}([\\/].*)?$"])
        if job["poll"] is None:
            observer = Observer()
        else:
            observer = PollingObserver(timeout=job["poll"] / 1000)
        try:
            observer.schedule(handler, directory, recursive=True)
            observer.start()
        except Exception as problem:  # noqa: BLE001: whatever the library raises is its error
            line("error", problem)
    write(b"ready\n")

    # When stdin is closed the adapter exits.
    for _ in stdin:
        pass
    os._exit(0)


main()
