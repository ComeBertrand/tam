# Source this file (bash or zsh) to step into tam's demo world:
#
#   . scripts/demo/env.sh
#
# tam, its daemon, its config and data, Claude's files, git's config and
# the projects tam discovers then all live under $TAM_DEMO (default
# /tmp/tam-demo): nothing of yours shows, and your own daemon is not
# touched. Leave by closing the shell.

TAM_DEMO=${TAM_DEMO:-/tmp/tam-demo}
TAM_DEMO_SCRIPTS=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
export TAM_DEMO TAM_DEMO_SCRIPTS

# From inside a tam agent or a Claude session, none of theirs; and git
# acts where it is pointed, with the demo's config.
unset TAM_AGENT_ID TAM_SOCKET GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_CONFIG_GLOBAL
for _v in $(env | sed -n 's/^\(CLAUDE[A-Za-z0-9_]*\)=.*/\1/p'); do unset "$_v"; done
unset _v

export HOME="$TAM_DEMO/home"
export XDG_RUNTIME_DIR="$TAM_DEMO/run"        # the daemon's socket (short: socket paths are)
export XDG_DATA_HOME="$HOME/.local/share"     # the task ledger
export XDG_CONFIG_HOME="$HOME/.config"        # tam's config: no notify command of yours
export XDG_STATE_HOME="$HOME/.local/state"
export XDG_CACHE_HOME="$HOME/.cache"
export CLAUDE_CONFIG_DIR="$HOME/.claude"      # session files: chat titles and history

# The scripted `claude` first, unless TAM_DEMO_REAL_CLAUDE is set.
case ":$PATH:" in
  *":$TAM_DEMO_SCRIPTS/bin:"*) ;;
  *) [ -n "${TAM_DEMO_REAL_CLAUDE:-}" ] || export PATH="$TAM_DEMO_SCRIPTS/bin:$PATH" ;;
esac

if [ -n "${ZSH_VERSION:-}" ]; then
  # shellcheck disable=SC2034 # zsh's prompt
  PROMPT='%1~ $ '
else
  PS1='\W $ '
fi
mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
cd "$HOME" 2>/dev/null || true
