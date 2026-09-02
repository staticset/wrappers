# DEPLOY.md — установка и обновление mssql_fdw_rq в продакшене

Расширение называется `wrappers` (общая обёртка фреймворка); FDW `mssql_fdw_rq`
включается cargo-фичей. Поддерживаемые PostgreSQL: **15, 16, 17** (CI гоняет 15
и 17). Сборка — только Linux. Исходник: ветка `feat/mssql-fdw-rq`.

## 1. Что оказывается на сервере

| Файл | Куда | Что это |
|---|---|---|
| `wrappers.so` | `$(pg_config --pkglibdir)` | библиотека (все собранные FDW) |
| `wrappers.control` | `$(pg_config --sharedir)/extension` | манифест расширения |
| `wrappers--<версия>.sql` | `$(pg_config --sharedir)/extension` | SQL-объекты (функции-обработчики) |

Системного ничего не трогается; в БД создаётся схема `wrappers` и таблица
статистики `wrappers_fdw_stats`.

## 2. Вариант A — сборка на сервере

Debian/Ubuntu (`PG_MAJOR` — ваша мажорная версия, например `16`):

```bash
# зависимости
sudo apt-get update
sudo apt-get install -y build-essential pkg-config curl git \
  libssl-dev postgresql-server-dev-${PG_MAJOR}
# только при использовании Kerberos (auth='kerberos'):
sudo apt-get install -y libkrb5-dev

# Rust ровно под репозиторий (workspace rust-version = 1.88.0)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.88.0
source "$HOME/.cargo/env"
cargo install --locked cargo-pgrx --version 0.16.1

# исходники
git clone https://github.com/staticset/wrappers.git
cd wrappers && git checkout feat/mssql-fdw-rq

# регистрация pg_config в pgrx и установка
cargo pgrx init --pg${PG_MAJOR} /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config
cd wrappers
cargo pgrx install --no-default-features \
  --features "mssql_fdw_rq pg${PG_MAJOR}" \
  --pg-config /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config
# с Kerberos: --features "mssql_fdw_rq_kerberos pg${PG_MAJOR}"
```

`cargo pgrx install` кладёт файлы из §1 и **не требует рестарта кластера**.

## 3. Вариант B — сборка в Docker, на прод только артефакты

Если на проде не должно быть Rust-тулчейна — собирайте в контейнере на любой
машине с Docker. **Собирайте под тот же мажор PostgreSQL и максимально близкую
к проду ОС/архитектуру** (`.so` не переносим между мажорами и между сильно
разными glibc).

```bash
git clone https://github.com/staticset/wrappers.git
cd wrappers && git checkout feat/mssql-fdw-rq

docker run --rm -v "$PWD":/work -w /work \
  rust:1.88.0 bash -exc '
    export DEBIAN_FRONTEND=noninteractive PG_MAJOR=16
    apt-get update -qq && apt-get install -y -qq \
      build-essential pkg-config libssl-dev postgresql-server-dev-${PG_MAJOR} libkrb5-dev
    cargo install --locked cargo-pgrx --version 0.16.1
    cd wrappers
    cargo pgrx init --pg${PG_MAJOR} /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config
    cargo pgrx install --no-default-features \
      --features "mssql_fdw_rq_kerberos pg${PG_MAJOR}" \
      --pg-config /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config
    PKG=$( /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config --pkglibdir )
    EXT=$( /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config --sharedir )/extension
    V=$( grep -m1 ^version Cargo.toml | cut -d\" -f2 )
    mkdir -p /work/dist
    tar -C / -cf /work/dist/wrappers-pg${PG_MAJOR}.tar \
      "$PKG/wrappers.so" "$EXT/wrappers.control" "$EXT/wrappers--${V}.sql"
  '
# результат: dist/wrappers-pg16.tar
```

На сервере:

```bash
# сначала бэкап текущих артефактов (см. §5), затем:
tar -xpf wrappers-pg16.tar -C / --keep-directory-symlink
# либо разложите три файла вручную по каталогам из §1
chmod 755 "$(pg_config --pkglibdir)/wrappers.so"
```

## 4. Включение в БД (один раз на каждую БД)

```sql
CREATE EXTENSION wrappers;

CREATE FOREIGN DATA WRAPPER mssql_fdw_rq
  HANDLER mssql_fdw_rq_handler
  VALIDATOR mssql_fdw_rq_validator;

-- ПРОД TLS: encrypt=true (обязательный TLS). TrustServerCertificate=true —
-- только при самоподписанном сертификате MSSQL; с нормальным CA не указывать.
CREATE SERVER mssql_srv
  FOREIGN DATA WRAPPER mssql_fdw_rq
  OPTIONS (
    conn_string 'Server=sqlprod.example.com,1433;Database=ERP;encrypt=true;TrustServerCertificate=true',
    log_remote_query 'false'
  );

-- учётные данные — через user mapping (не в conn_string)
CREATE USER MAPPING FOR app_reader
  SERVER mssql_srv
  OPTIONS (user 'svc_pgbridge', password '...');
-- Supabase: OPTIONS (user '...', password_id '<vault-secret-id>')

-- foreign-таблицы: объявляйте NOT NULL у ключевых колонок — это разблокирует
-- оконные сортировки и убирает CASE-обёртки в ORDER BY
CREATE FOREIGN TABLE erp_orders (
  id          bigint NOT NULL,
  customer_id uuid,
  amount      numeric(18,2),
  created_at  timestamp
)
  SERVER mssql_srv
  OPTIONS (schema 'dbo', table 'Orders');

GRANT USAGE ON FOREIGN SERVER mssql_srv TO app_reader;
GRANT SELECT ON erp_orders TO app_reader;
```

Kerberos: сервер с опцией `auth 'kerberos'` (без user mapping), расширение
собрано с `mssql_fdw_rq_kerberos`, бэкенд работает под доменной УЗ с TGT —
чек-лист в `wrappers/src/fdw/mssql_fdw_rq/README.md`.

### Проверка после установки

```sql
SELECT extversion FROM pg_extension WHERE extname = 'wrappers';
SELECT * FROM mssql_fdw_rq_meta();
SELECT id FROM erp_orders WHERE amount > 1000 LIMIT 5;
EXPLAIN (VERBOSE) SELECT customer_id, count(*) FROM erp_orders
  GROUP BY customer_id;   -- Foreign Scan, без локальных Aggregate/Sort
SELECT * FROM wrappers_fdw_stats WHERE fdw_name = 'MssqlFdwRq';
```

## 5. Обновление

Различайте два случая: менялся ли только Rust-код или также **SQL-схема
расширения** (новые функции, bump `version` в `wrappers/Cargo.toml`).

### 5.1 Только код (багфиксы FDW), версия не менялась

```bash
cd /path/to/wrappers && git fetch && git checkout feat/mssql-fdw-rq && git pull

# бэкап старой библиотеки для откката
PKGLIB=$(pg_config --pkglibdir)
cp "$PKGLIB/wrappers.so" "$PKGLIB/wrappers.so.bak-$(date +%Y%m%d)"

cd wrappers
cargo pgrx install --no-default-features \
  --features "mssql_fdw_rq pg${PG_MAJOR}" \
  --pg-config /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config
```

- Рестарт кластера не нужен, но **уже открытые подключения продолжат работать
  со старой версией `.so`** — новые подключения получат новую.
- Чистая раскатка: терминация бэкендов приложения в согласованное окно
  (`pg_terminate_backend` по `application_name`) или rolling через pgbouncer.

### 5.2 Смена версии SQL-схемы

Например `0.6.2` → `0.7.0`:

```bash
cargo pgrx install ...   # появятся wrappers--0.7.0.sql и .control с новой версией
EXTDIR=$(pg_config --sharedir)/extension
# upgrade-скрипт OLD→NEW (аналогично upstream CI):
cp "$EXTDIR/wrappers--0.7.0.sql" "$EXTDIR/wrappers--0.6.2--0.7.0.sql"
```

```sql
ALTER EXTENSION wrappers UPDATE TO '0.7.0';   -- в каждой БД с расширением
```

Серверы, user mappings и foreign-таблицы UPDATE не затрагивает.

### 5.3 Откат

```bash
# 5.1: вернуть .so
cp "$PKGLIB/wrappers.so.bak-YYYYMMDD" "$PKGLIB/wrappers.so"
# + терминация/рестарт пользовательских сессий
# 5.2: при наличии старых SQL-файлов
```

```sql
ALTER EXTENSION wrappers UPDATE TO '0.6.2';
```

### 5.4 Чек-лист релиза

1. Коммит сборки зафиксирован тегом (`git tag prod-0.6.2-r1 && git push --tags`).
2. CI `mssql_fdw_rq` на этом коммите зелёный (pg15+pg17).
3. Снят бэкап старых артефактов.
4. Smoke-запросы и `EXPLAIN` из §4 — сначала staging, потом прод.
5. В согласованное окно — обновление пользовательских сессий.
6. Контрольное окно наблюдения: `wrappers_fdw_stats`; при диагностике —
   `ALTER SERVER … OPTIONS (SET log_remote_query 'true')` и
   `SET client_min_messages = LOG`.

## 6. Эксплуатационные заметки

- **Бэкапы**: `pg_dump` сохраняет DDL foreign-объектов, но не данные и не
  пароли из user mapping — держите их в секрет-хранилище отдельно.
  Материализованные представления поверх foreign-таблиц могут ломать
  восстановление бэкапа (наследие upstream) — проверяйте restore.
- **Пароли**: `pg_user_mapping_options` читается только суперпользователями;
  значения никогда не попадают в сообщения об ошибках FDW.
- **Соединения**: одно TCP-соединение на запрос (как в upstream `mssql_fdw`) —
  закладывайте в лимиты MSSQL: сессии PG-бэкендов × число серверов. Пул
  per-server — плановая доработка.
- **Поведение**: FDW только на чтение; JOIN-pushdown — для top-level запросов
  (SPI/PL-pgSQL-обёртки дают явную ошибку); set-операции исполняются по одному
  оператору на arm; `RESCAN`-планы отвергаются.
- **Мажорный апгрейд PostgreSQL** = пересборка расширения под новый мажор +
  установка в новый кластер (`pg_upgrade` подхватит файлы, если расширение
  стояло в старом).
