# Test-environment deploy target (runbook m4-test-environment, plan 14 §8 item 2).
# SYNTHETIC: one small "app server" for forge-deploy on this PC. sshd with a
# host key generated on first start (kept in a volume, pinned in forge), a
# `deploy` user whose only key is forge's, restricted to the dispatcher, and
# the synthetic app `hello-app` (busybox-extras httpd on 127.0.0.1:8080 inside the
# container: never published). Never a production pattern for the network:
# the real targets are the PFS Linux servers (ADR-0031).
FROM alpine:3.20@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc
RUN apk add --no-cache openssh-server git busybox-extras \
    && adduser -D -s /bin/sh deploy \
    && echo 'deploy:*' | chpasswd -e
COPY --chmod=0755 dispatch /opt/forge-deploy/dispatch
COPY --chmod=0755 hello-app /opt/forge-deploy/scripts/hello-app
COPY --chmod=0755 app-supervisor /opt/forge-deploy/app-supervisor
COPY --chmod=0755 entrypoint.sh /usr/local/bin/entrypoint.sh
COPY --chmod=0644 sshd_config /etc/ssh/sshd_config
EXPOSE 22
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
