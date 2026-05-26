# syntax=docker/dockerfile:1

ARG ACP_STACK_SYSTEMD_TEST_BASE_IMAGE=ubuntu:24.04
FROM ${ACP_STACK_SYSTEMD_TEST_BASE_IMAGE}

ENV container=docker

RUN apt-get update \
    && apt-get install --no-install-recommends -y \
      ca-certificates \
      systemd-sysv \
    && rm -rf /var/lib/apt/lists/*

STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
