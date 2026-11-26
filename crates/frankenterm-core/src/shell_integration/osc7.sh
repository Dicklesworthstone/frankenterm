# OSC 7 paths are URI paths. Encode filesystem bytes so the terminal can
# recover the original directory even when it contains spaces or Unicode.
__ft_current_dir() {
    local LC_ALL=C
    local host_name="${HOSTNAME:-${HOST:-localhost}}" encoded='' byte code hex i
    case "$host_name" in ''|*[!A-Za-z0-9._-]*) host_name=localhost ;; esac
    if [[ $PWD != *[!A-Za-z0-9/._~-]* ]]; then
        encoded=$PWD
    else
        for ((i=0; i<${#PWD}; i++)); do
            byte=${PWD:$i:1}
            case "$byte" in
                [A-Za-z0-9/._~-]) encoded+="$byte" ;;
                # Bash 3.2 (macOS /bin/bash) reports bytes above 0x7F as
                # negative codes, so keep only the byte itself.
                *) printf -v code '%d' "'$byte"
                   printf -v hex '%%%02X' "$((code & 255))"; encoded+="$hex" ;;
            esac
        done
    fi
    printf '\033]7;file://%s%s\033\\' "$host_name" "$encoded"
}
