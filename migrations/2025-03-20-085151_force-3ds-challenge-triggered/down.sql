-- This file should undo anything in `up.sql`
ALTER TABLE payment_intent
DROP COLUMN IF EXISTS force_3ds_challenge_trigger;