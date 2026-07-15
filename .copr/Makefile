# SRPM generation for COPR ("make srpm" source build method). COPR invokes
# this as: make -f .copr/Makefile srpm outdir="..." spec="..."
# The version is derived from git describe on the checked-out commit, so a
# build of a release tag packages exactly that tag and any other committish
# packages a snapshot version.

TOP := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/..)
SPEC := $(TOP)/rpm/tuwunel.spec

srpm:
	-dnf -y install git-core rpm-build
	set -eux; \
	version="$$(git -C '$(TOP)' describe --tags --always | sed 's/^v//; s/-/^/; s/-/./g')"; \
	mkdir -p '$(TOP)/rpmbuild/SOURCES'; \
	git -C '$(TOP)' archive --prefix "tuwunel-$$version/" \
		--output "$(TOP)/rpmbuild/SOURCES/tuwunel-$$version.tar.gz" HEAD; \
	sed -i "s/^Version:.*/Version:        $$version/" '$(SPEC)'; \
	rpmbuild -bs '$(SPEC)' \
		--define '_sourcedir $(TOP)/rpmbuild/SOURCES' \
		--define '_srcrpmdir $(outdir)'
