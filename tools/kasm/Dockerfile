# syntax=docker/dockerfile:1
# Kasm Workspaces image for the ryll GUI. Built by `make kasm-image`,
# which stages the .deb from `make deb` next to this file as ryll.deb.
#
# The base must be the same Debian release as the devcontainer that
# built the .deb: cargo-deb's $auto dependencies name that release's
# library packages (libasound2t64 and friends), and an older base
# cannot satisfy them. Everything else ryll needs at runtime --
# PulseAudio and its ALSA plugin for audio in the browser, Mesa for
# software OpenGL, libxkbcommon-x11, wmctrl -- is already in the base.
# jq reads the launch form answers in custom_startup.sh.
#
# The layout follows Kasm's own single-application images (for
# example kasmweb/remmina): set up the default profile as root, then
# hand the image back to the unprivileged kasm-user.
ARG KASM_BASE=kasmweb/core-debian-trixie:1.19.0
FROM ${KASM_BASE}
USER root

ENV HOME=/home/kasm-default-profile
ENV STARTUPDIR=/dockerstartup
ENV INST_SCRIPTS=$STARTUPDIR/install
WORKDIR $HOME

COPY ryll.deb /tmp/ryll.deb
RUN apt-get update \
    && apt-get install -y --no-install-recommends /tmp/ryll.deb jq \
    && rm -rf /var/lib/apt/lists/* /tmp/ryll.deb

# A single-application desktop: no panel, and the xfconf settings Kasm
# ships for single-application workspaces.
RUN apt-get remove -y xfce4-panel \
    && cp $HOME/.config/xfce4/xfconf/single-application-xfce-perchannel-xml/* \
        $HOME/.config/xfce4/xfconf/xfce-perchannel-xml/

COPY custom_startup.sh $STARTUPDIR/custom_startup.sh
RUN chmod 755 $STARTUPDIR/custom_startup.sh

RUN chown 1000:0 $HOME \
    && $STARTUPDIR/set_user_permission.sh $HOME

ENV HOME=/home/kasm-user
WORKDIR $HOME
RUN mkdir -p $HOME && chown -R 1000:0 $HOME

USER 1000
