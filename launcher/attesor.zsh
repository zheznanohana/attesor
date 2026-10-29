# Attesor shell hook (sourced from ~/.zshrc): lets this shell and everything started from it run arm64-only
# programs directly, e.g. ./tool, the way binfmt_misc does on Linux. It loads the x86-64 exec redirection
# library into the interactive shell itself by restarting the shell once with it in DYLD_INSERT_LIBRARIES.
# Off switch: ATTESOR_NO_SHELL_HOOK=1 (or remove the `source` line from ~/.zshrc).
# Only real terminal sessions: restarting would drop the arguments of `zsh -i -c …` style invocations.
if [[ -o interactive && -t 0 && -t 1 && -z ${ZSH_EXECUTION_STRING:-} \
      && -z ${ATTESOR_SHELL_HOOK:-} && -z ${ATTESOR_NO_SHELL_HOOK:-} \
      && -r $HOME/bin/gpu/libattesorexec.dylib && -x $HOME/bin/attesor ]]; then
  export ATTESOR_SHELL_HOOK=1 ATTESOR_BIN=$HOME/bin/attesor
  export DYLD_INSERT_LIBRARIES=${DYLD_INSERT_LIBRARIES:+$DYLD_INSERT_LIBRARIES:}$HOME/bin/gpu/libattesorexec.dylib
  if [[ -o login ]]; then exec /bin/zsh -l; else exec /bin/zsh; fi
fi
