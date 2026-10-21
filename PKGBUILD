# Maintainer: zeteref <zeteref@gmail.com>

pkgname=osd-window
pkgver=0.1.0
pkgrel=1
pkgdesc="Wayland OSD overlay that displays text piped on stdin"
arch=('x86_64')
url="https://github.com/zeteref/osd-window"
license=('MIT')
depends=('cairo' 'glib2' 'libxkbcommon' 'pango')
makedepends=('rust' 'pkgconf' 'cairo' 'glib2' 'libxkbcommon' 'pango')
source=("$pkgname::git+https://github.com/zeteref/osd-window.git#tag=v$pkgver")
sha256sums=('SKIP')

build() {
  cd "$pkgname"
  cargo build --release --locked
}

package() {
  cd "$pkgname"
  install -Dm755 "target/release/$pkgname" "$pkgdir/usr/bin/$pkgname"
}
