# Backup sidecar image (payment-backup): pg_dump 16 + age + gnupg + rclone and
# the scripts in this directory, running as an unprivileged user (uid 65532,
# the same uid the app writes KYC documents as, so no capability is needed to
# read them). Used by the postgres-backup and kyc-backup services, by
# deploy.sh (pre-deploy dump, backup key generation) and by restore-drill.sh
# (age decryption).
#
#   docker build -t payment-backup:<tag> deploy/backup
#
# The build context is THIS directory only (scripts, no secrets). Legacy-
# builder compatible: plain FROM/RUN/COPY, no BuildKit-only syntax.
#
# pg_dump must be >= the server's major version: keep the tag on the same
# major as the `postgres` service (bump both together).
FROM postgres:16.15-alpine@sha256:721873c34ceb9f8d8fc265984940dc982404c105f19ad51be9fdc5970a6080ea

# gosu is only needed to drop root in the server entrypoint, which this image
# never runs; removing it also removes a Go binary that scanners flag often.
RUN apk add --no-cache bash age gnupg rclone tar coreutils findutils \
    && rm -f /usr/local/bin/gosu \
    && addgroup -S -g 65532 backup \
    && adduser -S -D -H -u 65532 -G backup -s /sbin/nologin backup \
    && mkdir -p /backups /metrics \
    && chown 65532:65532 /backups /metrics \
    && age --version && gpg --version | head -n1 && rclone version | head -n1 && pg_dump --version

COPY backup-lib.sh db-backup.sh kyc-backup.sh backup-loop.sh backup-healthcheck.sh /usr/local/bin/
RUN chmod 0755 /usr/local/bin/backup-*.sh /usr/local/bin/db-backup.sh /usr/local/bin/kyc-backup.sh

ENV HOME=/tmp \
    RCLONE_CACHE_DIR=/tmp/rclone-cache
USER 65532:65532
WORKDIR /tmp
ENTRYPOINT ["/usr/local/bin/backup-loop.sh"]
CMD ["db"]
