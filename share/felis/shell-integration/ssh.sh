# Run ssh, first installing the xterm-felis terminfo entry on the remote
# when this is an interactive login from a felis session. The ssh.bash,
# ssh.zsh and ssh.fish wrappers run this file with sh:
# docs/how-to/fix-terminfo-problems.md.

[ "${TERM-}" = xterm-felis ] || exec ssh "$@"

# One line inside one single-quoted sh -c argument, so that a fish, csh
# or nushell login shell hands it to sh unchanged.
script='sh -c '\''command -v tic >/dev/null 2>&1 || { infocmp xterm-felis >/dev/null 2>&1 && exit 0; exit 3; }; mkdir -p "$HOME/.terminfo" && tic -x -o "$HOME/.terminfo" - || exit 4; command -v infocmp >/dev/null 2>&1 || exit 0; infocmp xterm-felis >/dev/null 2>&1 || exit 5'\'

# ssh's own option letters (ssh.c getopt); ssh also reads options after
# the destination, and the first other word starts a remote command.
dest=
skip_next=0
options_done=0
for arg in "$@"; do
	if [ "$skip_next" = 1 ]; then
		skip_next=0
		continue
	fi
	case $options_done$arg in
	0--)
		options_done=1
		continue
		;;
	0-?*)
		rest=${arg#-}
		while [ -n "$rest" ]; do
			opt=${rest%"${rest#?}"}
			rest=${rest#?}
			case $opt in
			[1246ACKXYZagkqvxy]) ;;
			[BDEFIJLPRbceilmop])
				[ -n "$rest" ] || skip_next=1
				rest=
				;;
			*) exec ssh "$@" ;;
			esac
		done
		continue
		;;
	esac
	[ -z "$dest" ] || exec ssh "$@"
	dest=$arg
done
[ -n "$dest" ] || exec ssh "$@"

probe_options='-T -o RequestTTY=no -o RemoteCommand=none -o ClearAllForwardings=yes'
login=$(ssh -G "$@" 2>/dev/null) || exec ssh "$@"
# shellcheck disable=SC2086
probe=$(ssh -G $probe_options "$@" "$script" 2>/dev/null) || exec ssh "$@"

if printf '%s\n' "$login" | awk '
	$1 == "remotecommand" { bad = 1 }
	$1 == "requesttty" && $2 != "auto" { bad = 1 }
	$1 == "sessiontype" && $2 != "default" { bad = 1 }
	$1 == "stdinnull" && $2 != "no" { bad = 1 }
	$1 == "forkafterauthentication" && $2 != "no" { bad = 1 }
	$1 == "tunnel" && $2 != "false" { bad = 1 }
	$1 == "controlmaster" && $2 != "false" && $2 != "auto" { bad = 1 }
	$1 == "permitlocalcommand" && $2 != "no" { bad = 1 }
	$1 == "setenv" && $2 ~ /^TERM=/ { bad = 1 }
	END { exit !bad }'; then
	exec ssh "$@"
fi

# The probe carries a command, so `Match command` and `Match sessiontype`
# can send it somewhere other than the login.
unprobed='$1 !~ /^(requesttty|remotecommand|clearallforwardings|localforward|remoteforward|dynamicforward)$/'
if [ "$(printf '%s\n' "$login" | awk "$unprobed")" != "$(printf '%s\n' "$probe" | awk "$unprobed")" ]; then
	exec ssh "$@"
fi

if entry=$(infocmp -x xterm-felis 2>/dev/null) && [ -n "$entry" ]; then
	# shellcheck disable=SC2086
	printf '%s\n' "$entry" | ssh $probe_options "$@" "$script"
	case $? in
	0) exec ssh "$@" ;;
	# ssh's own failure: a retry would wait out a second connect timeout.
	255) exit 255 ;;
	esac
fi

printf 'felis: xterm-felis is not available on %s; using TERM=xterm-256color\n' "$dest" >&2
TERM=xterm-256color exec ssh "$@"
