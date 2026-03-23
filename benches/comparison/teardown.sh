#!/bin/bash
# Stop and remove all benchmark containers and volumes.
set -euo pipefail
cd "$(dirname "$0")"

echo "Stopping benchmark containers..."
docker compose --profile all down -v --remove-orphans
echo "Done. All containers and volumes removed."
