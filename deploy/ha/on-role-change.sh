#!/bin/bash
# Patroni postgresql.callbacks.on_role_change (installed as `ha-on-role-change`;
# Patroni passes: <action> <new role> <scope>). When this member has just been
# promoted, start a pgBackRest backup in the background: with archive_mode=on a
# leader that died before shipping its last completed WAL segment leaves a hole
# in the archive that point-in-time recovery from an older base backup cannot
# cross, so the new timeline gets a base of its own right away. Incremental
# (pgBackRest makes it full when there is none to build on). Its output goes to
# Patroni's log; a failure is never fatal (the schedule and the dumps go on).
#
# pgBackRest refuses to start until pg_control carries the new timeline, i.e.
# until a checkpoint has completed after the promotion: wait for the end of
# recovery, then force one.
[[ "${2:-}" == primary || "${2:-}" == master ]] || exit 0
echo "ha-on-role-change: promoted — starting a pgBackRest backup for the new timeline" >&2
# shellcheck disable=SC2016  # expanded by the background shell
setsid bash -c '
  q() { psql -X -qtA -U payment -d postgres -c "$1"; }
  for _ in $(seq 1 120); do [[ "$(q "SELECT pg_is_in_recovery()")" == f ]] && break; sleep 0.5; done
  q CHECKPOINT && exec pgbackrest --stanza=payment --type=incr --log-level-console=warn backup
' >&2 < /dev/null &
exit 0
