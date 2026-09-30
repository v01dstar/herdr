FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a
RUN apt-get update && apt-get install -y --no-install-recommends bash git curl ca-certificates python3 procps ncurses-term libgcc-s1 && rm -rf /var/lib/apt/lists/*
COPY herdr /opt/herdr/herdr
COPY herdr-wrapper /usr/local/bin/herdr
COPY entrypoint.py /opt/herdr/entrypoint.py
RUN chmod 755 /opt/herdr/herdr /usr/local/bin/herdr && touch /opt/herdr/runtime-v1
ENV HOME=/data/home XDG_CONFIG_HOME=/data/config XDG_STATE_HOME=/data/state SHELL=/bin/bash TERM=xterm-256color
EXPOSE 8080
ENTRYPOINT ["python3", "/opt/herdr/entrypoint.py"]
