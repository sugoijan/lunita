#!/bin/bash
# wd.sh <limit_s> cmd args...: kills the command above 2 GB RSS or after limit_s.
lim=$1; shift; "$@" & pid=$!; start=$(date +%s); maxrss=0
while kill -0 $pid 2>/dev/null; do
  rss=$(ps -o rss= -p $pid 2>/dev/null | tr -d ' ')
  [ -n "$rss" ] && [ "$rss" -gt "$maxrss" ] && maxrss=$rss
  if [ -n "$rss" ] && [ "$rss" -gt 2000000 ]; then kill -9 $pid; echo "WATCHDOG: rss=${rss}KB"; break; fi
  if [ $(( $(date +%s) - start )) -ge "$lim" ]; then kill -9 $pid; echo "WATCHDOG: ${lim}s"; break; fi
  sleep 0.2
done
wait $pid; rc=$?; echo "[wd] rc=$rc maxrss=${maxrss}KB"; exit $rc
