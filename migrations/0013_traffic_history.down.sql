-- Reverts 0013_traffic_history. The traffic history goes with the tables (ADR-0062: acceptable
-- for a dashboard).
DROP TABLE traffic_hours;
DROP TABLE traffic_minutes;
