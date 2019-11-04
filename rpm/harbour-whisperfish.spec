# 
# Do NOT Edit the Auto-generated Part!
# Generated by: spectacle version 0.27
# 

Name:       harbour-whisperfish

# >> macros
# << macros

Summary:    Signal client for SailfishOS
Version:    0.6.0
Release:    1
Group:      Qt/Qt
License:    GPLv3
URL:        https://github.com/rubdos/whisperfish/
Source0:    %{name}-%{version}.tar.bz2
Source100:  harbour-whisperfish.yaml
Requires:   sailfishsilica-qt5 >= 0.10.9
Requires:   nemo-qml-plugin-configuration-qt5
Requires:   nemo-qml-plugin-notifications-qt5
BuildRequires:  pkgconfig(sailfishapp) >= 1.0.2
BuildRequires:  pkgconfig(Qt5Core)
BuildRequires:  pkgconfig(Qt5Qml)
BuildRequires:  pkgconfig(Qt5Quick)
BuildRequires:  desktop-file-utils

%description
Private messaging using Signal for SailfishOS.

%prep
%setup -q -n %{name}-%{version}

# >> setup
# << setup

%build
# >> build pre
export RPM_VERSION=%{version}
source $HOME/.cargo/env
cargo build --release --manifest-path %{_sourcedir}/../Cargo.toml
# << build pre



# >> build post
# << build post

%install
rm -rf %{buildroot}
# >> install pre
# << install pre

# >> install post
install -m 755 target/release/whisperfish %{buildroot}/usr/bin/harbour-whisperfish
# << install post

desktop-file-install --delete-original       \
  --dir %{buildroot}%{_datadir}/applications             \
   %{buildroot}%{_datadir}/applications/*.desktop

%files
%defattr(-,root,root,-)
%{_bindir}
%{_datadir}/%{name}
%{_datadir}/applications/%{name}.desktop
%{_datadir}/icons/hicolor/*/apps/%{name}.png
# >> files
# << files
