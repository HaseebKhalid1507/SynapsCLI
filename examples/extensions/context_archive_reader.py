#!/usr/bin/env python3
"""Reference reader for Synaps context archives, format version 1.

    context_archive_reader.py <project directory>

Lists every archive in one project directory and passes over everything
else, as docs/extensions/context-archives.md tells a reader to. It only
reads: it never writes, locks, renames or deletes. Standard library only.
"""
import json
import os
import re
import stat
import sys

SHELF = re.compile(r"s([0-9]{6}|[1-9][0-9]{6,})")
ARCHIVE = re.compile(r"[0-9a-f]{32}\.json")
SLOT = re.compile(r"(0[0-9][0-9]|1[01][0-9]|12[0-7])\.json")
MAX_BYTES = 16 * 1024 * 1024


def listing(directory):
    """The names in a directory, sorted. A link, a file or nothing has none."""
    try:
        if stat.S_ISDIR(os.lstat(directory).st_mode):
            return sorted(os.listdir(directory))
    except OSError:
        pass
    return []


def read(path, scope, name):
    """The version 1 record in the file `path`. Raises when it is not one."""
    info = os.lstat(path)
    if not stat.S_ISREG(info.st_mode) or info.st_size > MAX_BYTES:
        raise ValueError("not a regular file of at most 16 MiB")
    with open(path, "rb") as file:  # one read, then closed
        record = json.loads(file.read().decode("utf-8"))
    if not isinstance(record, dict) or type(record.get("version")) is not int:
        raise ValueError("not a record")
    if record["version"] != 1:
        raise ValueError("version %d is newer than this reader" % record["version"])
    if record["scope"] != scope:
        raise ValueError("a record of another project")
    file_name = "%s.json" % record["id"]
    if not ARCHIVE.fullmatch(file_name) or name not in (None, file_name):
        raise ValueError("the id is not 32 hex characters, or not the file's name")
    if record["body"]["state"] not in ("live", "tombstone"):
        raise ValueError("unknown state")
    return record


def describe(record, relative):
    body = record["body"]
    if body["state"] == "tombstone":
        return "tombstone", record["id"], relative
    messages = body["messages"]
    reduced = sum("_archive_reduced" in entry["message"] for entry in messages)
    return "live", record["id"], relative, len(messages), body["source_message_count"], reduced


def main(project):
    project = os.path.realpath(project)
    scope = os.path.basename(project)
    # Legacy slot files, named by number; then the shelves, each file named by its id.
    files = [(name, None) for name in listing(project) if SLOT.fullmatch(name)]
    shelves = [name for name in listing(project) if SHELF.fullmatch(name)]
    for shelf in sorted(shelves, key=lambda name: int(name[1:])):
        names = listing(os.path.join(project, shelf))
        files += [(shelf + "/" + name, name) for name in names if ARCHIVE.fullmatch(name)]
    for relative, name in files:
        path = os.path.join(project, *relative.split("/"))
        try:
            print(*describe(read(path, scope, name), relative), sep="\t")
        except (OSError, ValueError, KeyError, TypeError) as error:
            reason = "%s: %s" % (type(error).__name__, error)
            print("skipped", relative, reason, sep="\t", file=sys.stderr)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
