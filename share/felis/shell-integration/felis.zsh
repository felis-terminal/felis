# OSC 133 prompt marks and OSC 7 working-directory reports for zsh, in
# the subset other terminals read too. Source it from .zshrc after the
# prompt theme: docs/how-to/mark-shell-prompts.md.

[[ -o interactive ]] || return 0
(( ${+_felis_command_running} )) && return 0
typeset -gi _felis_command_running=0

autoload -Uz add-zsh-hook

_felis_preexec() {
  _felis_command_running=1
  print -n '\e]133;C\e\\'
}

_felis_mark_exit() {
  local ret=$?
  # A D after an empty command line would close a command that never ran.
  (( _felis_command_running )) || return 0
  _felis_command_running=0
  print -n "\e]133;D;$ret\e\\"
}

_felis_mark_prompt() {
  # Inside PS1, not printed from this hook: PROMPT_SP can move the prompt
  # down a row after precmd, and a SIGWINCH repaint re-emits only PS1.
  [[ $PS1 == *$'\e]133;A'* ]] || PS1=$'%{\e]133;A\e\\%}'$PS1
  [[ $PS1 == *$'\e]133;B'* ]] || PS1+=$'%{\e]133;B\e\\%}'
}

_felis_report_cwd() {
  local LC_ALL=C c enc=
  for c in ${(s::)PWD}; do
    if [[ $c == [[:alnum:]/._~-] ]]; then
      enc+=$c
    else
      enc+=%${(l:2::0:)$(( [##16] #c ))}
    fi
  done
  print -n "\e]7;file://${HOST}${enc}\e\\"
}

# zsh 5.10 emits these marks itself, but its D carries no exit code and
# its OSC 7 mangles non-ASCII paths, so its own set is switched off. The
# array must exist before ZLE starts; older zsh rejects the name.
eval 'typeset -ga .term.extensions' 2>/dev/null && .term.extensions+=(-integration)

add-zsh-hook preexec _felis_preexec
add-zsh-hook precmd _felis_mark_exit
add-zsh-hook precmd _felis_mark_prompt
add-zsh-hook chpwd _felis_report_cwd
_felis_report_cwd
