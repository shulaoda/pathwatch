// watchpack 2, the watcher of webpack 5, used as its README shows: `new Watchpack(options)`,
// `watch({ files, directories, missing })`, and the `change` and `remove` events. A path of
// the job that is a directory goes to `directories`, a file to `files`, and one that does not
// exist to `missing`. With `poll`: the `poll` option with the pause. With `ignore`: a function
// for `ignored`. `aggregateTimeout` is left out: the single events are what is measured. The
// events name a directory like a file, so a new directory counts as an event too.

import fs from 'node:fs';

import Watchpack from 'watchpack';

import { error, event, hasPart, readJob, ready } from './protocol.mjs';

const job = await readJob();
const options = {};
if (job.ignore !== null) options.ignored = (file) => hasPart(file, job.ignore);
if (job.poll !== null) options.poll = job.poll;

const files = [];
const directories = [];
const missing = [];
for (const one of job.paths) {
  const stat = fs.statSync(one, { throwIfNoEntry: false });
  if (stat === undefined) missing.push(one);
  else if (stat.isDirectory()) directories.push(one);
  else files.push(one);
}

const wp = new Watchpack(options);
wp.on('change', (file) => event('change', file));
wp.on('remove', (file) => event('delete', file));
wp.on('error', (problem) => error(problem));
wp.watch({ files, directories, missing, startTime: Date.now() });
ready();
