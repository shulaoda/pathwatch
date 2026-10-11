// fsnotify, used as its README shows: one Watcher, Add for each directory, and a loop over
// Events and Errors. The protocol is described in ../adapters/src/lib.rs.
//
// fsnotify watches one directory, not what is below it. So the adapter adds every directory
// of a watched tree, and each directory that comes later, as the README of fsnotify tells. A
// file is added by itself. With `ignore`: the directories with that name are not added, and
// the events of paths with that name are dropped. It has no polling mode.
package main

import (
	"bufio"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"github.com/fsnotify/fsnotify"
)

type job struct {
	paths  []string
	ignore string
	poll   bool
}

// Reads the job from stdin, up to the `start` line.
func readJob(stdin *bufio.Reader) job {
	var j job
	for {
		line, err := stdin.ReadString('\n')
		if err != nil {
			// The driver went away before the job was complete.
			os.Exit(0)
		}
		command, rest, _ := strings.Cut(strings.TrimRight(line, "\r\n"), "\t")
		switch command {
		case "ignore":
			j.ignore = rest
		case "poll":
			j.poll = true
		case "watch":
			j.paths = append(j.paths, rest)
		case "start":
			return j
		}
	}
}

// The ignore rule of the benchmark: the path has a part with this name.
func hasPart(path, name string) bool {
	for _, part := range strings.Split(path, string(os.PathSeparator)) {
		if part == name {
			return true
		}
	}
	return false
}

func word(op fsnotify.Op) string {
	switch {
	case op.Has(fsnotify.Create):
		return "create"
	case op.Has(fsnotify.Write):
		return "update"
	case op.Has(fsnotify.Remove):
		return "delete"
	case op.Has(fsnotify.Rename):
		return "rename"
	case op.Has(fsnotify.Chmod):
		return "metadata"
	}
	return "other"
}

func main() {
	stdin := bufio.NewReader(os.Stdin)
	out := bufio.NewWriterSize(os.Stdout, 64*1024)
	// A line is one message: an error text of many lines becomes one line.
	line := func(word, text string) {
		fmt.Fprintf(out, "%s\t%s\n", word, strings.NewReplacer("\n", " ", "\t", " ").Replace(text))
		out.Flush()
	}
	j := readJob(stdin)
	if j.poll {
		line("unsupported", "fsnotify has no polling mode")
		return
	}
	watcher, err := fsnotify.NewWatcher()
	if err != nil {
		line("error", err.Error())
		return
	}
	ignored := func(path string) bool { return j.ignore != "" && hasPart(path, j.ignore) }

	failed := 0
	add := func(path string) {
		if err := watcher.Add(path); err != nil {
			failed++
			// One error for each path would be thousands of equal lines.
			if failed <= 3 {
				line("error", "watch "+path+": "+err.Error())
			}
		}
	}
	// Every directory of a tree gets a watch of its own.
	addTree := func(dir string) {
		filepath.WalkDir(dir, func(path string, entry fs.DirEntry, err error) error {
			if err != nil || !entry.IsDir() {
				return nil
			}
			if ignored(path) {
				return filepath.SkipDir
			}
			add(path)
			return nil
		})
	}
	trees := 0
	for _, path := range j.paths {
		if info, err := os.Stat(path); err == nil && info.IsDir() {
			trees++
			addTree(path)
		} else {
			add(path)
		}
	}
	if trees > 0 {
		line("note", "fsnotify watches one directory: the adapter adds each directory of a tree, and each new one")
	}
	if failed > 0 {
		line("error", fmt.Sprintf("watch failed for %d of %d paths", failed, len(j.paths)))
	}

	// When stdin is closed the adapter exits. The reader is started before `ready`, so the
	// memory at `ready` holds everything that the adapter needs.
	go func() {
		for {
			if _, err := stdin.ReadString('\n'); err != nil {
				os.Exit(0)
			}
		}
	}()
	fmt.Fprintln(out, "ready")
	out.Flush()

	handle := func(event fsnotify.Event) {
		if ignored(event.Name) {
			return
		}
		fmt.Fprintf(out, "event\t%s\t%s\n", word(event.Op), event.Name)
		if event.Has(fsnotify.Create) {
			if info, err := os.Stat(event.Name); err == nil && info.IsDir() {
				addTree(event.Name)
			}
		}
	}
	for {
		select {
		case event, ok := <-watcher.Events:
			if !ok {
				return
			}
			handle(event)
			// Everything that waits goes out in one write.
		drain:
			for {
				select {
				case event, ok := <-watcher.Events:
					if !ok {
						return
					}
					handle(event)
				default:
					break drain
				}
			}
			out.Flush()
		case err, ok := <-watcher.Errors:
			if !ok {
				return
			}
			line("error", err.Error())
		}
	}
}
