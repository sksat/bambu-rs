#!/usr/bin/env bash
# Build the DOOM frame source for `bambu serve --emulate-doom`.
#
# Fetches doomgeneric (GPL-2.0) and stb_image_write.h (public domain) into
# ./build and compiles them with doomgeneric_bambu.c. Neither is vendored into
# this repo: bambu-rs is MIT, its build must not need a C toolchain, and the
# demo is the only thing that wants DOOM.
#
# Also fetches the shareware doom1.wad unless one is already here. That WAD is
# freely redistributable; the retail ones are not, and if you own one you can
# point the engine at it instead with -iwad.
set -euo pipefail
cd "$(dirname "$0")"

# Pinned, not tracked: the object list in the Makefile is doomgeneric's own, and
# a file added upstream would silently drop out of the build.
DOOMGENERIC_REPO="https://github.com/ozkl/doomgeneric.git"
DOOMGENERIC_COMMIT="dcb7a8dbc7a16ce3dda29382ac9aae9d77d21284"
STB_URL="https://raw.githubusercontent.com/nothings/stb/master/stb_image_write.h"
WAD_URL="https://github.com/Akbar30Bill/DOOM_wads/raw/master/doom1.wad"
# The shareware DOOM 1.9 IWAD. Checked, because a mirror that answers an
# HTML error page with a 200 is the normal failure here.
WAD_MD5="f0cefca49926d00903cf57551d901abe"

mkdir -p build

if [ ! -d build/doomgeneric ]; then
  echo "fetching doomgeneric @ ${DOOMGENERIC_COMMIT:0:12}"
  git clone --quiet "$DOOMGENERIC_REPO" build/doomgeneric
fi
git -C build/doomgeneric checkout --quiet "$DOOMGENERIC_COMMIT"

if [ ! -f build/stb_image_write.h ]; then
  echo "fetching stb_image_write.h"
  curl -sSLf -o build/stb_image_write.h "$STB_URL"
fi

if [ ! -f build/doom1.wad ]; then
  echo "fetching the shareware doom1.wad"
  curl -sSLf -o build/doom1.wad.part "$WAD_URL"
  got="$(md5sum build/doom1.wad.part | cut -d' ' -f1)"
  if [ "$got" != "$WAD_MD5" ]; then
    rm -f build/doom1.wad.part
    echo "the WAD that came back is not the shareware doom1.wad (md5 $got)." >&2
    echo "Fetch one yourself and put it at $(pwd)/build/doom1.wad." >&2
    exit 1
  fi
  mv build/doom1.wad.part build/doom1.wad
fi

make --no-print-directory "$@"

cat <<EOF

built $(pwd)/build/bambu-doom-engine

  see it on its own:
    ./build/bambu-doom-engine -raw -iwad ./build/doom1.wad -warp 1 1 | ffplay -f mjpeg -

  play it through a printer that does not exist:
    bambu serve --fake --emulate --serial DOOM00000000001 --access-code 12345678 \\
      --emulate-doom \\
      --emulate-doom-engine $(pwd)/build/bambu-doom-engine \\
      --emulate-doom-arg -iwad --emulate-doom-arg $(pwd)/build/doom1.wad \\
      --emulate-doom-arg -warp --emulate-doom-arg 1 --emulate-doom-arg 1
EOF
