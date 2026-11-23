-- Claude session switching and its prompt-cache cost (plan W7).
--
-- Read-only. Run against the gateway's request history:
--   sqlite3 -readonly ~/.mahoquot/auth/request-history.sqlite \
--     -cmd ".parameter set :start_ms 1761264000000" \
--     -cmd ".parameter set :end_ms   1761868800000" \
--     < scripts/claude_switch_cache.sql
--
-- :start_ms / :end_ms bound occurred_at_ms (half-open). Compare the 7 days
-- before the session-pinning deploy with the 7 days after it.
--
-- Within each session_identifier, successful Claude turns are ordered by
-- occurred_at_ms, then id. A turn is a "switch" when the previous turn ran on a
-- different account, "same" when it ran on the same one, and "first" for the
-- opening turn. A switch re-writes the prompt cache on the new account, which
-- shows up as a higher cache-write share.

.headers on
.mode column

WITH turns AS (
    SELECT
        id,
        session_identifier,
        account_identifier,
        cache_write_tokens,
        cached_input_tokens,
        LAG(account_identifier) OVER (
            PARTITION BY session_identifier
            ORDER BY occurred_at_ms, id
        ) AS previous_account
    FROM usage_events
    WHERE provider = 'claude'
      AND succeeded = 1
      AND session_identifier IS NOT NULL
      AND occurred_at_ms >= :start_ms
      AND occurred_at_ms < :end_ms
),
classified AS (
    SELECT
        CASE
            WHEN previous_account IS NULL THEN 'first'
            WHEN previous_account = account_identifier THEN 'same'
            ELSE 'switch'
        END AS class,
        cache_write_tokens,
        cached_input_tokens
    FROM turns
)
SELECT
    class,
    COUNT(*) AS turns,
    SUM(cache_write_tokens) AS cache_write_tokens,
    SUM(cached_input_tokens) AS cache_read_tokens,
    printf('%.3f',
        CAST(SUM(cache_write_tokens) AS REAL)
        / NULLIF(SUM(cache_write_tokens) + SUM(cached_input_tokens), 0)
    ) AS write_share
FROM classified
GROUP BY class
ORDER BY CASE class WHEN 'first' THEN 0 WHEN 'same' THEN 1 ELSE 2 END;

-- Share of follow-on turns that switched account.
SELECT
    printf('%.3f',
        CAST(SUM(previous_account <> account_identifier) AS REAL)
        / NULLIF(SUM(previous_account IS NOT NULL), 0)
    ) AS follow_on_switch_share
FROM (
    SELECT
        account_identifier,
        LAG(account_identifier) OVER (
            PARTITION BY session_identifier
            ORDER BY occurred_at_ms, id
        ) AS previous_account
    FROM usage_events
    WHERE provider = 'claude'
      AND succeeded = 1
      AND session_identifier IS NOT NULL
      AND occurred_at_ms >= :start_ms
      AND occurred_at_ms < :end_ms
);

-- Sessions with 10 or more successful requests, and how many of them used
-- more than one account.
SELECT
    COUNT(*) AS sessions_10_plus,
    SUM(accounts > 1) AS multi_account,
    printf('%.3f', CAST(SUM(accounts > 1) AS REAL) / NULLIF(COUNT(*), 0)) AS multi_account_share
FROM (
    SELECT session_identifier, COUNT(*) AS requests, COUNT(DISTINCT account_identifier) AS accounts
    FROM usage_events
    WHERE provider = 'claude'
      AND succeeded = 1
      AND session_identifier IS NOT NULL
      AND occurred_at_ms >= :start_ms
      AND occurred_at_ms < :end_ms
    GROUP BY session_identifier
    HAVING COUNT(*) >= 10
);
