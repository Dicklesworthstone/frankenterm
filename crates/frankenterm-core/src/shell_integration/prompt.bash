# Emit one OSC 133 command boundary per interactive submission, including compound commands.
# Prompt code running after this block sees PIPESTATUS from ft's hook; read
# __ft_pipestatus for the last command's pipeline statuses instead (with
# bash-preexec loaded, its BP_PIPESTATUS).
if [[ $- == *i* ]]; then
    __ft_precmd() {
        # PROMPT_COMMAND runs this hook first, so it alone still sees the
        # command's statuses: every command resets PIPESTATUS, and no hook can
        # restore it. One command keeps both values.
        __ft_pipestatus=("${PIPESTATUS[@]}") __ft_last_exit=$?
        local last_exit=$__ft_last_exit
        unset __ft_last_exit __ft_armed
        # On the DEBUG fallback the trap runs just before this hook, and
        # Bash 3.2 does not restore PIPESTATUS after it, so it saved them.
        if [[ -n ${__ft_trap_pipestatus+set} ]]; then
            __ft_pipestatus=("${__ft_trap_pipestatus[@]}")
            unset __ft_trap_pipestatus
        fi
        if [[ -n ${__ft_running_marker:-} ]]; then
            printf '\033]133;D;%d\033\\' "$last_exit"
        fi
        __ft_running_marker=''
        __ft_current_dir
        return "$last_exit"
    }

    __ft_finish_prompt() {
        local last_exit=$?
        # Readline must see both markers as zero-width. Wrap the final PS1
        # after themes and the user's PROMPT_COMMAND have rebuilt it.
        local start='\[\e]133;A\e\\\]' end='\[\e]133;B\e\\\]' arm='${__ft_armed=}'
        local prompt=${PS1:-}
        prompt=${prompt//"$start"/}
        prompt=${prompt//"$end"/}
        prompt=${prompt//"$arm"/}
        if [[ ${__ft_native_ps0:-0} == 0 ]] && shopt -q promptvars; then
            # The DEBUG fallback arms when Bash expands PS1, which happens
            # after every PROMPT_COMMAND hook, so a hook that runs after this
            # one cannot pass for the user's command. ${__ft_armed=} sets the
            # flag and expands to nothing.
            PS1="$start$arm$prompt$end"
        else
            PS1="$start$prompt$end"
            __ft_armed=1
        fi
        return "$last_exit"
    }

    __ft_preexec() {
        # The DEBUG trap passes PIPESTATUS, then the previous command's $_ as
        # the last word of its only command, so $_ is unchanged for the
        # user's command. PS0 owns the event on modern Bash.
        if [[ $BASH_COMMAND == __ft_precmd ]] && (( $# > 1 )); then
            __ft_trap_pipestatus=("${@:1:$#-1}")
        fi
        if (( $# == 0 )); then
            # Only an earlier version's DEBUG trap calls this without an
            # argument, and it replaces $_ with its own words. A re-source
            # can neither see nor clear that trap from inside source, but a
            # function may set a new one. PS0 shells need none, so that one
            # removes itself. Bash 3.2 restores the earlier trap when this
            # function returns, so it stays until that shell exits.
            if [[ ${__ft_native_ps0:-0} == 0 ]]; then
                trap '__ft_preexec "${PIPESTATUS[@]}" "$_"' DEBUG
            else
                trap 'trap - DEBUG' DEBUG
            fi
        fi
        if [[ ${__ft_native_ps0:-0} == 0 && -n ${__ft_armed+set} &&
              $BASH_SUBSHELL == 0 && $BASH_COMMAND != __ft_precmd &&
              -z ${__ft_running_marker:-} ]]; then
            unset __ft_armed
            __ft_running_marker=1
            printf '\033]133;C\033\\'
        fi
        return 0
    }

    # Remove every earlier ft hook from one PROMPT_COMMAND entry: this
    # version's and the previous version's scalar wrapping, and a bare entry.
    __ft_strip_prompt_command() {
        local nl=$'\n'
        __ft_stripped=$1
        __ft_stripped=${__ft_stripped//"__ft_precmd$nl"/}
        __ft_stripped=${__ft_stripped//"${nl}__ft_finish_prompt"/}
        __ft_stripped=${__ft_stripped#'__ft_precmd;'}
        case $__ft_stripped in
            __ft_precmd | __ft_finish_prompt) __ft_stripped='' ;;
        esac
    }

    # Rewrap PROMPT_COMMAND so __ft_precmd runs first and __ft_finish_prompt
    # last. Stripping first keeps one copy of each on a re-source, including
    # one that reassigns PROMPT_COMMAND or prepends another tool's hook. A
    # plain PROMPT_COMMAND stays plain text, so a later plain assignment
    # still replaces all of it. Bash 5.1+ runs each array entry as a hook;
    # older Bash runs only the first entry.
    __ft_prompt_entries=0
    [[ -n ${PROMPT_COMMAND+set} ]] && __ft_prompt_entries=${#PROMPT_COMMAND[@]}
    if (( __ft_prompt_entries > 1 )) &&
       (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
        __ft_prompt_kept=()
        for __ft_prompt_entry in "${PROMPT_COMMAND[@]}"; do
            __ft_strip_prompt_command "$__ft_prompt_entry"
            [[ -n $__ft_stripped ]] && __ft_prompt_kept+=("$__ft_stripped")
        done
        PROMPT_COMMAND=(__ft_precmd ${__ft_prompt_kept[@]+"${__ft_prompt_kept[@]}"} __ft_finish_prompt)
        unset __ft_prompt_kept __ft_prompt_entry
    else
        __ft_strip_prompt_command "${PROMPT_COMMAND:-}"
        PROMPT_COMMAND=$'__ft_precmd\n'"$__ft_stripped"$'\n__ft_finish_prompt'
    fi
    unset __ft_prompt_entries __ft_stripped

    __ft_native_ps0=0
    if (( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4) )) && shopt -q promptvars; then
        # PS0 expands once after Bash reads a complete command. The assignment
        # records that a command started and expands to its output-start marker.
        __ft_native_ps0=1
        __ft_exec_marker=$'\033]133;C\033\\'
        __ft_ps0='${__ft_running_marker:=$__ft_exec_marker}'
        [[ ${PS0:-} == *"$__ft_ps0"* ]] || PS0="${PS0:-}$__ft_ps0"
    elif declare -F __bp_install >/dev/null; then
        case " ${preexec_functions[*]:-} " in
            *' __ft_preexec '*) ;;
            *) preexec_functions+=(__ft_preexec) ;;
        esac
    elif [[ -z $(trap -p DEBUG) ]]; then
        # Bash 3.2 has no PS0. Only take an unowned trap. It acts only once
        # the prompt has armed it, so setup and prompt commands stay
        # invisible. Its one command ends with "$_", which keeps $_ intact.
        trap '__ft_preexec "${PIPESTATUS[@]}" "$_"' DEBUG
    fi
fi
