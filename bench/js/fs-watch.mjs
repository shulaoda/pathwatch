// `fs.watch` of Node with `recursive: true`: the floor, with no library at all.
//
// It is called once, for the lowest directory that holds every path of the job, so it also
// reports files that the job did not name. Its words are `rename` (a name appeared or went
// away) and `change`. It has no ignore rule.
// With `poll`: `fs.watchFile` for each path, which is the polling API of Node.

import fs from 'node:fs';
import path from 'node:path';

import { commonDirectory, error, event, note, readJob, ready } from './protocol.mjs';

const job = await readJob();
if (job.ignore !== null) note('fs.watch has no ignore rule: every event is printed');

if (job.poll !== null) {
  note('polling: fs.watchFile for each path');
  for (const file of job.paths) {
    fs.watchFile(file, { interval: job.poll }, (now, before) => {
      // A file that is not there has a size and times of zero.
      const kind = now.nlink === 0 ? 'delete' : before.nlink === 0 ? 'create' : 'change';
      event(kind, file);
    });
  }
} else {
  const directory = commonDirectory(job.paths);
  if (directory !== null) {
    if (job.paths.length !== 1 || job.paths[0] !== directory) {
      note(`fs.watch is called once, for ${directory}, for ${job.paths.length} paths`);
    }
    try {
      const watcher = fs.watch(directory, { recursive: true }, (kind, name) => {
        event(kind, name === null ? directory : path.join(directory, name));
      });
      watcher.on('error', error);
    } catch (problem) {
      error(problem);
    }
  }
}
ready();
