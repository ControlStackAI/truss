#!/usr/bin/env sh
set -eu
test -f expected.txt
grep -q "fixture passes" expected.txt
