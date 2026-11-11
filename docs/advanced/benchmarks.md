# Benchmarks

These are historical upstream Oxyde measurements, retained for reference. They are not measurements of this fork or PostgreSQL 18. New fork benchmarks have not yet been published.

!!! info "Benchmark Date"
    These benchmarks were run on **March 11, 2026** with Oxyde 0.5.0.
    Results may vary depending on hardware and database configuration.

## Summary (average ops/sec)

### PostgreSQL

| Rank | ORM | Avg ops/sec |
|------|-----|-------------|
| 1 | Oxyde | 1,475 |
| 2 | Piccolo | 932 |
| 3 | Tortoise | 888 |
| 4 | Django | 736 |
| 5 | SQLAlchemy | 445 |
| 6 | SQLModel | 431 |
| 7 | Peewee | 80 |

## PostgreSQL Results

### CRUD Operations

![PostgreSQL CRUD](../img/benchmarks/postgresql_crud.png)

| Test | asyncpg | Oxyde | Django | SQLAlchemy | Tortoise | Piccolo | Peewee | SQLModel |
|------|---------|-------|--------|------------|----------|---------|--------|----------|
| insert_single | 769 | 664 | 701 | 531 | 735 | 700 | 143 | 537 |
| insert_bulk_100 | 444 | 354 | 252 | 185 | 261 | 201 | 100 | 126 |
| select_pk | 7,181 | 6,823 | 2,956 | 1,566 | 3,910 | 3,708 | 174 | 1,468 |
| select_filter | 829 | 308 | 153 | 162 | 143 | 132 | 93 | 138 |
| update_single | 747 | 756 | 659 | 526 | 674 | 723 | 144 | 528 |
| update_bulk | 657 | 707 | 586 | 501 | 640 | 673 | 142 | 501 |
| delete_single | 275 | 282 | 172 | 263 | 276 | 282 | 106 | 253 |

### Query Operations

![PostgreSQL Queries](../img/benchmarks/postgresql_queries.png)

| Test | asyncpg | Oxyde | Django | SQLAlchemy | Tortoise | Piccolo | Peewee | SQLModel |
|------|---------|-------|--------|------------|----------|---------|--------|----------|
| filter_simple | 5,731 | 5,017 | 2,829 | 1,449 | 3,524 | 3,491 | 170 | 1,428 |
| filter_complex | 1,489 | 599 | 281 | 310 | 280 | 263 | 122 | 267 |
| filter_in | 2,657 | 620 | 653 | 602 | 769 | 656 | 149 | 562 |
| order_limit | 4,882 | 4,363 | 2,284 | 1,347 | 2,051 | 2,688 | 169 | 1,296 |
| aggregate_count | 6,132 | 7,319 | 3,715 | 1,584 | 4,333 | 4,397 | 175 | 1,599 |
| aggregate_mixed | 4,990 | 5,374 | 1,871 | 1,388 | 2,629 | 3,421 | 173 | 1,383 |

### Relations

![PostgreSQL Relations](../img/benchmarks/postgresql_relations.png)

| Test | asyncpg | Oxyde | Django | SQLAlchemy | Tortoise | Piccolo | Peewee | SQLModel |
|------|---------|-------|--------|------------|----------|---------|--------|----------|
| join_simple | 22 | 9 | 3 | 5 | 4 | 2 | 4 | 4 |
| join_filter | 23 | 10 | 3 | 5 | 4 | 2 | 4 | 4 |
| prefetch_related | 33 | 4 | 4 | 4 | 6 | 5 | 5 | 3 |
| nested_prefetch | 25 | 3 | 1 | 2 | 3 | 3 | 4 | 2 |

### Concurrent Operations

![PostgreSQL Concurrent](../img/benchmarks/postgresql_concurrent.png)

| Concurrency | asyncpg | Oxyde | Django | SQLAlchemy | Tortoise | Piccolo | Peewee | SQLModel |
|-------------|---------|-------|--------|------------|----------|---------|--------|----------|
| 10 | 1,304 | 1,077 | 282 | 224 | 544 | 517 | 18 | 214 |
| 50 | 262 | 233 | 57 | 5 | 112 | 105 | 4 | 5 |
| 100 | 132 | 121 | 29 | 4 | 56 | 54 | 2 | 4 |

### Scalability

![PostgreSQL Scalability](../img/benchmarks/postgresql_scalability.png)

---

## Latency (PostgreSQL)

### Mean Latency (ms)

| Test | asyncpg | Oxyde | Django | SQLAlchemy | Tortoise | Piccolo | Peewee | SQLModel |
|------|---------|-------|--------|------------|----------|---------|--------|----------|
| insert_single | 1.300 | 1.505 | 1.426 | 1.883 | 1.361 | 1.428 | 6.970 | 1.861 |
| select_pk | 0.139 | 0.147 | 0.338 | 0.638 | 0.256 | 0.270 | 5.737 | 0.681 |
| update_single | 1.339 | 1.323 | 1.518 | 1.901 | 1.484 | 1.383 | 6.960 | 1.895 |

### P99 Latency (ms)

| Test | asyncpg | Oxyde | Django | SQLAlchemy | Tortoise | Piccolo | Peewee | SQLModel |
|------|---------|-------|--------|------------|----------|---------|--------|----------|
| insert_single | 1.367 | 6.959 | 1.660 | 3.314 | 1.637 | 1.750 | 10.516 | 2.380 |
| select_pk | 0.159 | 0.234 | 0.363 | 0.854 | 0.294 | 0.528 | 6.194 | 0.888 |
| update_single | 1.417 | 2.156 | 1.734 | 5.442 | 5.008 | 1.655 | 8.108 | 2.016 |

---

## Memory Usage (PostgreSQL)

Peak memory (MB):

| Test | asyncpg | Oxyde | Django | SQLAlchemy | Tortoise | Piccolo | Peewee | SQLModel |
|------|---------|-------|--------|------------|----------|---------|--------|----------|
| insert_single | 37.6 | 52.7 | 66.4 | 58.3 | 51.0 | 41.1 | 60.1 | 67.3 |
| select_pk | 43.6 | 57.6 | 72.5 | 64.0 | 56.1 | 47.0 | 64.3 | 69.6 |
| join_simple | 73.7 | 80.7 | 107.8 | 99.0 | 85.5 | 100.5 | 98.5 | 110.1 |
| nested_prefetch | 78.5 | 104.2 | 159.3 | 115.5 | 121.5 | 108.4 | 105.0 | 128.0 |

---

## Test Environment

| Parameter | Value |
|-----------|-------|
| CPU | Intel Core i7-11800H @ 2.30GHz |
| Cores | 2 |
| RAM | 4 GB |
| OS | Linux 6.17.0-14-generic |
| Python | 3.12.13 |
| Container | Docker |

### Package Versions

| Package | Version |
|---------|---------|
| oxyde | 0.5.0 |
| asyncpg | 0.31.0 |
| django | 6.0.3 |
| sqlalchemy | 2.0.48 |
| tortoise-orm | 1.1.6 |
| piccolo | 1.33.0 |
| peewee | 4.0.1 |
| sqlmodel | 0.0.37 |

### Test Data

- Users: 1,000
- Posts per user: 20

---

## Methodology

Benchmarks are available in a separate repository: [oxyde-benchmarks](https://github.com/mr-fatalyst/oxyde-benchmarks)

**Configuration:**

- 100 iterations per test
- 10 warmup iterations
- Connection pool warmed up before measurements
- Each ORM tested in isolated subprocess for accurate memory measurement
- Each ORM tested with recommended async drivers
- Raw driver baselines (asyncpg) included for reference

## Next Steps

- [Performance Tips](performance.md) — How to optimize your queries
- [Internals](internals.md) — Rust core architecture
