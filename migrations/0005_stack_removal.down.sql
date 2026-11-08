-- Reverts 0005_stack_removal. The removal marks are lost: a removed stack's row reads as an
-- ordinary stack again.
ALTER TABLE stacks DROP COLUMN removed_at;
