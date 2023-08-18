
CREATE TABLE speedtests_migration (
       pubkey text NOT NULL,
       upload_speed bigint,
       download_speed bigint,
       latency integer,
       "serial" text,
       timestamp timestamptz NOT NULL,
       inserted_at timestamptz default now(),
       PRIMARY KEY(pubkey, timestamp)
);
CREATE INDEX idx_speedtests2_pubkey on speedtests_migration (pubkey);

INSERT INTO speedtests_migration (pubkey, upload_speed, download_speed, latency, serial, timestamp)
SELECT id, (st).upload_speed, (st).download_speed, (st).latency, '', (st).timestamp
FROM (select id, unnest(speedtests) as st from speedtests_old) as tmp
ON CONFLICT DO NOTHING;

ALTER TABLE speedtests RENAME TO speedtests_old;
ALTER TABLE speedtests_migration RENAME TO speedtests;


