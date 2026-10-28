# FreeBSD port Makefile for veles-agent.
#
# FreeBSD is the only host for the `jail` sandbox backend, and the one
# where shell.rs's bash -> sh fallback matters: /bin/sh here is not
# bash, and veles shells out with `-c` from six call sites.

PORTNAME=	veles-agent
DISTVERSION=	0.1.0
CATEGORIES=	misc

MAINTAINER=	noreply@example.invalid
COMMENT=	Rust core + Tcl scripting AI agent framework
WWW=		https://github.com/veles-agent/veles-agent

LICENSE=	AGPLv3

BUILD_DEPENDS=	${LOCALBASE}/bin/tclsh8.6:lang/tcl86
LIB_DEPENDS=	libssl.so:security/openssl \
		libsqlite3.so:databases/sqlite3
RUN_DEPENDS=	${LOCALBASE}/bin/tclsh8.6:lang/tcl86 \
		tcllib>0:devel/tcllib

USES=		cargo pkgconfig ssl sqlite
USE_RC_SUBR=	veles

OPTIONS_DEFINE=		TK GIT RIPGREP
OPTIONS_DEFAULT=	TK GIT RIPGREP
TK_DESC=		Tcl/Tk desktop GUI
GIT_DESC=		Checkpoints and git-backed plugin state
RIPGREP_DESC=		Fast search tool (a slower fallback is used without it)

TK_RUN_DEPENDS=		${LOCALBASE}/bin/wish8.6:x11-toolkits/tk86
GIT_RUN_DEPENDS=	git:devel/git
RIPGREP_RUN_DEPENDS=	rg:textproc/ripgrep

CARGO_BUILD_ARGS=	--bin veles
# The full suite wants a Tk display; the library tests do not.
CARGO_TEST_ARGS=	--lib

do-install:
	${INSTALL_PROGRAM} ${CARGO_TARGET_DIR}/${CARGO_BUILD_TARGET}/release/veles \
		${STAGEDIR}${PREFIX}/bin/veles

.include <bsd.port.mk>
