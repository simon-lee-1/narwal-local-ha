#!/bin/sh
set -eu
count=0
while [ "$count" -lt 100 ]; do
    [ ! -S /run/narwal-local-ha/robot.sock ] || exit 0
    sleep 0.1
    count=$((count + 1))
done
echo 'Narwal service socket was not created' >&2
exit 1
