#!/usr/bin/env bash
# Start the development Postgres in Docker (idempotent).
#
# Listens on 127.0.0.1:5433 only (5432 is often taken by a system
# Postgres), data in the `jessie-pgdata` volume. The credentials are
# local development values, not secrets.
#
#   export DATABASE_URL=postgres://jessie:jessie@localhost:5433/jessie
set -euo pipefail

name=jessie-postgres
if [ -n "$(docker ps -q --filter "name=^${name}$")" ]; then
    echo "$name is already running"
elif [ -n "$(docker ps -aq --filter "name=^${name}$")" ]; then
    docker start "$name" >/dev/null
    echo "started existing $name"
else
    docker run -d --name "$name" \
        -e POSTGRES_USER=jessie -e POSTGRES_PASSWORD=jessie -e POSTGRES_DB=jessie \
        -p 127.0.0.1:5433:5432 -v jessie-pgdata:/var/lib/postgresql/data \
        --restart unless-stopped postgres:17 >/dev/null
    echo "created $name"
fi

# The first start initialises the database in a temporary server, then
# restarts it; wait until the real one answers queries.
for _ in $(seq 1 60); do
    if docker exec "$name" psql -U jessie -tAc "select 1" >/dev/null 2>&1; then
        echo "ready: postgres://jessie:jessie@localhost:5433/jessie"
        exit 0
    fi
    sleep 1
done
echo "Postgres did not become ready" >&2
exit 1
