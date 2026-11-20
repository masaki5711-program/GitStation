# The image of the demo app hello-app on the PFS trial server (built by
# /opt/forge-deploy/scripts/hello-app as gs-demo/hello-app:<sha>). FIXED in
# this kit: the build context is only the commit's public/ (static files),
# so nothing in the demo repository decides what is built or run.
# Installed as /opt/forge-deploy/hello-app/Dockerfile by setup-target.sh.
# Same pinned base as ops/test-env/deploy-target (alpine 3.20). The
# busybox-extras layer is cached by Docker after the first build.
FROM docker.io/library/alpine:3.20@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc
RUN apk add --no-cache busybox-extras \
    && adduser -D -H -u 10080 -s /sbin/nologin web
COPY --chown=root:root public/ /srv/www/
USER 10080
EXPOSE 8080
CMD ["busybox-extras", "httpd", "-f", "-p", "8080", "-h", "/srv/www"]
