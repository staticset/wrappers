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
- **Поведение**: FDW только на чтение; JOIN-pushdown — для top-level запросов
  (SPI/PL-pgSQL-обёртки дают явную ошибку); set-операции исполняются по одному
  оператору на arm; `RESCAN`-планы отвергаются.
- **Мажорный апгрейд PostgreSQL** = пересборка расширения под новый мажор +
  установка в новый кластер (`pg_upgrade` подхватит файлы, если расширение
  стояло в старом).

## 7. Установка на другой сервер Сбер Навигатора (перенос готовой .so)

Сценарий: новый сервер Навигатора (Astra Linux, PostgreSQL, БД `navigator`),
**без компиляции** — переносим готовые артефакты с рабочего сервера и правим
словарь источников. Всё выполняется под `postgres` (psql) и root (файлы);
стоковый wildfly/Навигатор не трогаем.

### 7.0 Совместимость

`.so` привязана к мажору PostgreSQL и glibc целевой ОС. Перенос с
действующего прода (Astra, pg15) безопасен один-в-один. Если на новом
сервере другой мажор PG или существенно другой дистрибутив — собрать
артефакты в Docker по §3 (в контейнере `PG_MAJOR` = мажор целевого сервера)
и забрать tar.

### 7.1 Артефакты

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

### 7.2 Включение в БД navigator

```sql
-- функции обработчиков лягут в схему ext (у Навигатора стандартная схема
-- для расширений; DDL FDW ниже ссылается именно на неё)
CREATE EXTENSION wrappers WITH SCHEMA ext;

CREATE FOREIGN DATA WRAPPER mssql_wrapper
  HANDLER ext.mssql_fdw_rq_handler
  VALIDATOR ext.mssql_fdw_rq_validator;

-- серверы создаются шаблоном словаря от имени as_admin
GRANT USAGE ON FOREIGN DATA WRAPPER mssql_wrapper TO as_admin;

SELECT * FROM ext.mssql_fdw_rq_meta();   -- самопроверка: версия/автор
```

Рядом должен стоять штатный `tds_fdw` (у Навигатора обычно уже есть — через
него работает обычный пункт «MS SQL»): он нужен для видимости пункта меню
(§7.3) и не используется сам.

### 7.3 Привязка пункта меню к нашему расширению (словарь)

Навигатор управляет типами подключений словарём `data.tdicdatawrapper`
(nid, sname, sdatawrapper, joptions, screateserver, salterserver). Как это
работает и почему правится именно строка `nid=3`:

- фильтр меню (`arm.getdictionary_v40`): тип видим ⇔ `sdatawrapper` = имя
  **установленного расширения** (`pg_extension.extname`) И у одноимённого FDW
  в `fdwacl` есть `as_admin=U`. Наше расширение называется `wrappers`, а
  листинг таблиц (`arm.getforeigntableoptionlist_v40`) ветвится по
  `sdatawrapper` — ветки `wrappers`/`mssql_wrapper` там нет, отдельная строка
  была бы невидима/неработоспособна;
- имя сервера генерирует CASE по `nid` в `arm.setuserconnection_v40`
  (`nid=3` → `navigator_mssql_<id>`), неизвестный nid → NULL;
- ветка `tds_fdw` листинга создаёт временную foreign table с опциями
  `schema_name`/`table_name` и varchar-колонками с `column_name` — наш FDW
  всё это понимает (алиасы + varchar ⇒ нужны `2692f74`/`50d94d9`/`d3dfc52`,
  т.е. любая сборка новее этих коммитов).

⇒ `sdatawrapper` оставляем `tds_fdw`, `sname` оставляем `MS SQL`, а шаблоны
`screateserver`/`salterserver` подменяем на наш FDW `mssql_wrapper` (сервер
создаётся по нашим шаблонам, остальное работает штатно):

```sql
-- 0) бэкап строки
CREATE TABLE data.tdicdatawrapper_nid3_backup_20260905 AS
  SELECT * FROM data.tdicdatawrapper WHERE nid = 3;

-- 1) подмена шаблонов (dollar-quoting из-за кавычек в шаблонах).
--    ВАЖНО: sname НЕ трогаем — это функциональный ключ, по которому
--    ветвится vendor-код (напр. arm.setusersource_v40:
--    _sDB IN ('Clickhouse','PostgreSQL','MS SQL','MySQL','Oracle',...)).
--    Переименование («MS SQL (Rubicon)») молча пропускает создание
--    foreign-таблицы источника: каталог обновится, таблицы не будет.
UPDATE data.tdicdatawrapper SET
  screateserver = $ddl$CREATE SERVER [**sDBForeignName**]
  FOREIGN DATA WRAPPER mssql_wrapper
  OPTIONS (conn_string 'Server=[**sHost**],[**sPort**];Database=[**sDBName**];IntegratedSecurity=false;Encrypt=true;TrustServerCertificate=true'[**sOptions**]);
CREATE USER MAPPING FOR as_admin
  SERVER [**sDBForeignName**]
  OPTIONS (user '[**sLogin**]', password '[**sHash**]');$ddl$,
  salterserver = $ddl$ALTER SERVER [**sDBForeignName**]
  OPTIONS (SET conn_string 'Server=[**sHost**],[**sPort**];Database=[**sDBName**];IntegratedSecurity=false;Encrypt=true;TrustServerCertificate=true'[**sOptions**]);
ALTER USER MAPPING FOR as_admin
  SERVER [**sDBForeignName**]
  OPTIONS (SET user '[**sLogin**]', SET password '[**sHash**]');$ddl$
WHERE nid = 3;
```

- `joptions` не трогаем — это поля формы (хост/порт/БД/логин/пароль),
  плейсхолдеры `[**sHost**]`… подставляются из них; `[**sDBForeignName**]` —
  сгенерированное имя `navigator_mssql_<id>`;
- `Encrypt=true` — TLS обязателен; `TrustServerCertificate=true` держим для
  самоподписанных сертификатов MSSQL, с нормальным CA — убрать;
- рестарт wildfly не нужен — словарь читается на каждый запрос;
- при обновлении Навигатора строку словаря может перезалить вендорское
  обновление — после каждого обновления проверять и повторять шаг 1
  (бэкап-таблица из шага 0 хранит исходник).

### 7.4 Проверка

1. UI: в списке типов подключений есть «MS SQL» (ведёт к нашему FDW).
2. Создать подключение к MSSQL → появился сервер
   `navigator_mssql_<id>` с `conn_string` и `USER MAPPING FOR as_admin`:
   ```sql
   SELECT srvname, srvoptions FROM pg_foreign_server
     WHERE srvname LIKE 'navigator_mssql_%';
   ```
3. Экран выбора таблиц — напрямую (то же, что делает UI):
   ```sql
   SET ROLE as_admin;
   CALL arm.getforeigntableoptionlist_v40(
     '{"params":{"param":[{"name":"nID","value":"<id подключения>"}]}}'::json,
     NULL, <nuserid>);
   -- ожидание: JSON root.ForeignTableOptions.option[] со схемами и таблицами
   ```
4. Досоздать источник до конца и выполнить тестовый запрос из отчёта.

### 7.5 Диагностика

- Ошибки UI портала пишутся в `comm.paramslog_v30`
  (`nerrorstatus = true`, текст в `serrormsg`, `jparams` — параметры вызова;
  ID записи UI и показывает). Сообщения PostgreSQL приходят локализованными:
  «нет доступа к таблице X» = `permission denied for table X`.
- Тип «MS SQL (Rubicon)» не виден в меню ⇔ `sdatawrapper` строки nid=3 не
  совпадает с `pg_extension.extname` установленного расширения (`tds_fdw`)
  или у FDW `tds_fdw` нет `as_admin=U` в `fdwacl`
  (`SELECT fdwacl FROM pg_foreign_data_wrapper WHERE fdwname='tds_fdw';`).
- Портал выполняет процедуры под `SET ROLE as_admin`; **user mapping по
  членству роли не наследуется** (поэтому шаблон создаёт mapping именно для
  `as_admin`), а вот гранты наследуются — прав будет достаточно.
- `ext.wrappers_fdw_stats` не пишется с `c3d512c` намеренно: INSERT статистики
  шёл от имени вызывающей роли и при пустом ACL таблицы ронял сканы под
  `as_admin` («нет доступа к таблице», comm.paramslog nid=19598 от 05.09).
  Гранты на неё выдавать не нужно.
- T-SQL уходит/приходит: `ALTER SERVER … OPTIONS (SET log_remote_query 'true')`
  + `SET client_min_messages = LOG` (префикс `mssql_fdw_rq:`); на MSSQL —
  Extended Events по `client_app_name='wrappers'`.
