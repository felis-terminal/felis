# Install the xterm-felis terminfo entry on hosts reached by an
# interactive ssh: docs/how-to/fix-terminfo-problems.md.

set -g _felis_ssh_script (path resolve (status dirname))/ssh.sh
function ssh --wraps ssh
    command sh -- $_felis_ssh_script $argv
end
