# Maintainer: Bruno Goncalves <bigbruno@gmail.com>

pkgname=auto-rules-io
pkgver=1.0.0
pkgrel=0
arch=('any')
license=('GPL')
url="https://github.com/biglinux/auto-rules-io"
pkgdesc="Automatic change io scheduler"
source=("git+https://github.com/biglinux/auto-rules-io.git")
md5sums=(SKIP)

package() {
    cp -r "${srcdir}/auto-rules-io/auto-rules-io/etc/" "${pkgdir}/"
} 
