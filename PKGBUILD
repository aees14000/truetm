# Maintainer: AEES14000 <aees14000@gmail.com>
pkgname=truetm
pkgver=1.0.0
pkgrel=1
pkgdesc="A simple truecolor terminal multiplexer"
arch=('x86_64')
url="https://github.com/theludd/truetm"
license=('Unlicense')
makedepends=('cargo')
source=()
sha256sums=()

build() {
  cd "$startdir"
  cargo build --release --locked
}

package() {
  cd "$startdir"
  install -Dm755 target/release/truetm "$pkgdir/usr/bin/truetm"
  install -Dm644 truetm.1 "$pkgdir/usr/share/man/man1/truetm.1"
  install -Dm644 UNLICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
}
