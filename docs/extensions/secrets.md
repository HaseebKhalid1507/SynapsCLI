# Extension secrets (`secret_env`)

A config entry in a plugin manifest can name an environment variable with
`secret_env`. When the entry has no other value, Synaps reads that variable
and passes its value to the extension in `initialize`. See
[protocol.md](./protocol.md) for the full config resolution order.

Synaps refuses some names. An extension that asks for one **fails to load**,
with an error that names the config key, the variable and the reason. Nothing
is read and nothing is passed to the extension.

## Name rules

`secret_env` must be a plain upper-case environment variable name matching
`[A-Z_][A-Z0-9_]*`. Lower-case names, names with spaces, `=`, `-` or other
characters, and empty names are rejected.

## Host credentials are never handed out

An extension cannot request a credential that Synaps itself uses. The
denylist is built from the provider registries, so every new provider is
covered automatically. It includes:

- every environment variable a built-in provider reads its API key from, for
  example `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GROQ_API_KEY`,
  `OPENROUTER_API_KEY`, `KIMI_API_KEY`, `MOONSHOT_API_KEY`,
  `TOGETHER_API_KEY`, `NVIDIA_API_KEY`, `HF_TOKEN`, `HUGGINGFACE_API_KEY`,
  `DEEPINFRA_TOKEN` and the rest of the OpenAI-compatible provider table;
- `META_API_KEY` (Meta Muse), `LOCAL_API_KEY` (local endpoint) and
  `OPENAI_CODEX_ACCESS_TOKEN`;
- `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` and `AWS_SESSION_TOKEN`;
- the proxy variables `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and
  `NO_PROXY`, because Synaps's HTTP client reads them and a proxy URL can
  carry a username and password.

Third-party secrets that Synaps does not use itself, such as `GITHUB_TOKEN`
or `STRIPE_KEY`, are still allowed.

## The `SYNAPS_*` namespace

Every `SYNAPS_*` variable belongs to the host (`SYNAPS_MACHINE_TOKEN`,
`SYNAPS_BROKER_TOKEN`, cloud login settings and so on) and is refused, with
one exception: an extension may name its **own** override variable for one of
the config keys it declares, `SYNAPS_EXTENSION_<PLUGIN_ID>_<KEY>` (upper case,
`-` folded to `_`). It cannot name an override variable for a key it does not
declare, so extension `foo` cannot read `SYNAPS_EXTENSION_FOO_BAR_TOKEN`,
which belongs to extension `foo-bar`'s `token` key.

Two plugin ids that differ only in `-` versus `_` (`foo-bar` and `foo_bar`)
would share override variables, so the second one to load is refused.

One overlap remains, because of how override names are built: extension
`foo` with a config key `bar-token` and extension `foo-bar` with a key
`token` both read `SYNAPS_EXTENSION_FOO_BAR_TOKEN`. If you set override
variables for one plugin, check that no other installed plugin declares a
key that maps to the same name.

## This is not isolation

An extension runs as your user. A determined extension can read your shell
files or, on Linux, `/proc/<pid>/environ` of its parent. The denylist stops a
manifest from being handed host credentials by declaration, with no prompt.
It does not stop code you chose to install from looking for them.
