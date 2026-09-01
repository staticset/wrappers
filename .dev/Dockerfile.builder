# Build/dev environment for mssql_fdw_rq.
# Mirrors upstream CI (.github/workflows/test_wrappers.yml):
#   Rust 1.88.0 + rustfmt/clippy, PostgreSQL 15 from PGDG, cargo-pgrx 0.16.1.
FROM rust:1.88.0

RUN rustup component add rustfmt clippy

RUN apt-get update \
 && apt-get install -y --no-install-recommends curl ca-certificates build-essential pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*

# PostgreSQL 15 from PGDG, as in upstream CI
RUN install -d /usr/share/postgresql-common/pgdg \
 && curl -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc --fail \
      https://www.postgresql.org/media/keys/ACCC4CF8.asc \
 && . /etc/os-release \
 && echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt $VERSION_CODENAME-pgdg main" \
      > /etc/apt/sources.list.d/pgdg.list \
 && apt-get update -y --fix-missing \
 && apt-get install -y --no-install-recommends postgresql-client-15 postgresql-15 postgresql-server-dev-15 \
 && apt-get autoremove -y && apt-get clean && rm -rf /var/lib/apt/lists/*

# pgrx refuses to run PostgreSQL as root: dev user + writable PG dirs (as upstream CI does)
RUN PG_BIN=/usr/lib/postgresql/15/bin \
 && chmod a+rwx "$("$PG_BIN/pg_config" --pkglibdir)" \
               "$("$PG_BIN/pg_config" --sharedir)"/extension \
               /var/run/postgresql /var/lib/postgresql \
 && useradd -m -u 1000 dev \
 && mkdir -p /opt/target

RUN cargo install --locked cargo-pgrx --version 0.16.1 \
 && chown -R dev:dev /opt/target /usr/local/cargo

USER dev
ENV HOME=/home/dev
ENV CARGO_TARGET_DIR=/opt/target
RUN cargo pgrx init --pg15 /usr/lib/postgresql/15/bin/pg_config

WORKDIR /work
CMD ["bash"]
