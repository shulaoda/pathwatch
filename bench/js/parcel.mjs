// @parcel/watcher 2, used as its README shows: `subscribe(dir, callback, options)`.
//
// It watches directories only. For a set of files the adapter watches the lowest directory
// that holds them all, so the library also reports files that the job did not name.
// It has no polling mode. With `ignore`: globs for the `ignore` option.

import watcher from '@parcel/watcher';

import { commonDirectory, error, event, note, readJob, ready, unsupported } from './protocol.mjs';

const job = await readJob();
if (job.poll !== null) unsupported('@parcel/watcher has no polling mode');

const directory = commonDirectory(job.paths);
if (directory !== null) {
  if (job.paths.length !== 1 || job.paths[0] !== directory) {
    note(`@parcel/watcher watches directories only: it watches ${directory} for ${job.paths.length} paths`);
  }
  const options = {};
  if (job.ignore !== null) options.ignore = [`**/${job.ignore}`, `**/${job.ignore}/**`];
  try {
    await watcher.subscribe(
      directory,
      (problem, events) => {
        if (problem) error(problem);
        // The library has the same three words: create, update, delete.
        for (const one of events ?? []) event(one.type, one.path);
      },
      options,
    );
  } catch (problem) {
    error(problem);
  }
}
ready();
