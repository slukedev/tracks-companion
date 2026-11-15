# ADR-0002: SQLite through rusqlite, one writer and a reader pool

**Status:** Accepted
**Date:** 2026-11-14
**Deciders:** Tracks maintainer

## Context

The server needs a durable store for a library of up to 500k songs and several users, with fast
reads, cheap writes, backups that are easy to reason about, and no extra service to run. Engine
already uses rusqlite with one writer and a few readers over WAL.

## Decision

SQLite via rusqlite 0.41 (bundled, which includes FTS5), WAL mode, one writer connection behind a mutex and a
pool of read-only connections, the same shape as `tracks-store`'s pool. Migrations with
`rusqlite_migration` on `user_version`. STRICT tables. Async handlers call `Db::read` and
`Db::write`, which run on tokio's blocking pool. Scheduled backups with `VACUUM INTO`.

rusqlite 0.41 rather than the newer 0.42 because `rusqlite_migration` 2.7 requires 0.41 and Engine
uses the same pair. Migration 1 as written in architecture section 5 was executed against
rusqlite 0.41's bundled SQLite (3.54.1) while writing this ADR, including FTS5 and the change-log
upsert.

## Options Considered

### Option A: rusqlite, single writer, reader pool
| Dimension | Assessment |
|---|---|
| Complexity | Low |
| Cost | No service; one file |
| Scalability | Plenty for one household or a small group; writes serialised |
| Team familiarity | High |

**Pros:** Fastest SQLite access in Rust; full feature set (FTS5, `VACUUM INTO`, backup API,
custom functions); static linking; same patterns as Engine.
**Cons:** Synchronous API needs `spawn_blocking`; one writer means long transactions must be
avoided.

### Option B: sqlx with SQLite
| Dimension | Assessment |
|---|---|
| Complexity | Medium |
| Cost | Macro build coupling |
| Scalability | Same limits |
| Team familiarity | Low |

**Pros:** Async API, compile-time checked queries.
**Cons:** Overhead per query, offline query metadata to maintain, less direct access to SQLite
features.

### Option C: PostgreSQL
| Dimension | Assessment |
|---|---|
| Complexity | High for self-hosters |
| Cost | A second service |
| Scalability | Concurrent writers |
| Team familiarity | Medium |

**Pros:** Concurrency.
**Cons:** Breaks "one binary, one service" for a load that does not need it.

## Trade-off Analysis

The workload is read-heavy with bursty writes (scans, listens). A single writer with short
transactions (at most 200 files or 250 ms per scan batch) keeps write latency for listens low,
and WAL lets readers proceed during writes. Operational simplicity outweighs concurrent writes.

## Consequences

- Easier: backups, restores, migrations, tests on in-memory or temp databases.
- Harder: the database must not live on NFS or SMB; the server refuses unless overridden.
- Revisit: if a deployment ever needs several server processes, which is a non-goal.

## Action Items

1. [ ] WP1 implements `Db` with the pragmas in architecture section 5.
2. [ ] WP1 writes migration 1 exactly as specified and a test that applies it to an empty file.
