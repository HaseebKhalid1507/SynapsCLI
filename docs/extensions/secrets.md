# Extension secrets (`secret_env`)

A config entry in a plugin manifest can name an environment variable with
`secret_env`. When the entry has no other value, Synaps reads that variable
and passes its value to the extension in `initialize`. See
[protocol.md](./protocol.md) for the full config resolution order.

Synaps refuses some names. An extension that asks for a host credential or a
`SYNAPS_*` variable **fails to load**, with an error that names the config
key, the variable and the reason. Nothing is read and nothing is passed to the
extension.

## Name rules

`secret_env` must be a plain upper-case environment variable name matching
`[A-Z_][A-Z0-9_]*`, after surrounding whitespace is trimmed. Any other name
(lower case, inner spaces, `=`, `-`, non-ASCII, empty) is **ignored with a
warning**: the plugin still loads, the variable is never read, and the value
resolves from plugin config or `default` as if no `secret_env` were declared.
A `required` entry with no other value then fails with the usual "missing
required config" error.

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

Two plugin ids that differ only in `-` versus `_` or in letter case
(`foo-bar`, `foo_bar`, `Foo-Bar`) would share override variables, so only
one of them is loaded and the others are refused:

- At startup, plugins load in order of their directory names (byte order),
  so the one whose directory name sorts first wins (`Foo-Bar` before
  `foo-bar` before `foo_bar`). If it fails to load for another reason, the
  next one in that order loads instead.
- An install or reload during a session is checked against what is loaded
  at that moment. If you install `foo-bar` while `foo_bar` is loaded,
  `foo-bar` is refused now, but after a restart the startup order applies
  and `foo-bar` wins while `foo_bar` is refused.

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
