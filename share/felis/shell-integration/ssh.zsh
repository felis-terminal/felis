# Install the xterm-felis terminfo entry on hosts reached by an
# interactive ssh: docs/how-to/fix-terminfo-problems.md.

typeset -g _felis_ssh_script=${${(%):-%x}:A:h}/ssh.sh
ssh() { command sh -- "$_felis_ssh_script" "$@"; }
