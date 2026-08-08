# GitHub

openpages has no GitHub App and no OAuth client. Every call it makes goes
through the official [`gh` CLI](https://cli.github.com) that is already
installed on the server.

## Why

The alternative is the standard one, and it is four steps of setup before
anybody can deploy their first page: register a GitHub App, generate a private
key, copy it onto the server, configure a callback URL that has to match the
hostname you have not finished setting up yet. For a tool whose entire pitch is
"two commands", that is the wrong trade.

`gh` has already solved authentication. Its token is stored, scoped and
refreshed by someone else, `gh auth login` is a flow most people have already
done, and it is packaged everywhere. openpages shells out to it.

## Setting it up

```bash
openpages login          # wraps `gh auth login` with the right scopes
```

Everything after that reads the token with `gh auth token`, per operation. The
token is never written to the openpages database. It is passed to a build only
as part of a single `git clone` URL, and every log line is redacted against it
before being stored or shown.

To switch accounts:

```bash
gh auth logout && openpages login
```

`openpages doctor` reports which account is in use.

## Signing in to the panel

This is the honest consequence of having no OAuth app: **there is no "Sign in
with GitHub" button**, because a browser redirect flow requires a registered
client. Instead:

```bash
openpages invite          # prints a one-time code, valid 15 minutes
```

Being able to run a command on the server *is* the credential. The session it
creates lasts 30 days and is identified with whatever GitHub account `gh` is
logged in as.

For one operator this is fine, and arguably better than an OAuth round trip.
For a team it is wrong — everybody would share one identity. If you need real
multi-user access, a GitHub App is the right answer, and `src/gh.rs` is where
it goes: every call is already funnelled through one `run()` method.

## Webhooks, or polling

If `server.public_url` is set and GitHub can reach it, openpages installs a
push webhook on each repository when you add it. Deliveries are verified with
HMAC-SHA256 against a secret generated on first run and kept in the database.
Re-adding a site reuses an existing hook with the same URL rather than stacking
up a sixth one.

If there is no public URL — the common self-hosted case — openpages polls each
repository's branch head instead. Slower, and needs no inbound access at all.
Nothing else changes; the deployment path is identical.

Turn webhooks off entirely with:

```toml
[github]
webhooks = false
```

The **first** poll of a newly added site records where the branch is without
deploying it, so adding a site that is already live does not immediately
rebuild it.

## Scopes

`openpages login` asks for `repo` and `read:org`. `repo` covers private clones
and webhook administration. Nothing asks for `delete_repo`, `admin:org`, or
anything that could remove code.

## What it does with them

| Operation | Command |
|---|---|
| Who am I | `gh api user` |
| List repositories | `gh repo list --source --json …` |
| Default branch, branch head | `gh api repos/{repo}` |
| Add / remove a push webhook | `gh api repos/{repo}/hooks` |
| Clone for a build | `git clone` with a token in the URL |
| Open a repair pull request | `gh pr create` |
| Check for an openpages release | `gh api repos/{repo}/releases/latest` |
