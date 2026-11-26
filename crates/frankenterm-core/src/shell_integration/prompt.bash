# Emit one OSC 133 command boundary per interactive submission, including compound commands.
if [[ $- == *i* ]]; then
    # An older setup block installed this exact DEBUG trap. A live re-source
    # can retire that owned hook without disturbing a user's trap.
    if [[ $(trap -p DEBUG) == "trap -- '__ft_preexec' DEBUG" ]]; then
        trap - DEBUG
    fi

    __ft_precmd() {
        local last_exit=$?
        __ft_armed=0
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
        local start='\[\e]133;A\e\\\]' end='\[\e]133;B\e\\\]'
        local prompt=${PS1:-}
        prompt=${prompt//"$start"/}
        prompt=${prompt//"$end"/}
        PS1="$start$prompt$end"
        __ft_armed=1
        return "$last_exit"
    }

    __ft_preexec() {
        # A sourced legacy rc may have installed this function as a DEBUG
        # hook in its caller. PS0 owns the event on modern Bash.
        if [[ ${__ft_native_ps0:-0} == 0 && ${__ft_armed:-0} == 1 &&
              $BASH_SUBSHELL == 0 && $BASH_COMMAND != __ft_precmd &&
              -z ${__ft_running_marker:-} ]]; then
            __ft_armed=0
            __ft_running_marker=1
            printf '\033]133;C\033\\'
        fi
        return 0
    }

    # In Bash 5.1+ every PROMPT_COMMAND array entry is a separate hook.
    # Older Bash executes the scalar entry as shell code.
    if [[ ${PROMPT_COMMAND[0]:-} != __ft_precmd$'\n'*'__ft_finish_prompt' ]] &&
       ! [[ ${PROMPT_COMMAND[0]:-} == __ft_precmd && " ${PROMPT_COMMAND[*]:-} " == *' __ft_finish_prompt '* ]]; then
        # Strip just the previous version's scalar prefix before rebuilding.
        case ${PROMPT_COMMAND[0]:-} in
            __ft_precmd) PROMPT_COMMAND[0]='' ;;
            '__ft_precmd;'*) PROMPT_COMMAND[0]=${PROMPT_COMMAND[0]#'__ft_precmd;'} ;;
        esac
        if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
            PROMPT_COMMAND=(__ft_precmd ${PROMPT_COMMAND[@]+"${PROMPT_COMMAND[@]}"} __ft_finish_prompt)
        else
            PROMPT_COMMAND=$'__ft_precmd\n'"${PROMPT_COMMAND:-}"$'\n__ft_finish_prompt'
        fi
    fi

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
        # Bash 3.2 has no PS0. Only take an unowned trap and arm it after
        # prompt hooks finish, so setup and prompt commands stay invisible.
        trap '[[ $BASH_COMMAND == __ft_precmd ]] || __ft_preexec' DEBUG
    fi
fi
