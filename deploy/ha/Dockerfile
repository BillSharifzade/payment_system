# payment-patroni: Postgres 16 — the same base image (tag AND digest) as the
# single-node `postgres` service; bump them together — plus Patroni (pip, every
# file pinned by hash in requirements.txt) and pgBackRest (archive_command on
# whichever node leads). Used by the HA overlay for the database members and
# the one-shot etcd-auth job.
#
#   docker build -t payment-patroni:<tag> deploy/ha
#
# The build context is this directory only (configuration + scripts, no
# secrets). Plain FROM/RUN/COPY: builds with the legacy builder too.
FROM postgres:16.15@sha256:ca0bd484cb98bf4b24eb1010e73fb3fcbd6714d240fbc1a10eea5b7dbecb641d

COPY requirements.txt /tmp/requirements.txt
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 python3-venv pgbackrest \
    && python3 -m venv /opt/patroni \
    && /opt/patroni/bin/pip install --no-cache-dir --disable-pip-version-check --require-hashes -r /tmp/requirements.txt \
    && rm -rf /var/lib/apt/lists/* /tmp/requirements.txt /root/.cache \
    && rm -f /usr/local/bin/gosu \
    && mkdir -p /etc/patroni /run/patroni /var/lib/pgbackrest /var/log/pgbackrest \
    && chown postgres:postgres /run/patroni /var/lib/pgbackrest /var/log/pgbackrest \
    && /opt/patroni/bin/patroni --version && pgbackrest version

COPY render-config.py etcd-auth.py /usr/local/lib/ha/
COPY entrypoint.sh /usr/local/bin/ha-entrypoint
COPY post-bootstrap.sh /usr/local/bin/ha-post-bootstrap
COPY on-role-change.sh /usr/local/bin/ha-on-role-change
RUN chmod 0755 /usr/local/bin/ha-entrypoint /usr/local/bin/ha-post-bootstrap /usr/local/bin/ha-on-role-change

# patronictl (`docker compose exec pg-1 patronictl list`) reads the rendered file.
ENV PATH=/opt/patroni/bin:$PATH \
    PATRONICTL_CONFIG_FILE=/run/patroni/patroni.yml \
    PYTHONDONTWRITEBYTECODE=1
USER postgres
ENTRYPOINT ["/usr/local/bin/ha-entrypoint"]
