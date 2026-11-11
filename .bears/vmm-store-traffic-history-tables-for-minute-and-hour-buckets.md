---
id: vmm
title: "Store: traffic history tables for minute and hour buckets"
status: done
priority: P1
created: "2026-11-11T09:08:20.548764628Z"
updated: "2026-11-11T09:45:42.591281563Z"
tags:
  - store
  - observability
depends_on:
  - mje
  - "5ae"
parent: t97
attempts: 1
---

## Summary

The persistence for traffic history (ADR-0062): a reversible migration with the minute and hour bucket tables, and the store methods to write a minute's rows, fold them into the hour rows, prune by retention and read buckets back for a time span.

## Acceptance Criteria

- [x] Migration created with `sqlx migrate add -r`; the `.down.sql` restores the previous schema exactly (the store's migration tests pass)
- [x] Two tables of one shape, as ADR-0062 specifies: bucket start, host, prefix, stack, service, a count per status class, upstream errors, latency histogram blob. No foreign key to stacks. A primary key that makes writing a bucket twice an upsert, and an index that serves "this stack over a span" and "everything over a span"
- [x] One store method writes a minute's rows and adds them to their hour rows in a single transaction (counts added, histograms merged), so a crash never leaves the two tables disagreeing
- [x] Prune methods delete minute rows and hour rows older than a cutoff and return how many
- [x] Read methods return the buckets of a span from either table, optionally for one stack, in bucket order
- [x] The store does not interpret the histogram beyond what merging needs; the row type uses the `stats` histogram type from the previous task or plain bytes, whichever keeps `store` free of a dependency cycle

## Implementation Notes

- Follow the events table for style: `src/store/` (migration `0008`/events queries, `prune_events`), `Timestamp` for bucket starts.
- Merging blobs cannot be done in SQL: read the affected hour rows, merge, upsert, inside the transaction. At most one hour row per active route per flush.
- Batch the inserts; a flush is tens of rows.

## Testing

- Write, re-write (upsert) and read back; hour rows equal the sum of their minutes; prune keeps the boundary bucket correctly; a removed stack's rows stay.
- Migration up/down round trip (existing store test harness).

References: ADR-0062, ADR-0022, ADR-0052 (reversible migrations); `src/store/`, `migrations/`.