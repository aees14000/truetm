# Maintainer: AEES14000 <aees14000@gmail.com>
pkgname=truetm
pkgver=1.0.0
pkgrel=1
pkgdesc="A simple truecolor terminal multiplexer"
url="https://github.com/theludd/truetm"
arch="x86_64 aarch64"
license="Unlicense"
makedepends="cargo"
source=""
sha512sums=""
builddir="$startdir"

build() {
	cd "$startdir"
	cargo build --release --locked
}

package() {
	cd "$startdir"
	install -Dm755 target/release/truetm "$pkgdir"/usr/bin/truetm
	install -Dm644 truetm.1 "$pkgdir"/usr/share/man/man1/truetm.1
	gzip -9n "$pkgdir"/usr/share/man/man1/truetm.1
	install -Dm644 UNLICENSE "$pkgdir"/usr/share/licenses/$pkgname/LICENSE
}
