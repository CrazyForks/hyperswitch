-- Your SQL goes here
ALTER TABLE refund ADD COLUMN IF NOT EXISTS platform_merchant_id VARCHAR(64) NULL;