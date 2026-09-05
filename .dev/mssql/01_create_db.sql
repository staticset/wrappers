-- Test database for mssql_fdw_rq e2e tests (TZ §7.2):
-- "orders / customers / products", FKs, every type supported by v1 (TZ §5.5).
-- Idempotent: safe to re-run.

IF DB_ID(N'rqtest') IS NULL
    CREATE DATABASE rqtest;
GO
USE rqtest;
GO
DROP TABLE IF EXISTS dbo.shipments, dbo.payments, dbo.order_items, dbo.orders, dbo.products, dbo.customers;
GO

CREATE TABLE dbo.customers (
    id            uniqueidentifier NOT NULL CONSTRAINT pk_customers PRIMARY KEY,
    code          int              NOT NULL UNIQUE,          -- deterministic join helper
    name          nvarchar(120)    NOT NULL,
    tier          varchar(12)      NOT NULL,
    credit_limit  decimal(18,2)    NOT NULL,
    active        bit              NOT NULL,
    registered_on date             NOT NULL,
    created_at    datetime2(3)     NOT NULL
);

CREATE TABLE dbo.products (
    id         int IDENTITY(1,1)  NOT NULL CONSTRAINT pk_products PRIMARY KEY,
    name       nvarchar(120)      NOT NULL,
    category   varchar(20)        NOT NULL,
    price      decimal(18,2)      NOT NULL,
    weight_kg  real               NOT NULL,
    rating     float              NOT NULL,
    ean13      varbinary(13)      NOT NULL,
    in_stock   bit                NOT NULL,
    updated_at smalldatetime      NOT NULL
);

CREATE TABLE dbo.orders (
    id           bigint IDENTITY(1,1) NOT NULL CONSTRAINT pk_orders PRIMARY KEY,
    customer_id  uniqueidentifier NOT NULL CONSTRAINT fk_orders_customers REFERENCES dbo.customers (id),
    status       varchar(16)      NOT NULL,
    total_amount money            NOT NULL,
    shipping_fee smallmoney       NOT NULL,
    order_date   date             NOT NULL,
    placed_at    datetime2(0)     NOT NULL,
    shipped_at   datetimeoffset(0) NULL
);

CREATE TABLE dbo.order_items (
    id           bigint IDENTITY(1,1) NOT NULL CONSTRAINT pk_order_items PRIMARY KEY,
    order_id     bigint           NOT NULL CONSTRAINT fk_items_orders   REFERENCES dbo.orders (id),
    product_id   int              NOT NULL CONSTRAINT fk_items_products REFERENCES dbo.products (id),
    qty          smallint         NOT NULL,
    unit_price   decimal(18,2)    NOT NULL,
    discount_pct tinyint          NOT NULL
);

CREATE TABLE dbo.payments (
    id       bigint IDENTITY(1,1) NOT NULL CONSTRAINT pk_payments PRIMARY KEY,
    order_id bigint           NOT NULL CONSTRAINT fk_payments_orders REFERENCES dbo.orders (id),
    method   varchar(16)      NOT NULL,
    amount   smallmoney       NOT NULL,
    paid_on  date             NOT NULL,
    paid_at  time(0)          NOT NULL
);

CREATE TABLE dbo.shipments (
    id         bigint IDENTITY(1,1) NOT NULL CONSTRAINT pk_shipments PRIMARY KEY,
    order_id   bigint           NOT NULL CONSTRAINT fk_shipments_orders REFERENCES dbo.orders (id),
    carrier    varchar(24)      NOT NULL,
    track_code char(12)         NOT NULL,
    shipped_on datetime         NOT NULL,
    delivered  bit              NOT NULL
);
GO

-- Deterministic seed driven by a number sequence: 25 customers, 20 products,
-- 60 orders, 2 items per order, 1 payment per order, shipments for 40 orders.
;WITH nums AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM nums WHERE n < 25)
INSERT INTO dbo.customers (id, code, name, tier, credit_limit, active, registered_on, created_at)
SELECT NEWID(), n, N'Customer ' + RIGHT('00' + CAST(n AS nvarchar(2)), 2),
       CHOOSE(n % 3 + 1, 'basic', 'silver', 'gold'),
       CAST(1000 + n * 250 AS decimal(18,2)),
       CASE WHEN n % 7 = 0 THEN 0 ELSE 1 END,
       DATEFROMPARTS(2024, 1 + n % 12, 1 + n % 28),
       DATETIME2FROMPARTS(2025, 1 + n % 12, 1 + n % 28, 10, n % 60, 0, 0, 0)
FROM nums
OPTION (MAXRECURSION 100);
GO
;WITH nums AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM nums WHERE n < 20)
INSERT INTO dbo.products (name, category, price, weight_kg, rating, ean13, in_stock, updated_at)
SELECT N'Product ' + RIGHT('00' + CAST(n AS nvarchar(2)), 2),
       CHOOSE(n % 4 + 1, 'tools', 'toys', 'food', 'tech'),
       CAST(9.5 + n * 3.25 AS decimal(18,2)),
       CAST(0.25 + n * 0.11 AS real),
       CAST(3 + (n % 20) * 0.1 AS float),
       CONVERT(varbinary(13), RIGHT('460000000000' + CAST(n AS varchar(2)), 13)),
       CASE WHEN n % 9 = 0 THEN 0 ELSE 1 END,
       CAST(CONVERT(varchar(16), DATEADD(day, -n, GETDATE()), 112) + ' 08:30' AS smalldatetime)
FROM nums
OPTION (MAXRECURSION 100);
GO
;WITH nums AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM nums WHERE n < 60)
INSERT INTO dbo.orders (customer_id, status, total_amount, shipping_fee, order_date, placed_at, shipped_at)
SELECT (SELECT id FROM dbo.customers WHERE code = n % 25 + 1),
       CHOOSE(n % 4 + 1, 'new', 'paid', 'shipped', 'done'),
       CAST(120 + n * 37 AS money),
       CAST(4.5 + n % 10 AS smallmoney),
       DATEFROMPARTS(2026, 1 + n % 9, 1 + n % 28),
       DATETIME2FROMPARTS(2026, 1 + n % 9, 1 + n % 28, 9 + n % 12, n % 60, 0, 0, 0),
       CASE WHEN n % 3 <> 0 THEN DATEADD(hour, n, SYSDATETIMEOFFSET()) END
FROM nums
OPTION (MAXRECURSION 100);
GO
;WITH nums AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM nums WHERE n < 120)
INSERT INTO dbo.order_items (order_id, product_id, qty, unit_price, discount_pct)
SELECT (n + 1) / 2,
       n % 20 + 1,
       CAST(1 + n % 5 AS smallint),
       (SELECT price FROM dbo.products WHERE id = n % 20 + 1),
       CAST(n % 15 AS tinyint)
FROM nums
OPTION (MAXRECURSION 200);
GO
;WITH nums AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM nums WHERE n < 60)
INSERT INTO dbo.payments (order_id, method, amount, paid_on, paid_at)
SELECT n,
       CHOOSE(n % 3 + 1, 'card', 'cash', 'transfer'),
       CAST(50 + n * 11 AS smallmoney),
       DATEADD(day, 1, (SELECT order_date FROM dbo.orders WHERE id = n)),
       TIMEFROMPARTS(9 + n % 10, n % 60, 0, 0, 0)
FROM nums
OPTION (MAXRECURSION 100);
GO
;WITH nums AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM nums WHERE n < 41)
INSERT INTO dbo.shipments (order_id, carrier, track_code, shipped_on, delivered)
SELECT n,
       CHOOSE(n % 3 + 1, 'DHL', 'UPS', 'Post'),
       RIGHT('RU000000000' + CAST(n AS varchar(2)), 12),
       DATEADD(day, n % 5, CONVERT(datetime, (SELECT order_date FROM dbo.orders WHERE id = n))),
       CASE WHEN n % 4 = 0 THEN 0 ELSE 1 END
FROM nums
OPTION (MAXRECURSION 100);
GO

PRINT 'rqtest: schema + seed done';
SELECT (SELECT COUNT(*) FROM dbo.customers)     AS customers,
       (SELECT COUNT(*) FROM dbo.products)      AS products,
       (SELECT COUNT(*) FROM dbo.orders)        AS orders,
       (SELECT COUNT(*) FROM dbo.order_items)   AS order_items,
       (SELECT COUNT(*) FROM dbo.payments)      AS payments,
       (SELECT COUNT(*) FROM dbo.shipments)     AS shipments;
GO

-- Mixed-case identifiers: sources created through Sber Navigator keep the
-- remote spelling ("DimCalendar"), so every pushdown path must survive
-- quoted names end to end.
DROP TABLE IF EXISTS dbo.[DimTest];
GO
CREATE TABLE dbo.[DimTest] (
    [MonthId] int           NOT NULL,
    [Val]     decimal(18,2) NULL
);
GO
INSERT INTO dbo.[DimTest] ([MonthId], [Val]) VALUES
    (202601, 100.50), (202601, NULL),
    (202602, 200.75), (202602, 50.25),
    (202603, NULL),   (202603, 300.00);
GO
