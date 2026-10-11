# What every Python adapter shares: the lines that it reads and the lines that it prints. The
# protocol is described in ../adapters/src/lib.rs.

import os
import queue
import sys
import threading

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


def _write_events():
    while True:
        lines = [waiting.get()]
        try:
            while True:
                lines.append(waiting.get_nowait())
        except queue.Empty:
            pass
        write("".join(lines).encode("utf-8"))


def start_writer():
    """Starts the thread that writes the events."""
    threading.Thread(target=_write_events, daemon=True).start()


def event(kind, path):
    waiting.put(f"event\t{kind}\t{os.fsdecode(path)}\n")


def has_part(path, name):
    """The ignore rule of the benchmark: the path has a part with this name."""
    return name in os.fsdecode(path).split(os.sep)


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
