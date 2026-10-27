%define version 1.1.0

Name:       gigabyte-dbus
Version:    %{version}
Release:    1%{?dist}
Summary:    D-Bus service for Gigabyte laptops

License:    MPL-2.0
URL:        https://github.com/mnetsiuk/gigabyte-dbus
Source:     %{url}/archive/refs/tags/v%{version}.tar.gz

BuildRequires:  rust >= 1.87
BuildRequires:  git
BuildRequires:  rust-std-static
BuildRequires:  pkgconfig(libzstd)
BuildRequires:  make

%if %{defined fedora}
%global debug_package %{nil}
%endif

%description
This package contains the D-Bus service needed to interact with the
aorus-laptop kernel module without root.

%prep
%autosetup

%build
make

%install
install -Dm755 ./target/release/gigabyted %{buildroot}%{_bindir}/gigabyted
install -Dm644 gigabyted.rules %{buildroot}%{_udevrulesdir}/gigabyted.rules
install -Dm644 gigabyted.service %{buildroot}%{_unitdir}/gigabyted.service
install -Dm644 gigabyted.conf %{buildroot}%{_datadir}/dbus-1/system.d/gigabyted.conf

%files
%{_bindir}/gigabyted
%{_udevrulesdir}/gigabyted.rules
%{_unitdir}/gigabyted.service
%{_datadir}/dbus-1/system.d/gigabyted.conf