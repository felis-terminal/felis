# Install the xterm-felis terminfo entry on hosts reached by an
# interactive ssh: docs/how-to/fix-terminfo-problems.md.

_felis_ssh_script=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/ssh.sh
ssh() { command sh -- "$_felis_ssh_script" "$@"; }
