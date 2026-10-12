#!/bin/sh
# Writes the seeds of the fuzz targets from the limit cases of tests/limits.rs: seeds.txt (the
# sources up to 4 KiB, which the grammar target splices into programs) and seeds/chunk (every
# source up to 64 KiB, as the initial corpus of the chunk target, each prefixed with the byte that
# selects the default mode).
set -eu
fuzz=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$fuzz/.." && pwd)
cd "$root"
cargo test -q --test limits -- --ignored export_cases >/dev/null
cases="$root/target/limit-cases"
python3 -I - "$cases" "$fuzz" <<'PY'
import os, sys
cases, fuzz = sys.argv[1], sys.argv[2]
data = open(os.path.join(cases, 'cases.txt'), 'rb').read()
seeds, chunks = [], os.path.join(fuzz, 'seeds', 'chunk')
os.makedirs(chunks, exist_ok=True)
for f in os.listdir(chunks):
    os.remove(os.path.join(chunks, f))
while data:
    header, data = data.split(b'\n', 1)
    case, expected, length = header.split(b' ')
    source, data = data[:int(length)], data[int(length) + 1:]
    if len(source) <= 4096:
        seeds.append(header + b'\n' + source + b'\n')
    if len(source) <= 65536:
        open(os.path.join(chunks, case.decode() + '.lua'), 'wb').write(b'\0' + source)
open(os.path.join(fuzz, 'seeds.txt'), 'wb').write(b''.join(seeds))
print(f'{len(seeds)} seeds, {len(os.listdir(chunks))} chunk seeds')
PY
