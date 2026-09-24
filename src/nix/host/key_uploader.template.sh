set -euo pipefail

destination=%DESTINATION%
tmp="${destination}.tmp"
user=%USER%
group=%GROUP%
permissions=%PERMISSIONS%
require_ownership=%REQUIRE_OWNERSHIP%

mkdir -p $(dirname "$destination")
touch "$tmp"

# chown reports a missing user or group itself
# macOS has no getent
if [ -n "$require_ownership" ]; then
	chown "$user:$group" "$tmp"
else
	chown "$user:$group" "$tmp" || >&2 echo "Skipping chown."
fi

chmod "$permissions" "$tmp"
cat <&0 >$tmp
mv "$tmp" "$destination"
