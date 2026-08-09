#!/usr/bin/env bash
set -euo pipefail

echo "Warning: scripts/run_live.sh is deprecated; use scripts/run_venue_live.sh." >&2
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/run_venue_live.sh" "$@"
