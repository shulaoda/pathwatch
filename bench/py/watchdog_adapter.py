# watchdog, used as its documentation shows: an `Observer`, one handler, and `schedule` with
# `recursive=True` for a directory. The protocol is described in ../adapters/src/lib.rs.
#
# It watches directories. For a set of files the adapter watches the lowest directory that
# holds them all, so the library also reports files that the job did not name.
# With `poll`: `PollingObserver` with the pause as its timeout.
# With `ignore`: `RegexMatchingEventHandler` with `ignore_regexes`. The library still watches
# the ignored directory and drops its events.

import os
import queue
import re
import sys
import threading

from watchdog.events import FileSystemEventHandler, RegexMatchingEventHandler
from watchdog.observers import Observer
from watchdog.observers.polling import PollingObserver

# The driver writes UTF-8, whatever the language of the system is.
stdin = sys.stdin.buffer
stdout = sys.stdout.buffer


def read_job():
    """Reads the job from stdin, up to the `start` line."""
    job = {"paths": [], "ignore": None, "poll": None}
    for raw in stdin:
        line = raw.decode("utf-8").rstrip("\r\n")
        command, _, rest = line.partition("\t")
        if command == "ignore":
            job["ignore"] = rest
        elif command == "poll":
            job["poll"] = int(rest)
        elif command == "watch":
            job["paths"].append(rest)
        elif command == "start":
            return job
    # The driver went away before the job was complete.
    os._exit(0)


def write(data):
    try:
        stdout.write(data)
        stdout.flush()
    except OSError:
        # The driver is gone when the pipe is closed.
        os._exit(0)


def line(word, text):
    # A line is one message: an error text of many lines becomes one line.
    text = str(text).replace("\n", " ").replace("\t", " ")
    write(f"{word}\t{text}\n".encode("utf-8"))


# Events are collected and written in one piece, so a batch of many events is one write.
waiting = queue.SimpleQueue()


def write_events():
    while True:
        lines = [waiting.get()]
        try:
            while True:
                lines.append(waiting.get_nowait())
        except queue.Empty:
            pass
        write("".join(lines).encode("utf-8"))


def event(kind, path):
    waiting.put(f"event\t{kind}\t{os.fsdecode(path)}\n")


def common_directory(paths):
    """The lowest directory that holds every path. A path that is a directory counts as
    itself."""
    common = None
    for one in paths:
        directory = one if os.path.isdir(one) else os.path.dirname(one)
        parts = directory.split(os.sep)
        if common is None:
            common = parts
            continue
        same = 0
        while same < len(common) and same < len(parts) and common[same] == parts[same]:
            same += 1
        del common[same:]
    if common is None:
        return None
    return os.sep.join(common) or os.sep


# The words of the library, and the words of the benchmark for them. Opening and closing a
# file is not a change (Linux reports both).
WORDS = {
    "created": "create",
    "modified": "update",
    "deleted": "delete",
    "opened": "access",
    "closed": "access",
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
    threading.Thread(target=write_events, daemon=True).start()

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
