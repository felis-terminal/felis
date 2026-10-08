---
title: Report a bug
sidebar:
  order: 11
---

Give a maintainer what they need to reproduce your problem on their own machine: which felis builds you run, on which
system, with which fonts and settings.

## Collect the environment

From the felis window where the problem happens, run:

```sh
felis doctor report
```

Running it there matters: the report names the terminal identity (`TERM`, `TERM_PROGRAM`) of the shell it runs in, and a
shell outside felis says nothing about the affected session. For a problem with a remote daemon, add the same carrier
flag you use for it (`felis --host devbox doctor report`); the daemon row then describes that daemon, and every other
line still describes this machine.

The output is Markdown. Read it before posting. Paths under your home directory print as `~`, and the arguments of
`send_string`, `run`, and `pipe` keybindings print as `<redacted>`, but every other config value you set is included,
because those values are what reproduce a rendering or input bug. The fields and redactions are listed in
[cli.md](../reference/cli.md#doctor-report).

## Open the issue

Open an issue on [GitHub](https://github.com/felis-terminal/felis/issues/new?template=bug_report.md), paste the report
where the template asks for it, and add:

- the steps that trigger the problem, starting from a fresh window if you can;
- for a rendering problem, a screenshot, and the program or the `printf` that produced the output;
- for a crash or a window that closes, the end of the log file the report names (`daemon.log` or `client.log`). Skim it
  first: log lines can carry window titles and paths.
