// Node with no watcher at all: what the runtime costs by itself.

import { readJob, ready } from './protocol.mjs';

await readJob();
ready();
