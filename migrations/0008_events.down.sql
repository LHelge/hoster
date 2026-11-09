-- Reverts 0008_events. The event history goes with the table (ADR-0055: acceptable for a
-- 30-day feed).
DROP TABLE events;
