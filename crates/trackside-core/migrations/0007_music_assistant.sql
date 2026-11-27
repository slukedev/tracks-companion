-- Music Assistant: speakers and streaming services (ADR 0013).
--
-- Two user permissions, also carried by invites; both default on, so existing users keep
-- what a new user gets.
ALTER TABLE users ADD COLUMN can_use_speakers INTEGER NOT NULL DEFAULT 1
    CHECK (can_use_speakers IN (0, 1));
ALTER TABLE users ADD COLUMN can_play_services INTEGER NOT NULL DEFAULT 1
    CHECK (can_play_services IN (0, 1));
-- A service account Trackside made for an integration (`music_assistant`): no password, no
-- sign-in, keys valid on /rest only.
ALTER TABLE users ADD COLUMN service TEXT;
ALTER TABLE invites ADD COLUMN can_use_speakers INTEGER NOT NULL DEFAULT 1
    CHECK (can_use_speakers IN (0, 1));
ALTER TABLE invites ADD COLUMN can_play_services INTEGER NOT NULL DEFAULT 1
    CHECK (can_play_services IN (0, 1));
-- The integration that made and rotates a key (`music_assistant`).
ALTER TABLE api_keys ADD COLUMN managed_by TEXT;
CREATE UNIQUE INDEX users_service ON users(service) WHERE service IS NOT NULL;
