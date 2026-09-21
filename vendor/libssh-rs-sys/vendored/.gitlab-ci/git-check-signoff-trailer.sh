#!/bin/bash

if [ $# != 1 ]; then
    echo "Usage: $0 UPSTREAM_COMMIT_SHA"
    exit 1
fi

failed=0

if [ -z "$CI_COMMIT_SHA" ]; then
    echo "CI_COMMIT_SHA is not set"
    exit 1
fi

CI_COMMIT_RANGE="$1..$CI_COMMIT_SHA"

red='\033[0;31m'
blue='\033[0;34m'

echo -e "${blue}Checking commit range: $CI_COMMIT_RANGE"
echo
echo

for commit in $(git rev-list "$CI_COMMIT_RANGE"); do
    git show -s --format=%B "$commit" | grep "^Signed-off-by: " >/dev/null 2>&1
    ret=$?
    if [ $ret -eq 1 ]; then
        echo -e "${red}  >>> Missing Signed-off-by trailer in commit $commit"
        failed=$(("$failed" + 1))
    fi
done

echo
echo

exit $failed
