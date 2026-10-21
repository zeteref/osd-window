# Maintainer: zeteref <zeteref@gmail.com>

pkgname=osd-window-git
_pkgname=osd-window
pkgver=r1.0000000
pkgrel=1
pkgdesc="Wayland OSD overlay that displays text piped on stdin"
arch=('x86_64')
url="https://github.com/zeteref/osd-window"
license=('MIT')
depends=('cairo' 'glib2' 'libxkbcommon' 'pango')
makedepends=('rust' 'pkgconf' 'cairo' 'glib2' 'libxkbcommon' 'pango')
provides=("$_pkgname")
conflicts=("$_pkgname")
replaces=("$_pkgname")
options=('!debug')
source=("$_pkgname::git+https://github.com/zeteref/osd-window.git")
sha256sums=('SKIP')

pkgver() {
  cd "$_pkgname"
  printf "r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short=7 HEAD)"
}

build() {
  cd "$_pkgname"
  cargo build --release --locked
}

package() {
  cd "$_pkgname"
  install -Dm755 "target/release/$_pkgname" "$pkgdir/usr/bin/$_pkgname"
}
