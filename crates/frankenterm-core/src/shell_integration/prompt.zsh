# ft: OSC 133 command lifecycle and prompt boundaries for interactive Zsh.
# Wrap the current theme's prompt after its precmd hooks have updated it.
__ft_zsh_restore_prompt() {
    if [[ ${__ft_zsh_marked_prompt-} == "$PS1" && -n ${__ft_zsh_marked_prompt-} ]]; then
        PS1=$__ft_zsh_original_prompt
    fi
}

__ft_zsh_prompt() {
    local start=$'\e]133;A\e\\' end=$'\e]133;B\e\\'
    if [[ -o promptpercent ]]; then
        start="%{${start}%}"
        end="%{${end}%}"
    fi
    __ft_zsh_original_prompt=$PS1
    PS1="${start}${PS1}${end}"
    __ft_zsh_marked_prompt=$PS1
}

__ft_precmd() {
    local last_exit=$?
    # Empty submissions can start a new prompt without a preexec.
    __ft_zsh_restore_prompt
    if (( __ft_zsh_executing )); then
        printf '\033]133;D;%d\033\\' "$last_exit"
        __ft_zsh_executing=0
    fi
    __ft_current_dir
}

__ft_preexec() {
    # Themes and commands that inspect or extend PS1 see their own prompt.
    __ft_zsh_restore_prompt
    __ft_zsh_executing=1
    printf '\033]133;C\033\\'
}

if [[ -o interactive ]]; then
    typeset -gi __ft_zsh_executing
    typeset -ga precmd_functions preexec_functions
    # Test membership by name. An (Ie) index is 0 for the first element under
    # KSH_ARRAYS, which would read as missing and add the hook again.
    [[ " ${(j: :)precmd_functions[@]} " == *" __ft_precmd "* ]] ||
        precmd_functions=(__ft_precmd "${precmd_functions[@]}")
    [[ " ${(j: :)precmd_functions[@]} " == *" __ft_zsh_prompt "* ]] ||
        precmd_functions+=(__ft_zsh_prompt)
    [[ " ${(j: :)preexec_functions[@]} " == *" __ft_preexec "* ]] ||
        preexec_functions=(__ft_preexec "${preexec_functions[@]}")
fi
