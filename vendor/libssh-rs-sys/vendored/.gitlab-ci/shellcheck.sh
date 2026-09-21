#!/bin/bash
# Simplified and de-github-ed version of
# https://github.com/ludeeus/action-shellcheck/blob/master/action.yaml

statuscode=0

declare -a filepaths
shebangregex="^#! */[^ ]*/(env *)?[abk]*sh"
set -f # temporarily disable globbing so that globs in inputs aren't expanded

while IFS= read -r -d '' file; do
  filepaths+=("$file")
done < <(find . \
    -type f \
    '(' \
    -name '*.bash' \
    -o -name '.bashrc' \
    -o -name 'bashrc' \
    -o -name '.bash_aliases' \
    -o -name '.bash_completion' \
    -o -name '.bash_login' \
    -o -name '.bash_logout' \
    -o -name '.bash_profile' \
    -o -name 'bash_profile' \
    -o -name '*.ksh' \
    -o -name 'suid_profile' \
    -o -name '*.zsh' \
    -o -name '.zlogin' \
    -o -name 'zlogin' \
    -o -name '.zlogout' \
    -o -name 'zlogout' \
    -o -name '.zprofile' \
    -o -name 'zprofile' \
    -o -name '.zsenv' \
    -o -name 'zsenv' \
    -o -name '.zshrc' \
    -o -name 'zshrc' \
    -o -name '*.sh' \
    -o -path '*/.profile' \
    -o -path '*/profile' \
    -o -name '*.shlib' \
    ')' \
    -print0)

while IFS= read -r -d '' file; do
    head -n1 "$file" | grep -Eqs "$shebangregex" || continue
    filepaths+=("$file")
done < <(find . \
    -type f ! -name '*.*' -perm /111 \
    -print0)

shellcheck "${filepaths[@]}" || statuscode=$?

set +f # re-enable globbing

exit "$statuscode"
