-- Backend-local compare-and-set winner token for review verdict mutations.
ALTER TABLE capability_capsules
    ADD COLUMN IF NOT EXISTS review_token String DEFAULT '' AFTER expires_at;
