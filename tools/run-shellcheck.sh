#!/bin/bash
# Run shellcheck over every .sh file under scripts/ and tools/.
#
# Uses find + xargs so an empty directory (no .sh files) is not
# an error. A naked `shellcheck scripts/*.sh tools/*.sh` would
# fail if either glob expanded to nothing.

set -euo pipefail

mapfile -d '' FILES < <(find scripts tools -type f -name '*.sh' -print0)

if [ ${#FILES[@]} -eq 0 ]; then
    echo "run-shellcheck: no shell scripts found."
    exit 0
fi

exec shellcheck -x "${FILES[@]}"
