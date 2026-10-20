#!/bin/sh
set -e

usage() {
    cat <<EOF
$0: Make a macOS icns file from an SVG with rsvg-convert, ImageMagick and iconutil.

Follows the Big Sur+ icon template:
  - 1024x1024 canvas with a rounded-square (squircle) background
  - Icon artwork scaled to fit inside an 824x824 content area, centered
  - Transparent padding outside the squircle so the Dock/Finder render it
    like other first-party macOS apps.

usage: $0 [SVG [ICNS [ICONSET]]]

ARGUMENTS
    SVG     The SVG file to convert
            Defaults to ./crates/syntra-ui/ui/assets/shell/syntra.svg
    ICNS    The icns file to create
            Defaults to ./target/icon.icns
    ICONSET The iconset directory to create
            Defaults to ./target/icon.iconset
            Intermediate output. An existing directory is reused only when
            every entry in it is one of the icon files this script writes;
            anything else is left untouched and the script stops.
EOF
}

case "${1:-}" in
    -h | --help) usage; exit 0 ;;
esac

die() {
    echo "$0: $*" >&2
    exit 1
}

[ $# -le 3 ] || die "too many arguments (see --help)"
svg="${1:-./crates/syntra-ui/ui/assets/shell/syntra.svg}"
icns="${2:-./target/icon.icns}"
iconset="${3:-./target/icon.iconset}"

set -u

# Everything is checked before the first change to disk, so a wrong argument or
# a missing tool leaves the file system exactly as it was.
[ -f "$svg" ] || die "SVG not found: $svg"
for tool in rsvg-convert magick; do
    command -v "$tool" >/dev/null 2>&1 || die "required tool not found: $tool"
done
if [ "$(uname -s)" = "Darwin" ]; then
    command -v iconutil >/dev/null 2>&1 || die "required tool not found: iconutil"
fi

# The files this script writes into the iconset: a fixed list, the only names
# it will ever create, overwrite or remove there.
icon_names="icon_512x512@2x.png icon_512x512.png icon_256x256@2x.png icon_256x256.png
icon_128x128@2x.png icon_128x128.png icon_32x32@2x.png icon_32x32.png
icon_16x16@2x.png icon_16x16.png"

is_known_icon() {
    for known in $icon_names; do
        [ "$1" = "$known" ] && return 0
    done
    return 1
}

# The iconset is this script's own intermediate output. It is used only when
# every entry in it is one of the files above, a regular file, never a link, so
# a directory holding anything else is somebody's data and is left untouched. A
# link at the iconset path is not followed either. No marker file is involved:
# what the directory contains is the proof, and the directory handed to
# iconutil holds nothing but icons.
[ ! -L "$iconset" ] || die "iconset path is a symbolic link: $iconset"
if [ -e "$iconset" ]; then
    [ -d "$iconset" ] || die "iconset path exists and is not a directory: $iconset"
    for entry in "$iconset"/* "$iconset"/.[!.]* "$iconset"/..?*; do
        [ -e "$entry" ] || [ -L "$entry" ] || continue
        name="$(basename "$entry")"
        if [ -L "$entry" ] || [ ! -f "$entry" ] || ! is_known_icon "$name"; then
            die "refusing to use $iconset: it contains $name, which this script did not create"
        fi
    done
fi

mkdir -p "$(dirname "$iconset")" "$(dirname "$icns")"

# Scratch space is a fresh directory this script creates and owns; the only
# recursive removal below is of that directory, never of an argument. The trap
# names the directory by value, so it needs no shell-specific feature.
workdir="$(mktemp -d "$(dirname "$iconset")/icon-work.XXXXXX")"
trap 'rm -rf -- "$workdir"' EXIT HUP INT TERM

# Reusing the iconset: remove only the known icon files, by exact name.
if [ -d "$iconset" ]; then
    for name in $icon_names; do
        rm -f -- "$iconset/$name"
    done
fi
mkdir -p "$iconset"

# Big Sur+ macOS icon template proportions (in a 1024 canvas):
#   canvas  = 1024
#   squircle = 824  (the white rounded-square background, inset 100px)
#   content  = 560  (artwork inside the squircle, with generous margin)
#   radius   = 185  (~22.5% of the squircle, the characteristic curvature)
CANVAS=1024
SQUIRCLE=824
CONTENT=560
RADIUS=185
BG_COLOR="#FFFFFF"
SQUIRCLE_OFFSET=$(( (CANVAS - SQUIRCLE) / 2 ))
CONTENT_OFFSET=$(( (CANVAS - CONTENT) / 2 ))

# 1) Render the SVG to the content size at full fidelity.
#    rsvg-convert handles our SVG correctly; ImageMagick sometimes crops it.
rsvg-convert -w "$CONTENT" -h "$CONTENT" "$svg" -o "$workdir/content.png"

# 2) Draw the rounded-square (squircle) background on a transparent canvas.
#    The squircle is inset from the canvas edges (transparent padding), so the
#    Dock/Finder render it at the same visual size as other first-party apps.
magick -size ${CANVAS}x${CANVAS} xc:none \
    -fill "$BG_COLOR" \
    -draw "roundrectangle ${SQUIRCLE_OFFSET},${SQUIRCLE_OFFSET} $((CANVAS-SQUIRCLE_OFFSET-1)),$((CANVAS-SQUIRCLE_OFFSET-1)) $RADIUS,$RADIUS" \
    "$workdir/background.png"

# 3) Composite the artwork onto the background, centered inside the content area.
magick "$workdir/background.png" \
    "$workdir/content.png" -geometry +${CONTENT_OFFSET}+${CONTENT_OFFSET} -composite \
    -colorspace sRGB -type TrueColorAlpha PNG32:"$workdir/icon-1024.png"

# 4) Generate each iconset size from the master so all sizes share the same
#    squircle proportions and look consistent at every resolution.
for size in 1024 512 256 128 64 32 16; do
    magick "$workdir/icon-1024.png" -resize ${size}x${size} \
        -colorspace sRGB -type TrueColorAlpha PNG32:"$workdir/${size}.png"
done

cp "$workdir/1024.png" "$iconset"/icon_512x512@2x.png
cp "$workdir/512.png"  "$iconset"/icon_512x512.png
cp "$workdir/512.png"  "$iconset"/icon_256x256@2x.png
cp "$workdir/256.png"  "$iconset"/icon_256x256.png
cp "$workdir/256.png"  "$iconset"/icon_128x128@2x.png
cp "$workdir/128.png"  "$iconset"/icon_128x128.png
cp "$workdir/64.png"   "$iconset"/icon_32x32@2x.png
cp "$workdir/32.png"   "$iconset"/icon_32x32.png
cp "$workdir/32.png"   "$iconset"/icon_16x16@2x.png
cp "$workdir/16.png"   "$iconset"/icon_16x16.png

mkdir -p "$(dirname "$icns")"

# Menu bar template icon: flatten all RGB channels to 0 (black) while keeping
# alpha so the artwork reads as a clean silhouette. NSStatusBarButton tints
# template images to match the menu bar appearance in light and dark modes.
menubar_template="$(dirname "$icns")/menubar-template.png"
rsvg-convert -w 44 -h 44 "$svg" -o "$workdir/menubar-44.png"
magick "$workdir/menubar-44.png" -channel RGB -evaluate set 0 +channel \
    "$menubar_template"

if ! iconutil -c icns "$iconset" -o "$icns"; then
    if ! command -v perl >/dev/null 2>&1; then
        echo "iconutil failed and perl is not available for the fallback icns writer" >&2
        exit 1
    fi

    echo "iconutil rejected the iconset; writing icns directly" >&2
    perl - "$icns" "$iconset" <<'PERL'
use strict;
use warnings;

my ($icns, $iconset) = @ARGV;
my @icons = (
    [ 'icp4', "$iconset/icon_16x16.png" ],
    [ 'ic11', "$iconset/icon_16x16\@2x.png" ],
    [ 'icp5', "$iconset/icon_32x32.png" ],
    [ 'ic12', "$iconset/icon_32x32\@2x.png" ],
    [ 'ic07', "$iconset/icon_128x128.png" ],
    [ 'ic13', "$iconset/icon_128x128\@2x.png" ],
    [ 'ic08', "$iconset/icon_256x256.png" ],
    [ 'ic14', "$iconset/icon_256x256\@2x.png" ],
    [ 'ic09', "$iconset/icon_512x512.png" ],
    [ 'ic10', "$iconset/icon_512x512\@2x.png" ],
);

my $body = '';
for my $icon (@icons) {
    my ($type, $path) = @$icon;
    open my $fh, '<:raw', $path or die "$path: $!";
    local $/;
    my $png = <$fh>;
    $body .= $type . pack('N', length($png) + 8) . $png;
}

open my $out, '>:raw', $icns or die "$icns: $!";
print {$out} 'icns' . pack('N', length($body) + 8) . $body;
PERL
fi
