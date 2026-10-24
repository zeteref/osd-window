# Maintainer: zeteref <zeteref@gmail.com>

pkgname=osd-window
pkgver=0.1.1
pkgrel=1
pkgdesc="Wayland OSD overlay that displays text piped on stdin"
arch=('x86_64')
url="https://github.com/zeteref/osd-window"
license=('MIT')
depends=('cairo' 'glib2' 'libxkbcommon' 'pango')
makedepends=('rust' 'pkgconf' 'cairo' 'glib2' 'libxkbcommon' 'pango')
options=('!debug')

# write the built package to /tmp instead of cluttering the source dir
PKGDEST=/tmp

build() {
  cargo build --release --locked --manifest-path "$startdir/Cargo.toml" --target-dir "$srcdir/target"
}

package() {
  install -Dm755 "$srcdir/target/release/osd-window" "$pkgdir/usr/bin/osd-window"
}
