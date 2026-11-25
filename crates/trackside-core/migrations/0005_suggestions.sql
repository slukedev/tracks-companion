-- Faster suggestions (architecture section 5, Suggestions and enrichment) and cover art for
-- suggested songs the server does not have.

-- A user's newest listen, to tell whether their cached recent listens are still current.
CREATE INDEX listens_user_id ON listens(user_id, id);
-- Co-listening around an artist's songs, and the songs a Last.fm row points at.
CREATE INDEX similar_songs_target ON similar_songs(similar_song_key);

-- Cover art for a suggested song, by song key: the image's id in the artwork cache, or NULL
-- when Last.fm has none. Looked up again after expires_at.
CREATE TABLE external_art (
    song_key    TEXT PRIMARY KEY,
    art_id      TEXT,
    fetched_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
) STRICT, WITHOUT ROWID;
CREATE INDEX external_art_id ON external_art(art_id) WHERE art_id IS NOT NULL;
