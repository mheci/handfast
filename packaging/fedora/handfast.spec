# Fedora spec for Handfast.
#
# Binaries are prebuilt by CI (packaging/fedora packaging happens after the
# native build-linux job); this spec only stages them into the RPM layout.
# Version is filled in at package time: scripts/package-rpm.sh seds
# `Version:` from the release tag.

Name:           handfast
Version:        0.0.0
Release:        1
Summary:        Wayland-first KDE Connect-compatible device pairing daemon
License:        MIT
URL:            https://github.com/mheci/handfast
BuildArch:      x86_64
Requires:       glibc >= 2.34, libwayland-client >= 1.18, libxkbcommon, sqlite-libs
Recommends:     mesa-vulkan-drivers

%description
Connect your Android phone to your Linux desktop over your local network -
no cloud, no account. Handfast speaks the KDE Connect wire protocol and pairs
with the standard KDE Connect Android app. Ships a daemon (handfastd), a
terminal UI (hfctl) and a desktop app (handfast-gui).

%install
rm -rf %{buildroot}
install -Dm755 %{binroot}/usr/bin/handfastd    %{buildroot}/usr/bin/handfastd
install -Dm755 %{binroot}/usr/bin/hfctl        %{buildroot}/usr/bin/hfctl
install -Dm755 %{binroot}/usr/bin/handfast-gui %{buildroot}/usr/bin/handfast-gui
install -Dm644 %{binroot}/usr/lib/systemd/user/handfast.service %{buildroot}/usr/lib/systemd/user/handfast.service
install -Dm644 %{binroot}/usr/share/applications/dev.handfast.Gui.desktop %{buildroot}/usr/share/applications/dev.handfast.Gui.desktop
install -Dm644 %{binroot}/usr/share/icons/hicolor/scalable/apps/handfast.svg %{buildroot}/usr/share/icons/hicolor/scalable/apps/handfast.svg
install -Dm644 %{binroot}/usr/share/licenses/handfast/LICENSE-MIT %{buildroot}/usr/share/licenses/handfast/LICENSE-MIT
install -Dm644 %{binroot}/usr/share/bash-completion/completions/hfctl %{buildroot}/usr/share/bash-completion/completions/hfctl
install -Dm644 %{binroot}/usr/share/zsh/site-functions/_hfctl %{buildroot}/usr/share/zsh/site-functions/_hfctl
install -Dm644 %{binroot}/usr/share/fish/vendor_completions.d/hfctl.fish %{buildroot}/usr/share/fish/vendor_completions.d/hfctl.fish

%files
/usr/bin/handfastd
/usr/bin/hfctl
/usr/bin/handfast-gui
/usr/lib/systemd/user/handfast.service
/usr/share/applications/dev.handfast.Gui.desktop
/usr/share/icons/hicolor/scalable/apps/handfast.svg
/usr/share/licenses/handfast/LICENSE-MIT
/usr/share/bash-completion/completions/hfctl
/usr/share/zsh/site-functions/_hfctl
/usr/share/fish/vendor_completions.d/hfctl.fish

%changelog
* Sun Aug 30 2026 mheci <274998646+mheci@users.noreply.github.com> - 0.1.0-1
- Initial packaged release.
