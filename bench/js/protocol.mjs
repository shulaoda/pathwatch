// What every Node adapter shares: the lines that it reads and the lines that it prints.
// The protocol is described in ../adapters/src/lib.rs.

import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';

// Reads the job from stdin, up to the `start` line. When stdin is closed, the adapter exits.
export function readJob() {
  return new Promise((resolve) => {
    const job = { paths: [], ignore: null, poll: null };
    const lines = readline.createInterface({ input: process.stdin });
    lines.on('line', (line) => {
      const tab = line.indexOf('\t');
      const command = tab < 0 ? line : line.slice(0, tab);
      const rest = tab < 0 ? '' : line.slice(tab + 1);
      if (command === 'ignore') job.ignore = rest;
      else if (command === 'poll') job.poll = Number(rest);
      else if (command === 'watch') job.paths.push(rest);
      else if (command === 'start') resolve(job);
    });
    lines.on('close', () => process.exit(0));
  });
}

// Events are collected and written in one piece at the end of the current turn of the event
// loop, so a batch of many events is one write.
let waiting = '';

function flush() {
  process.stdout.write(waiting);
  waiting = '';
}

export function event(kind, file) {
  if (waiting === '') setImmediate(flush);
  waiting += `event\t${kind}\t${file}\n`;
}

function line(word, text) {
  // A line is one message: an error text of many lines becomes one line.
  process.stdout.write(`${word}\t${String(text).replace(/[\n\t]/g, ' ')}\n`);
}

export const note = (text) => line('note', text);
export const error = (text) => line('error', text);

// The library cannot do this job.
export function unsupported(text) {
  line('unsupported', text);
  process.exit(0);
}

export function ready() {
  process.stdout.write('ready\n');
}

// The ignore rule of the benchmark: the path has a part with this name.
export function hasPart(file, name) {
  return file.split(path.sep).includes(name);
}

// The lowest directory that holds every path of the job: for a library that watches
// directories only. A path that is a directory counts as itself.
export function commonDirectory(paths) {
  let common = null;
  for (const one of paths) {
    const isDirectory = fs.statSync(one, { throwIfNoEntry: false })?.isDirectory() ?? false;
    const parts = (isDirectory ? one : path.dirname(one)).split(path.sep);
    if (common === null) {
      common = parts;
      continue;
    }
    let same = 0;
    while (same < common.length && same < parts.length && common[same] === parts[same]) same++;
    common.length = same;
  }
  return common === null ? null : common.join(path.sep) || path.sep;
}
