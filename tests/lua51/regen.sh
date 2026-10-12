#!/bin/sh
# Rebuilds the expected outputs (*.out) of the Lua 5.1 tests with PUC-Rio Lua 5.1.5.
#
#   tests/lua51/regen.sh [path/to/lua5.1.5]
#
# Without an interpreter, downloads lua-5.1.5.tar.gz, checks its sha256 and builds it in
# target/lua51 (a C compiler and make are needed). The outputs are committed, so running the tests
# does not need PUC-Rio Lua.
set -eu

dir=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$dir/../.." && pwd)
lua=${1:-}
if [ -n "$lua" ]; then
    lua="$(cd "$(dirname "$lua")" && pwd)/$(basename "$lua")"
fi

if [ -z "$lua" ]; then
    build="$root/target/lua51"
    lua="$build/lua-5.1.5/src/lua"
    if [ ! -x "$lua" ]; then
        mkdir -p "$build"
        cd "$build"
        curl -sSfLO https://www.lua.org/ftp/lua-5.1.5.tar.gz
        sum=$(shasum -a 256 lua-5.1.5.tar.gz | cut -d' ' -f1)
        if [ "$sum" != 2640fc56a795f29d28ef15e13c34a47e223960b0240e8cb0a82d9b0738695333 ]; then
            echo "unexpected sha256 for lua-5.1.5.tar.gz: $sum" >&2
            exit 1
        fi
        tar xzf lua-5.1.5.tar.gz
        case "$(uname)" in
            Darwin) platform=macosx ;;
            Linux) platform=linux ;;
            *) platform=posix ;;
        esac
        make -C lua-5.1.5 "$platform" >/dev/null
    fi
fi

cd "$dir"
for script in *.lua; do
    # Scripts print their results; errors they do not catch end up in the output too.
    "$lua" "$script" >"${script%.lua}.out" 2>&1 || true
done
echo "regenerated $(ls *.lua | wc -l | tr -d ' ') outputs with $lua"
