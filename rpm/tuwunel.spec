# The binary is built with the toolchain pinned by rust-toolchain.toml and
# stripped by the release profile, so no usable debuginfo is produced.
%global debug_package %{nil}

# Pass --without selinux for chroots lacking selinux-policy-devel.
%bcond_without selinux
%global selinuxtype targeted

Name:           tuwunel
Version:        1.9.0
Release:        1%{?dist}
Summary:        High performance Matrix homeserver written in Rust
License:        Apache-2.0
URL:            https://github.com/matrix-construct/tuwunel
Source0:        tuwunel-%{version}.tar.gz

BuildRequires:  ca-certificates
BuildRequires:  clang
BuildRequires:  clang-devel
BuildRequires:  cmake
BuildRequires:  curl
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  git-core
BuildRequires:  liburing-devel
BuildRequires:  make
BuildRequires:  pkgconf
BuildRequires:  (systemd-rpm-macros or systemd)
%if %{with selinux}
BuildRequires:  selinux-policy-devel
# The policy devel Makefile assembles interfaces with find; without it
# every interface call fails to expand.
BuildRequires:  findutils
%endif

Requires:       ca-certificates
Requires(pre):  shadow-utils
# Two homeservers of this lineage cannot share the database directory this
# package claims, and installing one over the other silently would leave the
# running server's data behind. Removing the other package first is the
# supported path, and its database is adopted on install.
Conflicts:      conduwuit
Conflicts:      matrix-conduit
%if %{with selinux}
Requires:       (%{name}-selinux if selinux-policy-%{selinuxtype})
%endif

%description
Tuwunel is a high performance, community driven Matrix homeserver
implemented in Rust.

%if %{with selinux}
%package selinux
Summary:        SELinux policy module for tuwunel
BuildArch:      noarch
%{?selinux_requires}

%description selinux
SELinux policy module providing the tuwunel_t confined domain and file
contexts for the tuwunel Matrix homeserver.
%endif

%prep
%autosetup -n tuwunel-%{version}

%build
%if %{with selinux}
# Built first so a policy error fails fast, ahead of the long cargo build.
(cd rpm/selinux && make -f %{_datadir}/selinux/devel/Makefile tuwunel.pp)
bzip2 -9 rpm/selinux/tuwunel.pp
%endif
# rpmbuild exports distribution build flags which break the build scripts
# of vendored C dependencies; the cargo release profile governs instead.
unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS RUSTFLAGS
# The distribution toolchain is generally older than the pin in
# rust-toolchain.toml, so the pinned toolchain is installed with rustup.
# This requires the COPR project setting "Enable internet access during
# builds" to be on.
export RUSTUP_HOME="%{_builddir}/rustup"
export CARGO_HOME="%{_builddir}/cargo"
channel="$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain "$channel"
export PATH="$CARGO_HOME/bin:$PATH"
# Overriding via the environment skips the extra components and cross
# targets listed in rust-toolchain.toml.
export RUSTUP_TOOLCHAIN="$channel"
cargo build --release --locked

%install
install -Dpm 0755 target/release/tuwunel %{buildroot}%{_sbindir}/tuwunel
install -Dpm 0640 tuwunel-example.toml %{buildroot}%{_sysconfdir}/tuwunel/tuwunel.toml
install -Dpm 0644 rpm/tuwunel.service %{buildroot}%{_unitdir}/tuwunel.service
install -Dpm 0644 rpm/tuwunel.socket %{buildroot}%{_unitdir}/tuwunel.socket
install -Dpm 0644 rpm/sysusers %{buildroot}%{_sysusersdir}/tuwunel.conf
install -Dpm 0755 rpm/adopt-legacy-database \
    %{buildroot}%{_libexecdir}/tuwunel/adopt-legacy-database
install -dm 0740 %{buildroot}%{_sharedstatedir}/tuwunel
%if %{with selinux}
install -Dpm 0644 rpm/selinux/tuwunel.pp.bz2 \
    %{buildroot}%{_datadir}/selinux/packages/%{selinuxtype}/tuwunel.pp.bz2
install -Dpm 0644 rpm/selinux/tuwunel.if \
    %{buildroot}%{_datadir}/selinux/devel/include/distributed/tuwunel.if
%endif

%pre
getent group tuwunel >/dev/null || groupadd --system tuwunel
getent passwd tuwunel >/dev/null || useradd --system --gid tuwunel \
    --home-dir %{_sharedstatedir}/tuwunel --shell /usr/sbin/nologin \
    --comment "tuwunel Matrix homeserver" tuwunel
exit 0

%post
%systemd_post tuwunel.service
# Adopt a predecessor package's database and leave the compatibility symlinks.
# Never raised: the package is installed and correct whether or not a previous
# database was found, and the command can be run again by hand.
%{_libexecdir}/tuwunel/adopt-legacy-database || :

%preun
# The socket is never preset in %%post, so socket activation stays opt-in; it is
# still disabled here for an operator who turned it on.
%systemd_preun tuwunel.service tuwunel.socket

%postun
%systemd_postun_with_restart tuwunel.service

%if %{with selinux}
%pre selinux
%selinux_relabel_pre -s %{selinuxtype}

%post selinux
%selinux_modules_install -s %{selinuxtype} %{_datadir}/selinux/packages/%{selinuxtype}/tuwunel.pp.bz2

%postun selinux
if [ $1 -eq 0 ]; then
    %selinux_modules_uninstall -s %{selinuxtype} tuwunel
fi

%posttrans selinux
%selinux_relabel_post -s %{selinuxtype}
%endif

%files
%license LICENSE
%doc README.md
%{_sbindir}/tuwunel
%dir %attr(0750, tuwunel, tuwunel) %{_sysconfdir}/tuwunel
%config(noreplace) %attr(0640, tuwunel, tuwunel) %{_sysconfdir}/tuwunel/tuwunel.toml
%{_unitdir}/tuwunel.service
%{_unitdir}/tuwunel.socket
%{_sysusersdir}/tuwunel.conf
%dir %{_libexecdir}/tuwunel
%{_libexecdir}/tuwunel/adopt-legacy-database
%dir %attr(0740, tuwunel, tuwunel) %{_sharedstatedir}/tuwunel

%if %{with selinux}
%files selinux
%{_datadir}/selinux/packages/%{selinuxtype}/tuwunel.pp.bz2
%{_datadir}/selinux/devel/include/distributed/tuwunel.if
%ghost %verify(not md5 size mode mtime) %{_sharedstatedir}/selinux/%{selinuxtype}/active/modules/200/tuwunel
%endif

%changelog
* Wed Jul 15 2026 June Strawberry <june@girlboss.ceo> - 1.8.1-1
- Initial spec for COPR builds
