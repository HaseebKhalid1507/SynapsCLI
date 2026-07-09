# hello-ext

The minimal reference SynapsCLI extension. It exists to be *the smallest thing
that loads*: one registered tool and one hook, nothing else.

- **Tool:** `hello` — returns a greeting for a given name.
- **Hook:** `before_tool_call` on `bash` — blocks `rm -rf` unless the command
  targets `/tmp`.

Files:

```
hello-ext/
  .synaps-plugin/plugin.json   # manifest: metadata + extension { permissions, hooks }
  main.py                      # the extension process (JSON-RPC over stdio)
  test_hello.py                # standalone harness — no runtime needed
  README.md                    # this file
```

Try it without SynapsCLI:

```bash
cd examples/extensions/hello-ext
python3 test_hello.py
```

Install it:

```bash
cp -r examples/extensions/hello-ext ~/.synaps-cli/plugins/hello-ext
```

Full walkthrough: [`docs/extensions/tutorial.md`](../../../docs/extensions/tutorial.md).
