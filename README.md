# pathwatch

`pathwatch` watches paths and tells you which files were created, updated or deleted. A path
can be a file, a directory with everything below it, or a path that does not exist yet.

```rust
use std::sync::mpsc;

use pathwatch::Watcher;

fn main() -> Result<(), pathwatch::Error> {
  let (sender, batches) = mpsc::channel();
  let watcher = Watcher::new(move |result| {
    let _ = sender.send(result);
  })?;
  watcher.watch(&["/project/src", "/project/package.json"])?;

  for result in batches {
    match result {
      Ok(events) => {
        for event in events {
          println!("{:?} {}", event.kind, event.path.display());
        }
      }
      Err(error) => eprintln!("{error}"),
    }
  }
  Ok(())
}
```

To try it on your own files: `cargo run -p pathwatch --example watch -- src package.json`.

## Events

Only files are reported, never a directory. A batch has at most one event for a path.

| Event | Meaning |
|---|---|
| `Create` | A file is there that you were not told about. |
| `Update` | The file may have other content. |
| `Delete` | The file that you were told about is gone. |

## Options

| `Config` method | Default |
|---|---|
| `with_ignored(f)` | nothing is ignored |
| `with_debounce(d)` | none on macOS, 10 ms elsewhere |
| `with_delete_delay(d)` | 10 ms; none under polling |
| `with_polling(bool)` | `false` |
| `with_poll_interval(d)` | 100 ms |

## Platforms

| OS | Watch API |
|---|---|
| Linux, Android | inotify |
| macOS | FSEvents, with kqueue for the directories on the way |
| Windows | `ReadDirectoryChangesW` |
| every other target | none: always polling |

The promises, the known holes and the timing are in
[`references/behavior.md`](references/behavior.md); the rules it follows are in
[`references/design.md`](references/design.md), and how the code follows them is in
[`references/architecture.md`](references/architecture.md).

## License

MIT
