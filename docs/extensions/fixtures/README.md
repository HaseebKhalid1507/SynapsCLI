# Fixtures for the extension docs

`context-archives/` is one project directory of context archive files,
exactly as Synaps wrote it. [`../context-archives.md`](../context-archives.md)
describes the format and walks through these files.

They are real files, not hand-written examples, and the tests in
`crates/agent-core/tests/context_archive_format.rs` hold the store, the
document and the reference reader to them. Do not edit them: they are format
version 1. That test file says where each one came from and how to print
them again.
