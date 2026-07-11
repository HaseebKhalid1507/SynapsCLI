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

It also declares one theme token in its manifest (`theme_tokens`):
`accent = #22d3ee`. At load, SynapsCLI merges it into the active theme as
`ext.hello-ext.accent`; a user theme-file line `ext.hello-ext.accent = "#ff00ff"`
overrides it. The field is optional — extensions without it are unaffected
(see `docs/extensions/contract.json` → `theme_tokens`).

Install it:

```bash
cp -r examples/extensions/hello-ext ~/.synaps-cli/plugins/hello-ext
```

Full walkthrough: [`docs/extensions/tutorial.md`](../../../docs/extensions/tutorial.md).
