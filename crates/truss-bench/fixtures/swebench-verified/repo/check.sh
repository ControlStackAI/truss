#!/bin/sh
set -eu
test "$(cat answer.txt)" = "fixed"
