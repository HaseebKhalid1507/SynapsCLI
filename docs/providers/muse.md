# Meta Muse

**Status: experimental.** Synaps talks to Meta Muse the way the official Muse Code
CLI does. Meta doesn't publish these endpoints as a stable third-party API, so they
can change without notice.

Muse models are `muse/<id>` (for example `muse/muse-wonder-1.0`). Requests go over
the OpenAI Responses wire to `https://api.meta.ai/v1/responses`, always through the
credential broker.

## Log in

```bash
synaps login --provider muse          # aliases: muse-code, meta, meta-ai, meta-muse
```

Or run `synaps login` and pick **Sign in with Meta Muse**.

This is a device flow (RFC 8628). Synaps prints a code and a URL on `auth.meta.com`
and opens the browser if it can. Approve the code there and the login finishes by
itself. You can do the approval on another machine.

After approval, Synaps exchanges the Meta login for a **Model API key** once and
stores both in `~/.synaps-cli/auth.json` (mode 0600):

- the Meta OAuth token, which is used only to mint a new key;
- the minted Model API key, which is the bearer for inference.

The key has no expiry. Synaps uses it until Meta answers 401, then mints a new one
**once** and retries the request once. If the mint itself is rejected, the login is
dead: run `synaps login --provider muse` again.

Login errors you might see:

| Message | What to do |
|---|---|
| "has not finished Model API onboarding" | Finish onboarding at <https://dev.meta.ai>, then log in again. |
| "needs an active Model API plan or payment method" | Set up a plan or payment method at <https://dev.meta.ai>, then retry. Your login is still good (from #249; before it, the message says to log in again). |
| "saved Meta login is no longer valid" | Log in again. |

Named accounts work the same way as for the other OAuth providers
(`--account <label>`, `auth.account.muse`). See
[multi-account-broker.md](multi-account-broker.md).

## Use a Model API key instead

`META_API_KEY` works like it does for the official CLI: a key created at
<https://dev.meta.ai> takes precedence over the account login.

- It applies to requests that use the account policy (the usual case). A request
  that names an account explicitly still uses that account.
- An environment key is never re-minted. If Meta rejects it, the error is
  returned as-is.
- With a daemon, the key is read from the **daemon's** environment. Restart or
  reload the daemon after you set or change it.

## Models

```bash
synaps --model muse/muse-wonder-1.0
```

Or pick one with `/model`. The catalog comes from Meta (`GET /muse-code/models`)
when you're logged in. Only `muse-wonder-1.0` is built in, because it's the only id
Meta documents publicly. The picker shows whatever else your account can see.

Any well-formed id (lowercase letters, digits, `.`, `-`, `_`) routes to Meta, and
Meta decides whether it exists. A typo fails on the first turn with Meta's error.
Some stricter paths, such as authorising a subagent's model, require an id from
the built-in list or the live catalog.

Daemon-hosted sessions can switch to Muse like any other provider.

## Effort levels

`/effort` (or `thinking = …` in config) offers only the levels the live catalog
advertises for that exact model. Synaps maps them to Muse's effort names:

| Synaps level | Sent to Muse |
|---|---|
| `adaptive` | nothing (Meta's default for the model) |
| `off` | `none` |
| `low`, `medium`, `high` | same |
| `xhigh`, `max`, `ultra` | same |

If the catalog hasn't loaded, or the model advertises no levels, only `adaptive`
is available and Synaps sends no effort field. When you switch to a Muse model
without having picked a level, Synaps uses the catalog's default effort for that
model, or `adaptive` if it names none.

## How the key is handled

Muse is **broker-proxy only**:

- The runtime never holds the Model API key or the Meta OAuth token. It sends the
  request to the broker, and the broker attaches the key.
- The broker pins the origin to `https://api.meta.ai` and allows only
  `POST /v1/responses` and `GET /muse-code/models`.
- The Meta OAuth token never leaves the broker and is never sent to the
  inference host. It is used only to mint a key.
- The OAuth refresh token isn't stored, because the official client never uses
  it.

With a remote broker (`synaps auth-broker`), Muse goes through the broker's
`/proxy`. From the consent-latch PR (`fix/consent-latch-broker-exposure`) onward,
the broker's `/token` endpoint refuses to hand out the Muse key at all. `/proxy` is
unchanged.
