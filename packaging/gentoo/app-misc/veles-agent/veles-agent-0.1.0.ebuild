# Copyright 2026
# Distributed under the terms of the GNU Affero General Public License v3

EAPI=8

inherit cargo

DESCRIPTION="Rust core + Tcl scripting AI agent framework"
HOMEPAGE="https://github.com/veles-agent/veles-agent"

LICENSE="AGPL-3"
SLOT="0"
KEYWORDS="~amd64 ~arm64"
IUSE="+tk podman firecracker desktop-images"

# The crate set is generated, not hand-listed:
#   cargo-ebuild   (app-portage/cargo-ebuild)
# regenerates CRATES from Cargo.lock. Committing a hand-edited list
# would go stale silently the first time a dependency moves.
CRATES=""

DEPEND="
	dev-lang/tcl:0=
	dev-libs/openssl:0=
	sys-libs/zlib:0=
	dev-db/sqlite:3=
	tk? ( dev-lang/tk:0= )
"
RDEPEND="
	${DEPEND}
	dev-tcltk/tcllib
	dev-vcs/git
	sys-apps/ripgrep
	podman? ( app-containers/podman )
	firecracker? ( sys-fs/e2fsprogs )
	desktop-images? (
		media-libs/libwebp
		media-libs/libheif
	)
"
BDEPEND="
	virtual/pkgconfig
	llvm-core/clang
"

src_compile() {
	cargo_src_compile --bin veles
}

src_test() {
	# The full suite wants a Tk display; the library tests do not.
	TMPDIR="${TMPDIR:-/dev/shm}" cargo_src_test --lib
}

src_install() {
	cargo_src_install --path . --bin veles
	systemd_douserunit scripts/install/units/veles.service
}

pkg_postinst() {
	elog "veles stores config in ~/.config/veles and state in"
	elog "~/.local/share/veles (XDG). An existing ~/.veles is migrated"
	elog "automatically on first run; see 'veles migrate-dirs --dry-run'."
	elog ""
	elog "Verify the install, including the sandbox fence:"
	elog "    veles doctor"
	elog "    veles doctor --sandbox"
}
