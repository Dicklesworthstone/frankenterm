#!/bin/sh
# Based on Github Action
# https://github.com/yshui/git-clang-format-lint

diff=$(git-clang-format --diff --commit "$CI_MERGE_REQUEST_DIFF_BASE_SHA")
[ "$diff" = "no modified files to format" ] && exit 0
[ "$diff" = "clang-format did not modify any files" ] && exit 0

printf "You have introduced coding style breakages, suggested changes:\n\n"

echo "${diff}" | colordiff
exit 1
