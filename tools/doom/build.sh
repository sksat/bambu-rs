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
# Pinned to a commit, not `master`, and checked. It was the one fetched from a
# moving branch: the same build would quietly compile different C over time, and
# whatever upstream became would be compiled into a program `bambu serve` runs.
STB_COMMIT="1ee679ca2ef753a528db5ba6801e1067b40481b8"
STB_URL="https://raw.githubusercontent.com/nothings/stb/${STB_COMMIT}/stb_image_write.h"
STB_SHA256="cbd5f0ad7a9cf4468affb36354a1d2338034f2c12473cf1a8e32053cb6914a05"
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
  echo "fetching stb_image_write.h @ ${STB_COMMIT:0:12}"
  curl -sSLf -o build/stb_image_write.h.part "$STB_URL"
  got="$(sha256sum build/stb_image_write.h.part | cut -d' ' -f1)"
  if [ "$got" != "$STB_SHA256" ]; then
    rm -f build/stb_image_write.h.part
    echo "stb_image_write.h does not match its pinned digest (sha256 $got)." >&2
    exit 1
  fi
  mv build/stb_image_write.h.part build/stb_image_write.h
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

  play it through a printer that does not exist — with a bambu built for it,
  because --emulate-doom is behind a feature that is off by default:
    cargo build --features doom
    bambu serve --fake --emulate --serial DOOM00000000001 --access-code 12345678 \\
      --emulate-doom \\
      --emulate-doom-engine $(pwd)/build/bambu-doom-engine \\
      --emulate-doom-arg -workdir --emulate-doom-arg $(pwd)/build/saves \\
      --emulate-doom-arg -iwad --emulate-doom-arg $(pwd)/build/doom1.wad \\
      --emulate-doom-arg -warp --emulate-doom-arg 1 --emulate-doom-arg 1
EOF
