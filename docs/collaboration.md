# Agents working together

The members of a task are peers: each agent knows the others (their names, repositories and worktrees), reads their worktrees, reaches them with `tam msg`, and works from the task's shared brief. You can message any agent too, without attaching.

## Messages

```
tam msg NAME TEXT              Send a message to the member's agent (TEXT or stdin)
tam msg NAME TEXT --wait       ...and print its reply (--timeout SECS, default 90)

tam history NAME               The task's agent runs and messages
tam peers                      The other members of your task, and what their agents do
tam peers --all                ...of every task (--json: machine-readable)
```

An agent gets a message right away: when idle it wakes up, when busy it reads it at its next step (after a pending permission prompt is answered). A member with no running agent gets it when its next agent starts. Sent from inside an agent tam started, a message comes from that agent, which the recipient can answer with `tam msg`; otherwise it comes from you. The reply `--wait` prints is the recipient's next message back, or its response at the end of the turn that got the message. Two agents answering each other are stopped after 6 messages, until you prompt one of them; an agent sends at most 20 messages per 10 minutes.

## What an agent knows

An agent tam starts knows its task. It is told when its session starts (again on resume, `/clear` and compaction): its task, its name, the other members with their repositories and worktrees, and the task's brief and scratch directory (see [Brief](#brief)), so a backend and a frontend session can agree on an API without you relaying. After `tam merge` or `tam split` changed its task, it is told again at its next prompt or tool call. It also has its member id in `$TAM_AGENT_ID`, and `tam peers` for what changed since. From an agent, `tam msg`, `tam run`, `tam stop`, `tam attach` and `tam split` take the short name of another member of its task (`tam msg frontend ...`), unless a task has that name; `tam peers` prints the name that works from where you run it (`TASK/MEMBER` from outside the task).

## Brief

```
tam brief [NAME]               The task's brief and scratch directory
tam brief NAME --set REF       Make REF the brief: a URL, a ticket id, a path
tam brief NAME --local         Make the local file brief.md the brief again
```

The members of a task work from one brief: the reference you give with `tam new --brief REF` or `tam brief --set REF` (a URL, a ticket id, a path; tam neither fetches nor reads it), else the file `brief.md` in the task's directory (`~/.local/share/tam/tasks/NAME/`, or under `$XDG_DATA_HOME/tam` when it is set), which tam starts with a heading per member. Next to it, `scratch/` is a directory the members share for transient notes. An agent is told both when its session starts, and how to write in the brief: under its own heading, and what the members agree on (an API shape, a name) under `Shared`, so that two agents do not overwrite each other's part. A changed brief reaches running agents at their next session start (resume, `/clear`, compaction); `tam peers` shows the current one. A pattern in the configuration can turn a ticket id into its URL and into the agents' start prompt (see [Brief patterns](configuration.md#brief-patterns)). Dropping the task removes its directory: a `brief.md` the members wrote in is kept in `~/.local/share/tam/tasks/.dropped/`, and `tam drop` says where.

## Reading each other's worktrees

An agent can also read the other members' worktrees without asking, but not edit them: tam adds them to the session's directories with a rule denying edits there, which covers Claude's file tools and the file commands it recognises in shell commands, not every program a command may run. A member that joined after the agent started is readable from the agent's next start. The task's directory is open to the agent as its worktree is: edits there follow its permission mode.

## One prompt, several repositories

With several repositories, every agent starts on the same prompt (`-p`, or the pattern's): each one is told its own repository and its peers, and takes its part of the brief. To tell one member more, `tam msg TASK/MEMBER`.
