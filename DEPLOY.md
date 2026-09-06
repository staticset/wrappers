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

Системного ничего не трогается; в БД создаётся схема расширения (на Навигаторе
— `ext`) с handler/validator-функциями и таблица `wrappers_fdw_stats`
(статистику в неё наш FDW **не пишет** — вызовы удалены, см. §4 и §7).

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

> Для сервера Сбер Навигатора установка сводится к переносу готовых
> артефактов и правке словаря — см. §7.

### Проверка после установки

```sql
SELECT extversion FROM pg_extension WHERE extname = 'wrappers';
SELECT * FROM mssql_fdw_rq_meta();
SELECT id FROM erp_orders WHERE amount > 1000 LIMIT 5;
EXPLAIN (VERBOSE) SELECT customer_id, count(*) FROM erp_orders
  GROUP BY customer_id;   -- Foreign Scan, без локальных Aggregate/Sort
```

`ext.wrappers_fdw_stats` остаётся пустой: с `c3d512c` FDW не пишет
статистику (INSERT от имени вызывающей роли ломал сканы Навигатора под
`as_admin` — пустой ACL таблицы; см. §7.5).

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
6. Контрольное окно наблюдения: лог PostgreSQL; при диагностике —
   `ALTER SERVER … OPTIONS (SET log_remote_query 'true')` и
   `SET client_min_messages = LOG` (T-SQL печатается с префиксом
   `mssql_fdw_rq:`); на стороне MSSQL — Extended Events по
   `client_app_name='wrappers'`.

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
- **Поведение**: FDW только на чтение; JOIN-pushdown — для top-level запросов.
  Под SPI/PL-pgSQL (виджеты Навигатора исполняются через CALL) remote-путь для
  join не строится — запрос исполняется локально по по-табличным сканам,
  без ошибок; полный пушдаун в Навигаторе достигается мостом (§7). RESCAN
  поддерживается: `re_scan` перезапускает запрос на новом соединении с теми же
  параметрами. Set-операции исполняются по одному оператору на arm.
- **Мажорный апгрейд PostgreSQL** = пересборка расширения под новый мажор +
  установка в новый кластер (`pg_upgrade` подхватит файлы, если расширение
  стояло в старом).

## 7. Сервер Сбер Навигатора: работа через мост (поддерживаемая топология)

Сценарий: сервер Навигатора (Astra Linux, PostgreSQL, БД `navigator`).
**Поддерживаемая топология — мост**: расширение работает в отдельной БД
(«мост», на проде — `test`), Навигатор подключается к ней штатным
источником типа «PostgreSQL». Словарь источников остаётся вендорским:
пункт «MS SQL» работает через штатный `tds_fdw`, наше расширение в БД
`navigator` не нужно.

```
Навигатор (wildfly) ──CALL──► БД navigator (postgres_fdw, srcext.*_ms)
                                 │ remote query: депарс из planner-дерева,
                                 │ работает под SPI/CALL
                                 ▼
                            БД test «мост» (wrappers, сервер mssql_vgu)
                                 │ один T-SQL: JOIN + фильтры + агрегация
                                 ▼
                            MS SQL (VGU)
```

Почему мост, а не прямой источник: виджет-запросы Навигатора исполняются
через `call ui.portal_arm_main_command(...)` (SPI), где наш фреймворк не
может депарсить join-дерево (собственный депарсер — план M3 — отклонён:
риски/сопровождение против дублирования того, что postgres_fdw уже делает).
postgres_fdw же депарсит запрос из planner-дерева независимо от контекста
вызова и пересылает на мост готовый top-level SELECT — там наш FDW
переводит его в один T-SQL. Проверено на проде 06.09: 5-табличный агрегат с
константными фильтрами уезжает целиком (sum + GROUP BY + ORDER BY).

### 7.0 Совместимость

`.so` привязана к мажору PostgreSQL и glibc целевой ОС. Перенос с
действующего прода (Astra, pg15) безопасен один-в-один. Если на новом
сервере другой мажор PG или существенно другой дистрибутив — собрать
артефакты в Docker по §3 (в контейнере `PG_MAJOR` = мажор целевого сервера)
и забрать tar.

### 7.1 Артефакты и лицензия

Три файла (на pg15 имена именно такие — control использует «versioned
shared-object mode», без `module_pathname`):

| Источник | Куда на новом сервере |
|---|---|
| `/usr/lib/postgresql/15/lib/wrappers-0.6.2.so` | `$(pg_config --pkglibdir)/` |
| `/usr/share/postgresql/15/extension/wrappers.control` | `$(pg_config --sharedir)/extension/` |
| `/usr/share/postgresql/15/extension/wrappers--0.6.2.sql` | `$(pg_config --sharedir)/extension/` |

```bash
# на источнике (действующий прод), забрать три файла:
sudo tar -czf /tmp/wrappers-artifacts.tgz \
  /usr/lib/postgresql/15/lib/wrappers-0.6.2.so \
  /usr/share/postgresql/15/extension/wrappers.control \
  /usr/share/postgresql/15/extension/wrappers--0.6.2.sql
# scp на новый сервер, затем:
sudo tar -xzf wrappers-artifacts.tgz -C / --keep-directory-symlink
sudo chmod 755 /usr/lib/postgresql/15/lib/wrappers-0.6.2.so
```

Рестарт кластера не нужен (новые подключения подхватят библиотеку при первом
`CREATE EXTENSION`/обращении к FDW).

Заодно перенесите лицензию портала, иначе wildfly каждую минуту пишет в лог
`License is not valid` + `Файл лицензии не найден`, а часть функций
отключена:

```bash
# ключ лежит в mssql-fdw-rq/Навигатор/*_license.key; целевой путь задан в
# standalone.xml свойством LICENSE_FILE_PATH = ${jboss.server.config.dir}/license.key
sudo install -o wildfly -g wildfly -m 640 license.key \
  /opt/wildfly/standalone/configuration/license.key
```

Рестарт wildfly не нужен: планировщик перечитывает ключ в течение минуты
(признак успеха — сообщения о лицензии исчезают из
`navigator-portal-server-debug-current.log`).

### 7.2 Настройка моста (БД test)

```sql
-- в БД-мосте (на проде — test; на чистой установке лучше отдельная БД)
CREATE EXTENSION wrappers;

CREATE FOREIGN DATA WRAPPER mssql_fdw_rq
  HANDLER mssql_fdw_rq_handler
  VALIDATOR mssql_fdw_rq_validator;

CREATE SERVER mssql_vgu FOREIGN DATA WRAPPER mssql_fdw_rq OPTIONS (
  conn_string 'Server=devsrv,1433;Database=VGU;Encrypt=true;TrustServerCertificate=true',
  log_remote_query 'true');            -- отладка; в бою выставить 'false'

CREATE USER MAPPING FOR postgres SERVER mssql_vgu
  OPTIONS (user '<mssql_login>', password '<mssql_password>');

-- таблицы моста: опции schema/table указывают на удалённые имена MSSQL
CREATE FOREIGN TABLE dbo.factipp (… ) SERVER mssql_vgu
  OPTIONS (schema 'dbo', table 'FactIPP');
CREATE FOREIGN TABLE dbo.dimcalendar (…) SERVER mssql_vgu
  OPTIONS (schema 'dbo', table 'DimCalendar');
-- … dimsotr / dimnorm / dimpodr / dimprojects
```

`Encrypt=true` — TLS обязателен; `TrustServerCertificate=true` — только при
самоподписанном сертификате MSSQL. Учётные данные — в user mapping, не в
conn_string.

### 7.3 Источник в Навигаторе

В UI Навигатора создайте источник типа **«PostgreSQL»**: хост/порт сервера
PG с мостом, БД моста. Навигатор создаст сервер `navigator_postgresql_<id>`
(postgres_fdw) и srcext-таблицки поверх схем моста (стандартный поток
вендора, ничего править не нужно). После создания — обязательный тюнинг:

```sql
-- в БД navigator, под postgres:
-- 1) remote-оценки + честные цены (мост — loopback; дефолт fdw_startup_cost=100
--    душит remote-агрегацию: все пути стоят одинаково и tie отдается локальному плану)
ALTER SERVER navigator_postgresql_<id>
  OPTIONS (ADD use_remote_estimate 'true',
           ADD fdw_startup_cost '10', ADD fdw_tuple_cost '0.005');
-- 2) статистика: без ANALYZE reltuples=-1 и планировщик слеп
ANALYZE srcext.ipp_ms_<id>;   -- все таблицы источника
```

### 7.4 Ограничения моста — важно для SQL виджетов

- **Локальные функции в фильтрах нешипуемы** (`tool.split([**param])` —
  SRF из БД navigator): postgres_fdw тянет join сырыми строками, фильтрует и
  группирует локально (корректно, но не оптимально). Рецепт полного пушдауна:
  в NavSQL подставлять параметры **константными списками**
  (`WHERE c.monthofyearid IN [**month]`) — такой запрос уезжает в MSSQL
  целиком одним T-SQL (проверено 06.09). Скалярные параметры
  (`yearid = [**year]`) шипуются всегда.
- Target-list выражения простых сканов (CASE/EXTRACT/NOW — например, «поле
  текущего года» в контролах) считаются локально у Навигатора — это норм.
- Контролы со строковыми литералами (кириллица «Не задано» и т.п.) шипуются
  (фиксы `555572e`); IN-списки по колонкам с алиасами — тоже.

### 7.5 Диагностика

- Ошибки UI портала пишутся в `comm.paramslog_v30`
  (`nerrorstatus = true`, текст в `serrormsg`, `jparams` — параметры вызова;
  ID записи UI и показывает). Сообщения PostgreSQL приходят локализованными:
  «нет доступа к таблице X» = `permission denied for table X`. Ошибки,
  перехваченные внутри vendor-процедур (`EXCEPTION WHEN OTHERS`), в paramslog
  **не попадают** — смотреть wildfly-лог
  (`/opt/wildfly/standalone/log/navigator-portal-server-debug-current.log`,
  там полный JSON запроса + текст ошибки).
- **T-SQL, реально ушедший в MSSQL**: на мосту `log_remote_query='true'` у
  сервера mssql_*; строки в `pg_log/postgresql-<День>.log`:
  `mssql_fdw_rq: remote query dispatched (N ms): <T-SQL>` (full-query) и
  `mssql_fdw_rq: remote query: <T-SQL>` (plain-скан). Читать дельту лога по
  офсету:
  ```sql
  SELECT (pg_stat_file('pg_log/postgresql-Sun.log')).size;         -- до
  -- …выполнить запрос (например, виджет в UI)…
  SELECT pg_read_file('pg_log/postgresql-Sun.log', <offset>, <new-offset - <offset>);
  ```
- `EXPLAIN (VERBOSE)` в БД navigator показывает `Remote SQL` (что postgres_fdw
  шлёт на мост); на мосту — `Remote query` нашего FDW (итоговый T-SQL ещё до
  исполнения).
- Портал выполняет процедуры под `SET ROLE as_admin`; user mapping по
  членству роли не наследуется — mapping должен существовать для роли,
  от которой работает подключение к мосту.
- `ext.wrappers_fdw_stats` не пишется с `c3d512c` намеренно (INSERT статистики
  от имени вызывающей роли ронял сканы под `as_admin` — пустой ACL таблицы).
  Гранты на неё выдавать не нужно.
- Словарь `data.tdicdatawrapper` — **вендорский, не подменять**: строка
  nid=3 (MS SQL/tds_fdw) должна остаться штатной. История: 04–05.09
  экспериментировали с подменой шаблонов на наш FDW (прямой путь), 06.09
  откачено из бэкапа `/tmp/tdicdatawrapper_backup_20260904.csv` (копия:
  `/home/administrator/tdic_orig.csv` на проде). Откат повторно:
  ```sql
  CREATE TEMP TABLE r (nid bigint, sname text, sdatawrapper text,
                       joptions text, screateserver text, salterserver text);
  \copy r FROM '/tmp/tdicdatawrapper_backup_20260904.csv' CSV
  UPDATE data.tdicdatawrapper t
  SET sname=r.sname, sdatawrapper=r.sdatawrapper, joptions=r.joptions::json,
      screateserver=r.screateserver, salterserver=r.salterserver
  FROM r WHERE t.nid=3 AND r.nid=3;
  ```
