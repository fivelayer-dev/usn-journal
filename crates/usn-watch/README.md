# usn-watch

Reads the Windows USN change journal and writes JSON Lines on stdout, so that a program
in any language can use it. Each line is one JSON object, and diagnostics go to stderr.

```text
usn-watch info     <volume> [--unprivileged]
usn-watch health   <volume> [--cursor CURSOR] [--required-hours N] [--unprivileged]
usn-watch position <volume> [--unprivileged]
usn-watch changes  <volume> --resume CURSOR [--until CURSOR] [filters] [--unprivileged]
usn-watch scan     <volume> [filters]
usn-watch watch    <volume> [--resume CURSOR | --oldest] [--settle-ms N]
                   [--max-open-s N] [--poll-ms N] [filters] [--unprivileged]
```

Filters are `--include PATH`, `--exclude PATH`, `--exclude-system-files` and
`--no-directories`. A volume is a drive such as `C:`, a GUID path such as
`\\?\Volume{GUID}\`, or a device path such as that of a volume shadow copy.

Reading a journal with file names needs an elevated process. `--unprivileged` doesn't,
but its changes have no paths and are told apart by `id`. `scan` always needs elevation.

## Output

Every line has `"v"`, the format version, and `"type"`, one of these:

| `type` | Meaning | What to do |
|---|---|---|
| `change` | A file or directory changed | Act on `kind` |
| `gap` | Changes were lost | Run `scan` again |
| `checkpoint` | A safe position to resume from | Save `cursor` once the lines before it are handled |
| `error` | A failure. `retrying` says whether it will be retried | Log it |
| `file` | One entry from `scan` | Look at it |
| `journal` | The journal's state, from `info` | |
| `health` | How much history the journal holds, from `health` | Act on `issues` |

A `change` line:

```json
{"v":1,"type":"change","kind":"modified","path":"C:\\docs\\a.txt","previous_path":null,
 "is_directory":false,"has_new_content":true,"is_reparse_point":false,"is_link":false,
 "reparse_tag":null,"still_open":false,"id":"0x1000000abcd","parent_id":"0x100000000ef",
 "first_usn":1024,"last_usn":1104,"timestamp_ms":1577836800000,"attributes":32,
 "reasons":["DATA_EXTEND","CLOSE"]}
```

`kind` is `created`, `modified`, `renamed`, `metadata_changed` or `deleted`, and the
[guide](https://github.com/fivelayer-dev/usn-journal/blob/main/docs/tracking-changes.md)
says what to do about each. An unpaired surrogate in a file name is written as a `\uXXXX`
escape, so you get back the exact name needed to open the file.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | A failure, such as access denied or no journal |
| 2 | A usage error |
| 3 | `health` found a problem |

To stop `watch`, close its stdout pipe or end the process.

## Example

Following a volume from Python:

```python
import json
import subprocess

def watch(volume, cursor):
    process = subprocess.Popen(
        ["usn-watch.exe", "watch", volume, "--resume", cursor],
        stdout=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    for line in process.stdout:
        event = json.loads(line)
        if event["type"] == "change":
            if event["has_new_content"]:
                read_file(event["path"])
        elif event["type"] == "gap":
            rescan(volume)
        elif event["type"] == "checkpoint":
            save_cursor(event["cursor"])
        elif event["type"] == "error":
            log(event["message"])
```

A `checkpoint` follows the changes it covers, so save it only once they are handled.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
