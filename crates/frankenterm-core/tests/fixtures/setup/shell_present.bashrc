# ~/.bashrc
export PATH=$HOME/bin:$PATH

# FT-BEGIN (do not edit this block)
# ft: OSC 133 prompt markers for deterministic state detection
__ft_prompt_start() { printf '\e]133;A\e\\'; }
__ft_command_start() { printf '\e]133;C\e\\'; }
# FT-END

alias ll='ls -la'
