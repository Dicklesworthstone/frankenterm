# ft: OSC 133 prompt markers for deterministic state detection
# These markers help ft detect prompt boundaries and command execution.

function __ft_prompt_start --on-event fish_prompt
    # fish's own $hostname needs no process per prompt, nor a hostname binary.
    set -l host_name $hostname
    string match -qr '^[a-zA-Z0-9._-]+$' -- "$host_name"; or set host_name localhost
    set -l encoded (string escape --style=url -- "$PWD")
    printf '\e]7;file://%s%s\e\\' "$host_name" "$encoded"
    printf '\e]133;A\e\\'
end

function __ft_command_start --on-event fish_preexec
    printf '\e]133;B\e\\'
    printf '\e]133;C\e\\'
end

function __ft_command_end --on-event fish_postexec
    printf '\e]133;D;%d\e\\' $status
end
