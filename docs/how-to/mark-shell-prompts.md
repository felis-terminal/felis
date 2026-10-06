---
title: Mark shell prompts
sidebar:
  order: 4
---

Make your shell report its prompts, command boundaries, exit codes and working directory to felis. The shell does this
with `OSC 133` prompt marks and `OSC 7` directory reports; felis never guesses where a prompt is. Without them,
`felis sessions send --wait` never returns, `last_exit_code` stays empty, `scroll_to_prompt` does nothing,
`capture --source command-output` finds no range, and a resize can leave copies of a full-width prompt line behind.

## Check what your shell already sends

In a felis window running the shell, run from any other terminal:

```sh
felis sessions send <id> --wait --timeout 5 false --key enter
```

- `1`: the shell marks commands with their exit codes. Nothing to do.
- `-`: the shell marks commands but sends no exit code. zsh 5.10's own marks are like this; use the script below.
- `wait: no command completed within 5s`: the shell sends no marks. For zsh, use the script below.

## Source the zsh script

felis ships `share/felis/shell-integration/felis.zsh` in its install prefix. Add this to `~/.zshrc`, after the line that
loads your prompt theme:

```zsh
if (( $+commands[felis] )); then
  source ${commands[felis]:A:h:h}/share/felis/shell-integration/felis.zsh
fi
```

`${commands[felis]:A:h:h}` resolves the `felis` on your `PATH` to its install prefix, which finds the file for the Nix
package and the release archives. From a source checkout, source `share/felis/shell-integration/felis.zsh` in the
checkout instead.

With the home-manager module, set `programs.felis.enableZshIntegration = true;` instead. It adds the `source` line after
the generated `.zshrc`'s default-order setup, where home-manager's prompt-theme modules load; a theme you initialize at
a later `lib.mkOrder` can drop the marks, so load it earlier.

The script adds its marks to `PS1` before every prompt, so a theme that rebuilds `PS1` each time keeps them as long as
the theme's hook runs first, which loading the theme earlier in `~/.zshrc` arranges. On zsh 5.10 it switches off zsh's
own marks and directory reports and sends its own, since those carry no exit code and garble non-ASCII paths. The
sequences are the subset other terminals read too, so the same `~/.zshrc` works outside felis. A terminal that injects
its own zsh integration, such as kitty or Ghostty, then receives each command mark twice, which marks the same place
twice and changes nothing.

## Verify

Start a new zsh (a new felis window, or `exec zsh` in the checked session) and repeat the check:

```sh
felis sessions send <id> --wait --timeout 5 false --key enter   # prints 1
felis sessions info <id>                                        # shows cwd and the last exit code
```

## Keep a prompt that the shell does not repaint

Once the shell has marked a command start (`C`), a resize while the cursor sits at a marked prompt blanks the prompt and
leaves the repaint to the shell, which zsh and fish do on `SIGWINCH`. A new shell's first prompt, before any command
ran, is re-wrapped instead. A prompt that is not repainted would vanish until the next one, so a shell or theme that
does not repaint sends `OSC 133 ; A ; redraw=0` instead of `OSC 133 ; A`
([vt-compliance.md](../reference/protocols/vt-compliance.md#osc) "OSC").
