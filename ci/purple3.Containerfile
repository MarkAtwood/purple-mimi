# SPDX-License-Identifier: GPL-3.0-or-later
# Debian 13 (trixie) with libpurple 3 built from the Pidgin 3 Alpha 4
# tarball (2.98.0) without the GTK user interface, for the libpurple 3
# plugin's build and smoke test. Debian packages no libpurple 3.
# GPlugin, birb, seagull and hasl come from the tarball's meson wraps,
# downloaded from fastly.imfreedom.org at build time.
FROM localhost/purple-mimi-debian13
ARG PIDGIN3_VERSION=2.98.0
ARG PIDGIN3_SHA256=62a5ece9dcf82650eba6a10d0804eeb04ca0e5f5c494e04f6bbc16036d704f30
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      meson ninja-build xz-utils gettext help2man \
      libjson-glib-dev libsoup-3.0-dev libsqlite3-dev libxml2-dev \
      libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
      libpango1.0-dev libidn2-dev libnice-dev libgnutls28-dev \
      libcmark-dev libglib2.0-dev-bin \
 && rm -rf /var/lib/apt/lists/*
RUN cd /tmp \
 && curl -sSfLO https://fastly.imfreedom.org/Pidgin3/${PIDGIN3_VERSION}/pidgin3-${PIDGIN3_VERSION}.tar.xz \
 && echo "${PIDGIN3_SHA256}  pidgin3-${PIDGIN3_VERSION}.tar.xz" | sha256sum -c - \
 && tar xf pidgin3-${PIDGIN3_VERSION}.tar.xz \
 && cd pidgin3-${PIDGIN3_VERSION} \
 && meson setup build --prefix=/usr/local --buildtype=debugoptimized \
      -Dgtkui=false -Dintrospection=false -Ddoc=false \
      -Ddemo=disabled -Dircv3=disabled -Dlink-local-messaging=disabled \
      -Dsip=disabled -Dsoju=disabled -Dxmpp=disabled -Dzulip=disabled \
      -Dlibsecret=disabled -Didle-xscreensaver=disabled \
      -Dgplugin:introspection=false -Dgplugin:doc=false -Dgplugin:help2man=false \
      -Dgplugin:lua=false -Dgplugin:python3=false -Dgplugin:vapi=false \
      -Dgplugin:install-gplugin-gtk4-viewer=false \
      -Dbirb:doc=false -Dbirb:introspection=false \
      -Dhasl:doc=false -Dhasl:introspection=false \
      -Dseagull:doc=false -Dseagull:introspection=false \
 && ninja -C build \
 && ninja -C build install \
 && ldconfig \
 && cd /tmp && rm -rf pidgin3-${PIDGIN3_VERSION}*
ENV PKG_CONFIG_PATH=/usr/local/lib/x86_64-linux-gnu/pkgconfig:/usr/local/lib/pkgconfig
