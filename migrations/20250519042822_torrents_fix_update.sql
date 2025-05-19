-- Fix for updated_at column and trigger issues

-- Re-ensure the function exists and is up-to-date
CREATE OR REPLACE FUNCTION update_updated_at_column()
RETURNS TRIGGER AS $$
BEGIN
    NEW.updated_at = CURRENT_TIMESTAMP;
    RETURN NEW;
END;
$$ language 'plpgsql';

-- Add the updated_at column to the torrents table if it doesn't exist.
-- This is the primary fix for the "record new has no field updated_at" error
-- if the column was missing when the trigger was initially created.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'torrents') THEN
        IF NOT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 'torrents' AND column_name = 'updated_at') THEN
            ALTER TABLE torrents ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP;
        ELSE
            -- If the column exists, ensure its default is set, though usually not necessary if already NOT NULL
            ALTER TABLE torrents ALTER COLUMN updated_at SET DEFAULT CURRENT_TIMESTAMP;
        END IF;
    END IF;
END $$;

-- Drop the old trigger if it exists, then recreate it.
-- This ensures it correctly references the (now guaranteed) updated_at column.
DROP TRIGGER IF EXISTS update_torrents_updated_at ON torrents;
CREATE TRIGGER update_torrents_updated_at
BEFORE UPDATE ON torrents
FOR EACH ROW
EXECUTE FUNCTION update_updated_at_column();
