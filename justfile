# bump the version (default patch) and reset pkgrel to 1
bump type="patch":
    #!/usr/bin/env bash
    set -u -o pipefail
    awk -F= -v bump="{{type}}" '
      /^pkgver=/ {
        split($2, v, "[.]");
        if (bump == "major") { v[1]++; v[2]=0; v[3]=0 }
        else if (bump == "minor") { v[2]++; v[3]=0 }
        else { v[3]++ }
        print "pkgver=" v[1] "." v[2] "." v[3];
        next;
      }
      /^pkgrel=/ { print "pkgrel=1"; next; }
      { print }
    ' PKGBUILD > PKGBUILD.tmp && mv PKGBUILD.tmp PKGBUILD
    grep -m1 '^pkgver=' PKGBUILD

# bump and commit (staging is up to you)
release type="patch":
    just bump {{type}}
    git commit -v .
