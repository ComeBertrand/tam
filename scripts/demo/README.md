# A demo world for screenshots

Your real tam shows your real work. These scripts build a separate one,
made up from scratch, to take screenshots and recordings of tam with:
three small repositories (`api`, `web`, `infra`), tasks in every status,
a task spanning two repositories, chats and shells.

It runs beside your own tam without touching it: everything lives under
`$TAM_DEMO` (default `/tmp/tam-demo`), with its own daemon, task ledger,
config, Claude files, git identity and `$HOME`, so the project picker
finds the demo repositories only.

## Use

```bash
scripts/demo/setup.sh            # build it (a few seconds)
. scripts/demo/env.sh && tam     # step into it and open the dashboard
scripts/demo/teardown.sh         # stop its daemon and delete it all
```

`setup.sh` uses the `tam` on your `PATH`. To show a build of your own, put
it first: `PATH=$PWD/target/release:$PATH scripts/demo/setup.sh`, and the
same when you source `env.sh`.

From 136 columns the pane sits beside the list. About 160x45 shows
everything at once.

## The agents

The agents are not Claude. `bin/claude` is a script that `env.sh` puts
first on the `PATH`. tam starts it as it would start Claude Code. It
draws a made-up conversation and plays the hooks Claude would fire
(through `tam hook-notify`), so each agent shows the status its scene
asks for: working, ready, blocked on a permission, asking a question, or
failed. It also writes a session file where Claude would, for the context
use (CTX) and the chats' titles. The scene comes from `DEMO_SCENE` in the
environment of the `tam` command that started it. Anything typed into
one gets a canned answer. Answering its permission or question with `a`
in the dashboard plays the rest of the turn.

A few things look old because `setup.sh` dates their ledger lines back:
the stale and idle tasks, the gone one, and the chat in its last day.

## The real Claude in the pane

```bash
scripts/demo/setup.sh --real-claude
```

The first chat (the dashboard opens on it) then runs the real Claude Code
on the demo `api` repository, with a short question about its rate
limiter. It costs one short turn. Your `~/.claude/.credentials.json` is
copied into the demo, which `teardown.sh` deletes. On macOS, Claude keeps
its credentials in the Keychain instead: log in once with `tam attach`
on that chat.

What Claude shows is what your account gets. With an organization's
account, its start screen says that your organization manages its
settings, and your organization's plugins and skills come along.

## Recording

The README's screenshot (`assets/dashboard.png`) was taken by hand in the
demo world, in the `unicorn` theme (`T` in the dashboard switches theme).

[vhs](https://github.com/charmbracelet/vhs) replays `dashboard.tape`
after `setup.sh`. It writes `dashboard.png` and `dashboard.gif` next to
it, never over the README's screenshot:

```bash
scripts/demo/setup.sh && vhs scripts/demo/dashboard.tape
```
