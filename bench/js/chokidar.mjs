// chokidar 5, used as its README shows: `chokidar.watch(paths, options)` and the `all` event.
// `ignoreInitial` is on, because the benchmark wants changes and not the list of what exists.
// With `poll`: `usePolling` with `interval` (and `binaryInterval`, so every file has the same
// pause). With `ignore`: a function for the `ignored` option.

import chokidar from 'chokidar';

import { error, event, hasPart, readJob, ready } from './protocol.mjs';

const words = {
  add: 'create',
  change: 'update',
  unlink: 'delete',
  addDir: 'create-dir',
  unlinkDir: 'delete-dir',
};

const job = await readJob();
const options = { ignoreInitial: true };
if (job.ignore !== null) options.ignored = (file) => hasPart(file, job.ignore);
if (job.poll !== null) {
  options.usePolling = true;
  options.interval = job.poll;
  options.binaryInterval = job.poll;
}

const watcher = chokidar.watch(job.paths, options);
watcher.on('all', (kind, file) => event(words[kind] ?? kind, file));
// A full table of open files gives one error for each path: thousands of equal lines.
let errors = 0;
watcher.on('error', (problem) => {
  errors++;
  if (errors <= 3) error(problem);
  if (errors === 4) error('more errors follow; only the first three are printed');
});
watcher.on('ready', ready);
// With no path at all chokidar never says `ready`.
if (job.paths.length === 0) ready();
