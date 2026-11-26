# Shells

A **shell** is your `$SHELL`, run by the daemon in a task's worktree or on a repository's main checkout: `tam shell`, or `!` in the [dashboard](dashboard.md#shells). It keeps running once you detach, and its screen shows in the dashboard's pane like an agent's.

## Commands

```
tam shell [NAME]               Open a shell in the task's worktree (default: where you are) and attach
tam shell -r REPO              ... on the repository's main checkout instead
tam shell -d                   Don't attach: print its id (--json: print a receipt)

tam attach SHELL               Attach to the shell
tam stop SHELL                 Close it: what runs in it stops

SHELL is a shell's id (sh-…), as tam ps shows it.
```

## How a shell runs

A shell is your `$SHELL`, run by the daemon in a task's worktree or on a repository's main checkout: it keeps running once you detach (`ctrl-]`), until it exits (`exit`, `ctrl-d`) or `tam stop` closes it, and dropping its task (or its member) closes it too, as `tam drop` says. It runs in the environment you ran `tam shell` from, through `[spawn] wrapper` as an agent does (see [The dev environment](claude-code.md#the-dev-environment-spawn-wrapper)), without `TAM_AGENT_ID`: what you type there is yours, `tam msg` from it comes from you. A daemon restart closes every shell. Shells are named `Shell #1`, `Shell #2`… in each place (the number after the highest one open there), and by their id on the command line.

## Where a shell opens

NAME is what `tam run` takes (a task, `TASK/MEMBER`, a member id), or a chat (its main checkout); REPO what `tam chat` takes (a path, or a project name as `tam ls` shows it; from a worktree, its main checkout). With neither, the shell opens in the task worktree you are in, else on the main checkout you are in; in a worktree no task has, `tam shell` refuses (`tam adopt` makes it a task's, `tam shell -r .` opens one on its main checkout). Without a terminal, or with `-d`, it opens the shell and prints its id alone on stdout (how to attach goes to stderr). `tam ps` lists the shells on main checkouts in a table of their own after the chats, and a task's shells below its row (below its member's, in a task with several), each with its id. The commands that act on agents or tasks (`run`, `msg`, `wait`, `result`, `history`, `drop`, `merge`, `close`) say when a name is a shell's id. The [dashboard](dashboard.md#shells) has them too.
