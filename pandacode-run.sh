#!/bin/sh
set -eu
cd "$(dirname "$0")"
# pandacode-preview: http://127.0.0.1:1420
exec npm run dev
