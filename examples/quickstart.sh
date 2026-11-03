#!/bin/sh
# The README quick start, as one file you can copy into a service definition.
#
#   NUBI_TOKEN=nubi_... ./examples/quickstart.sh
#
# The token is the only thing you must provide (nubi app: Agents > New agent).
# Everything else is a default: the engine is https://api.nubi.chat and Claude
# Code answers with its own login. Set NUBI_URL for another engine, and pass
# any nubi-agent flag through, e.g. `./examples/quickstart.sh --harness codex`.
set -eu
: "${NUBI_TOKEN:?set NUBI_TOKEN to an agent token from the nubi app (Agents > New agent)}"
exec nubi-agent "$@"
